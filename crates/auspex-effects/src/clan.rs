//! Which `clan` binary to run, and running it.
//!
//! auspex never reimplements what the `clan` CLI does (deploys, vars), but `clan` is not on
//! `PATH` in a plain shell, and `clan --version` is not supported by the clan-core in use,
//! so "which one is this?" cannot be asked of a binary. The answer has to come from the
//! fleet: its `flake.lock` pins a `clan-core` input, and that input's
//! `packages.<system>.clan-cli` is the binary that matches the fleet's clan-core revision.
//!
//! # Resolution, in this order and nothing else
//!
//! 1. **An explicit path from configuration**, taken as is once it is an absolute path to
//!    an executable regular file. Nothing else is consulted, and no nix runs.
//! 2. **The clan-core input pinned in the fleet's lock.** Two nix calls, both through the
//!    process supervisor with a timeout and cancellation:
//!    - `nix flake metadata --json --no-write-lock-file --no-update-lock-file <flake>`
//!      reads the lock, and the node behind the root input `clan-core` gives the revision
//!      (`locked.rev`, or `locked.narHash` for an input with no revision, such as a path or
//!      a tarball). `--no-update-lock-file` makes a lock that does not match `flake.nix` an
//!      error instead of a silent re-lock: the point is to run what the lock says.
//!    - `nix build --no-link --print-out-paths --inputs-from <flake>
//!      clan-core#packages.<system>.clan-cli` builds it. `--inputs-from` resolves the
//!      indirect reference `clan-core` through *the fleet's own lock*, with the lock's
//!      `narHash`, so a newer upstream commit can never be built in its place.
//!
//!    The binary is `<out>/bin/clan`.
//! 3. **Nothing.** `PATH` is never a fallback. A `clan` on `PATH` is compared with the
//!    built one by canonical path (a store path names its contents), and the verdict is
//!    recorded as [`PathClan`]: [`PathClan::Same`], or [`PathClan::Different`] with the
//!    message "clan on PATH is a different clan-core than the fleet pins". Either way the
//!    binary used is the fleet's.
//!
//! **Why `nix flake metadata` and not parsing `flake.lock`.** The lock file on disk is only
//! the fleet's lock for a local checkout, and for a git checkout nix reads the *committed*
//! one, not the working tree's. The metadata call asks nix, which is also what the build
//! then uses, so the revision shown is by construction the revision built. It costs one
//! process and is not an evaluation of the fleet's outputs: the fleet's dev shell is never
//! evaluated, and nothing here builds or evaluates a machine.
//!
//! **The lock file on disk is checked too**, for a local fleet: `--no-update-lock-file`
//! stops nix changing an entry, but not locking a *new* input in memory to whatever its URL
//! points at today. So the revision nix reports must equal the one in the fleet's own
//! `flake.lock`, or resolution fails saying the lock is out of date (`check_lock_file`).
//!
//! Every step is recorded in [`ClanBinary::trail`], so the operator can see why this
//! binary and not another. The result is cached per `(flake, lock hash)` for the life of a
//! [`ClanLocator`], which the shell keeps for the whole run: the lock is read each time
//! (it is what the key is made of), the build only when the lock changed.
//!
//! # Running it
//!
//! [`run`] runs `clan <args> --flake <flake>` in the fleet's directory, in a session of its
//! own with no terminal, and streams each stdout and stderr line to a channel as it is
//! printed. Cancelling kills the whole process group (ssh and nixos-rebuild included) and
//! reaps it before returning. Output is capped without ever stopping the process: a deploy
//! killed because it was chatty would be worse than a log that stops showing lines. A line
//! longer than [`RunParams::line_cap`] is replaced by a note, and lines past
//! [`RunParams::max_lines`] are counted but not sent.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Mutex;
use std::time::Duration;

use auspex_domain::text::sanitize_line;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncRead;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::nix::{DEFAULT_NIX, FlakeRef, STDERR_CAP};
use crate::process::{
    End, Limits, Line, Lines, OnExit, PIPE_GRACE, command, describe_status, resolve_in,
    run as run_process, spawn, spawn_error,
};
use crate::text::{lossy, reason};

/// The root input of the fleet's lock that holds Clan.
pub const CLAN_CORE_INPUT: &str = "clan-core";
/// The package of that input that is the CLI.
pub const CLAN_CLI_PACKAGE: &str = "clan-cli";
/// The program name looked for on `PATH`, only to compare it with the pinned one.
pub const CLAN_PROGRAM: &str = "clan";

/// How much of `nix flake metadata`'s stdout is read: 16 MiB. A lock is kilobytes.
const METADATA_STDOUT_CAP: usize = 16 * 1024 * 1024;
/// `nix build --print-out-paths` prints one path per output.
const BUILD_STDOUT_CAP: usize = 64 * 1024;
/// Every store path starts with this. The check on what `nix build` printed, not a claim
/// about where a store may live.
const STORE_PREFIX: &str = "/nix/store/";

/// The step of resolving or running `clan` that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClanStep {
    /// The explicit path from configuration.
    Config,
    /// Reading the fleet's lock (`nix flake metadata`).
    Lock,
    /// Building the pinned `clan-cli` (`nix build`).
    Build,
    /// Finding the binary in what the build produced.
    Binary,
    /// Running `clan`.
    Run,
}

impl fmt::Display for ClanStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Config => "configured clan path",
            Self::Lock => "reading the fleet lock",
            Self::Build => "building the pinned clan-cli",
            Self::Binary => "locating the clan binary",
            Self::Run => "running clan",
        })
    }
}

/// Why `clan` could not be resolved or run, naming the step. A cancellation is not an error:
/// it is [`Cancelled`], as everywhere in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClanError {
    /// Where it went wrong.
    pub step: ClanStep,
    /// What went wrong, with the process's sanitised stderr tail where there is one.
    pub reason: String,
}

impl ClanError {
    fn new(step: ClanStep, reason: impl Into<String>) -> Self {
        Self {
            step,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ClanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.step, self.reason)
    }
}

impl std::error::Error for ClanError {}

/// Where the binary came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClanSource {
    /// The path the operator configured.
    Config,
    /// Built from the clan-core input pinned in the fleet's lock.
    FleetLock {
        /// The input's `rev` (or its `narHash` when it has none), in full.
        revision: String,
    },
}

impl fmt::Display for ClanSource {
    /// `config`, or `fleet lock <rev>` with a 40-digit hex revision cut to 7 digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => f.write_str("config"),
            Self::FleetLock { revision } => write!(f, "fleet lock {}", short(revision)),
        }
    }
}

fn short(revision: &str) -> &str {
    if revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        &revision[..7]
    } else {
        revision
    }
}

/// What a `clan` on `PATH` is, next to the one the fleet pins. Never a candidate to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathClan {
    /// Not looked for: an explicit configured path was used.
    NotChecked,
    /// There is no `clan` on `PATH`.
    Absent,
    /// There is, and it is the same store path as the pinned one.
    Same,
    /// There is, and it is not: the message to show, which says so.
    Different(String),
}

/// A resolved `clan` binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClanBinary {
    /// The absolute path to run: a store path for [`ClanSource::FleetLock`].
    pub path: PathBuf,
    /// Why this one.
    pub source: ClanSource,
    /// The clan-core revision it was built from; `None` for a configured path, which says
    /// nothing about one.
    pub revision: Option<String>,
    /// `clan` on `PATH`, for display only.
    pub path_clan: PathClan,
    /// Every step taken, in order, one line each: what was tried and what it gave.
    pub trail: Vec<String>,
}

/// What [`ClanLocator::resolve`] needs.
#[derive(Debug, Clone)]
pub struct ResolveParams<'a> {
    /// The `nix` program: [`DEFAULT_NIX`] (from `PATH`) or a path.
    pub nix: &'a OsStr,
    /// The configured path to `clan`, if any. Wins over everything.
    pub config_path: Option<&'a Path>,
    /// The nix system whose `clan-cli` is built; `None` for this machine's
    /// ([`current_system`]).
    pub system: Option<&'a str>,
    /// The `PATH` to look for `clan` in, for the comparison only; `None` for the process's.
    pub search_path: Option<&'a OsStr>,
    /// `nix flake metadata`'s deadline.
    pub lock_timeout: Duration,
    /// `nix build`'s deadline. Building is the long part when the binary is not cached.
    pub build_timeout: Duration,
}

impl ResolveParams<'_> {
    /// `nix` from `PATH`, no configured path, this machine's system, 2 minutes to read the
    /// lock and 30 to build.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nix: OsStr::new(DEFAULT_NIX),
            config_path: None,
            system: None,
            search_path: None,
            lock_timeout: Duration::from_secs(120),
            build_timeout: Duration::from_secs(30 * 60),
        }
    }
}

impl Default for ResolveParams<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// This machine's nix system (`x86_64-linux`, `aarch64-darwin`), from how auspex was built.
#[must_use]
pub fn current_system() -> String {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

/// The root `clan-core` input of a lock, as found by [`clan_core_lock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedClanCore {
    /// `locked.rev`, or `locked.narHash` when the input has no revision.
    pub revision: String,
    /// A digest of the whole lock (`locks` of the metadata), so any change to any input
    /// changes it.
    pub lock_hash: String,
}

/// The `clan-core` input of the fleet's lock, from the output of `nix flake metadata --json`.
///
/// Follows `follows` (a root input that is a path through other inputs) to the node that is
/// actually locked.
///
/// # Errors
///
/// What is wrong with the lock, naming the input: not JSON, no `locks`, no input named
/// `clan-core`, a `follows` that leads nowhere, or a node with nothing locked.
pub fn clan_core_lock(metadata_json: &str) -> Result<LockedClanCore, String> {
    let meta: Value = serde_json::from_str(metadata_json)
        .map_err(|e| format!("nix flake metadata printed something that is not JSON: {e}"))?;
    let locks = meta
        .get("locks")
        .ok_or_else(|| "nix flake metadata printed no `locks`".to_owned())?;
    let revision = locked_revision(locks)?;
    let mut hash = Sha256::new();
    hash.update(locks.to_string().as_bytes());
    Ok(LockedClanCore {
        revision,
        lock_hash: hex(&hash.finalize()),
    })
}

/// The revision of the root `clan-core` input of a lock (`locks` of the metadata, or the
/// whole of a `flake.lock`, which is the same shape).
fn locked_revision(locks: &Value) -> Result<String, String> {
    let nodes = locks
        .get("nodes")
        .and_then(Value::as_object)
        .ok_or_else(|| "the lock has no `nodes`".to_owned())?;
    let root = locks.get("root").and_then(Value::as_str).unwrap_or("root");
    let missing = || {
        format!(
            "the fleet's flake.lock has no input named {CLAN_CORE_INPUT}: \
             add it to the fleet flake's inputs and lock it"
        )
    };
    let entry = nodes
        .get(root)
        .and_then(|r| r.get("inputs"))
        .and_then(|i| i.get(CLAN_CORE_INPUT))
        .ok_or_else(missing)?;
    let node = follow(nodes, root, entry, 0)?;
    let locked = nodes
        .get(&node)
        .and_then(|n| n.get("locked"))
        .ok_or_else(|| format!("the lock's {CLAN_CORE_INPUT} node ({node}) has nothing locked"))?;
    let revision = ["rev", "narHash"]
        .iter()
        .find_map(|k| locked.get(*k).and_then(Value::as_str))
        .ok_or_else(|| format!("the locked {CLAN_CORE_INPUT} has neither a rev nor a narHash"))?;
    Ok(sanitize_line(revision))
}

/// Checks that the `flake.lock` file in `dir` pins `clan-core` at `revision`, the one nix
/// reported.
///
/// Why this exists: `nix flake metadata --no-update-lock-file` refuses to *change* an entry
/// of the lock, but an input the lock does not have yet is locked in memory, to whatever its
/// URL points at today, with only a warning ("not writing modified lock file"). Taken at its
/// word, a fleet whose `flake.lock` lacks `clan-core` (or predates it) would get the newest
/// clan-cli, which is exactly what pinning is meant to prevent. The file on disk is what the
/// operator committed; if nix saw something else, the lock is out of date and is said to be.
///
/// # Errors
///
/// Why the file does not agree: missing, unreadable, not a lock, no `clan-core`, or another
/// revision.
async fn check_lock_file(dir: &Path, revision: &str) -> Result<(), String> {
    let file = dir.join("flake.lock");
    let stale = "run `nix flake lock` in the fleet and commit flake.lock";
    let meta = tokio::fs::metadata(&file).await.map_err(|e| {
        format!(
            "the fleet has no usable flake.lock ({}: {e}); {stale}",
            file.display()
        )
    })?;
    if meta.len() > METADATA_STDOUT_CAP as u64 {
        return Err(format!("{} is implausibly large", file.display()));
    }
    let text = tokio::fs::read_to_string(&file)
        .await
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let locks: Value =
        serde_json::from_str(&text).map_err(|e| format!("{} is not JSON: {e}", file.display()))?;
    match locked_revision(&locks) {
        Ok(on_disk) if on_disk == revision => Ok(()),
        Ok(on_disk) => Err(format!(
            "flake.lock pins {CLAN_CORE_INPUT} at {on_disk}, but nix resolves {revision}: \
             the lock is out of date; {stale}"
        )),
        Err(e) => Err(format!(
            "{e}; nix would lock it afresh to {revision}, which is not a pin"
        )),
    }
}

/// The node name an input entry leads to. An entry is a node name, or a list of input names
/// to walk from the root (`follows`), where each step may itself be either.
fn follow(
    nodes: &serde_json::Map<String, Value>,
    root: &str,
    entry: &Value,
    depth: usize,
) -> Result<String, String> {
    // A lock is a graph; a `follows` loop would otherwise never end.
    if depth > 32 {
        return Err(format!("{CLAN_CORE_INPUT} follows a loop in the lock"));
    }
    match entry {
        Value::String(name) => Ok(name.clone()),
        Value::Array(path) => {
            let mut at = root.to_owned();
            for step in path {
                let name = step
                    .as_str()
                    .ok_or_else(|| format!("{CLAN_CORE_INPUT} follows a malformed path"))?;
                let next = nodes
                    .get(&at)
                    .and_then(|n| n.get("inputs"))
                    .and_then(|i| i.get(name))
                    .ok_or_else(|| {
                        format!("{CLAN_CORE_INPUT} follows {name:?}, which the lock does not have")
                    })?;
                at = follow(nodes, root, next, depth + 1)?;
            }
            Ok(at)
        }
        _ => Err(format!(
            "{CLAN_CORE_INPUT} is a malformed entry in the lock"
        )),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Checks that `path` is an absolute path to an executable regular file (symlinks followed,
/// as `exec` does).
///
/// # Errors
///
/// Why it is not.
pub fn check_executable(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "{} is not an absolute path; give the full path to clan",
            path.display()
        ));
    }
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(())
}

/// `clan` on `PATH`, as [`PathClan`], next to the pinned binary. By canonical path: a store
/// path names its contents, so equal canonical paths are the same clan-core, and a profile's
/// `bin/clan` symlink is seen through.
fn compare_path_clan(search_path: Option<&OsStr>, pinned: &Path) -> PathClan {
    let from_env = std::env::var_os("PATH");
    let path = search_path.or(from_env.as_deref());
    let Ok(found) = resolve_in(OsStr::new(CLAN_PROGRAM), path) else {
        return PathClan::Absent;
    };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(&found) == canon(pinned) {
        PathClan::Same
    } else {
        PathClan::Different(format!(
            "clan on PATH is a different clan-core than the fleet pins ({} is not {}); \
             auspex uses the pinned one",
            found.display(),
            pinned.display()
        ))
    }
}

/// Resolves `clan` for a fleet, and remembers the answer.
///
/// Keep one for the whole run. The cache is per `(flake, lock hash)`, so a fleet whose lock
/// is updated meanwhile is resolved afresh, and the old entry is simply never asked for.
#[derive(Debug, Default)]
pub struct ClanLocator {
    cache: Mutex<HashMap<(String, String), ClanBinary>>,
}

impl ClanLocator {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves the `clan` binary for `flake`; see the module documentation for the order.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired; any `nix` process has been killed and reaped.
    /// Everything else is the inner [`ClanError`], naming the step.
    pub async fn resolve(
        &self,
        params: &ResolveParams<'_>,
        flake: &FlakeRef,
        cancel: &CancellationToken,
    ) -> Result<Result<ClanBinary, ClanError>, Cancelled> {
        if let Some(path) = params.config_path {
            return Ok(from_config(path));
        }
        let system = params.system.map_or_else(current_system, str::to_owned);
        if let Some(e) = bad_system(&system) {
            return Ok(Err(e));
        }

        let mut trail = vec!["no explicit clan path configured".to_owned()];
        let metadata = match nix_step(
            params.nix,
            ClanStep::Lock,
            metadata_argv(flake),
            METADATA_STDOUT_CAP,
            params.lock_timeout,
            cancel,
        )
        .await?
        {
            Ok(out) => out,
            Err(e) => return Ok(Err(e)),
        };
        let locked = match clan_core_lock(&metadata) {
            Ok(l) => l,
            Err(e) => return Ok(Err(ClanError::new(ClanStep::Lock, e))),
        };
        trail.push(format!(
            "fleet lock {}: {CLAN_CORE_INPUT} pinned at {} (lock {})",
            flake.as_str(),
            locked.revision,
            &locked.lock_hash[..12]
        ));

        // A local flake's lock file must say the same; see `check_lock_file`. A remote one
        // (`github:…`) has no file to look at: nix read the committed lock of that revision.
        if let Some(dir) = flake.local_dir() {
            if let Err(e) = check_lock_file(&dir, &locked.revision).await {
                return Ok(Err(ClanError::new(ClanStep::Lock, e)));
            }
            trail.push("flake.lock on disk agrees".to_owned());
        } else {
            trail.push(
                "flake.lock not checked on disk: the flake is not a local directory".to_owned(),
            );
        }

        let key = (flake.as_str().to_owned(), locked.lock_hash.clone());
        let cached = self
            .cache
            .lock()
            .ok()
            .and_then(|c| c.get(&key).cloned())
            // A garbage-collected build is rebuilt, not trusted.
            .filter(|b| check_executable(&b.path).is_ok());
        let mut found = if let Some(mut b) = cached {
            trail.push(format!("already built for this lock: {}", b.path.display()));
            b.trail = trail;
            b
        } else {
            let out = match nix_step(
                params.nix,
                ClanStep::Build,
                build_argv(flake, &system),
                BUILD_STDOUT_CAP,
                params.build_timeout,
                cancel,
            )
            .await?
            {
                Ok(out) => out,
                Err(e) => return Ok(Err(e)),
            };
            let path = match binary_in(&out) {
                Ok(p) => p,
                Err(e) => return Ok(Err(e)),
            };
            trail.push(format!(
                "nix build packages.{system}.{CLAN_CLI_PACKAGE} of the locked input: {}",
                path.display()
            ));
            let b = ClanBinary {
                path,
                source: ClanSource::FleetLock {
                    revision: locked.revision.clone(),
                },
                revision: Some(locked.revision),
                path_clan: PathClan::NotChecked,
                trail,
            };
            if let Ok(mut c) = self.cache.lock() {
                c.insert(key, b.clone());
            }
            b
        };
        // Not cached: `PATH` may change while auspex runs, and this is a comparison only.
        found.path_clan = compare_path_clan(params.search_path, &found.path);
        found.trail.push(match &found.path_clan {
            PathClan::Absent => "no clan on PATH (not used in any case)".to_owned(),
            PathClan::Same => "clan on PATH is the same store path".to_owned(),
            PathClan::Different(why) => why.clone(),
            PathClan::NotChecked => unreachable!("set just above"),
        });
        Ok(Ok(found))
    }
}

/// Refuses a system that is not a nix system name. It becomes part of an attribute path, one
/// argv element and never Nix source; but a `.` or a quote in it would still change which
/// attribute that is.
fn bad_system(system: &str) -> Option<ClanError> {
    let ok = !system.is_empty()
        && system
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    (!ok).then(|| {
        ClanError::new(
            ClanStep::Build,
            format!("{system:?} is not a nix system name such as x86_64-linux"),
        )
    })
}

fn from_config(path: &Path) -> Result<ClanBinary, ClanError> {
    check_executable(path).map_err(|e| ClanError::new(ClanStep::Config, e))?;
    Ok(ClanBinary {
        path: path.to_path_buf(),
        source: ClanSource::Config,
        revision: None,
        path_clan: PathClan::NotChecked,
        trail: vec![format!(
            "explicit path from configuration: {} (an executable file); the fleet lock and PATH were not consulted",
            path.display()
        )],
    })
}

/// The exact argv (after the program) of the lock read.
#[must_use]
pub fn metadata_argv(flake: &FlakeRef) -> Vec<OsString> {
    [
        "flake",
        "metadata",
        "--json",
        "--no-write-lock-file",
        "--no-update-lock-file",
        "--",
        &flake.shallow_safe(),
    ]
    .map(OsString::from)
    .to_vec()
}

/// The exact argv (after the program) of the build.
#[must_use]
pub fn build_argv(flake: &FlakeRef, system: &str) -> Vec<OsString> {
    let attr = format!("{CLAN_CORE_INPUT}#packages.{system}.{CLAN_CLI_PACKAGE}");
    [
        "build",
        "--no-link",
        "--print-out-paths",
        "--no-write-lock-file",
        "--no-update-lock-file",
        "--inputs-from",
        &flake.shallow_safe(),
        "--",
        &attr,
    ]
    .map(OsString::from)
    .to_vec()
}

/// `bin/clan` of the first printed output that has one.
fn binary_in(printed: &str) -> Result<PathBuf, ClanError> {
    let outs: Vec<&str> = printed.lines().filter(|l| !l.is_empty()).collect();
    if outs.is_empty() {
        return Err(ClanError::new(
            ClanStep::Binary,
            "nix build printed no output path",
        ));
    }
    let mut last = String::new();
    for out in &outs {
        if !out.starts_with(STORE_PREFIX) {
            return Err(ClanError::new(
                ClanStep::Binary,
                format!(
                    "nix build printed {:?}, which is not a store path",
                    sanitize_line(out)
                ),
            ));
        }
        let candidate = Path::new(out).join("bin").join(CLAN_PROGRAM);
        match check_executable(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) => last = e,
        }
    }
    Err(ClanError::new(
        ClanStep::Binary,
        format!("the built {CLAN_CLI_PACKAGE} has no usable bin/{CLAN_PROGRAM}: {last}"),
    ))
}

/// Runs `nix <argv>` to completion, and returns its stdout.
async fn nix_step(
    nix: &OsStr,
    step: ClanStep,
    argv: Vec<OsString>,
    stdout_cap: usize,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<String, ClanError>, Cancelled> {
    let what = match step {
        ClanStep::Lock => "nix flake metadata",
        _ => "nix build",
    };
    let mut cmd = match command(nix) {
        Ok(c) => c,
        Err(e) => return Ok(Err(ClanError::new(step, spawn_error(what, nix, &e)))),
    };
    cmd.args(argv);
    let limits = Limits {
        stdout: stdout_cap,
        stderr: STDERR_CAP,
        memory: None,
    };
    let deadline = Instant::now() + timeout;
    let ran = match run_process(cmd, None, OnExit::KillGroup, limits, deadline, cancel).await {
        Ok(r) => r,
        Err(e) => return Ok(Err(ClanError::new(step, spawn_error(what, nix, &e)))),
    };
    let stderr = lossy(&ran.stderr);
    let with_stderr = |what: String| match reason(&stderr) {
        Some(r) => format!("{what}: {r}"),
        None => what,
    };
    match ran.end {
        End::Cancelled => Err(Cancelled),
        End::TimedOut => Ok(Err(ClanError::new(
            step,
            with_stderr(format!("{what} timed out after {timeout:?}")),
        ))),
        End::Overflow => Ok(Err(ClanError::new(
            step,
            format!("{what} printed more than {stdout_cap} bytes"),
        ))),
        End::Exited(s) if !s.success() => Ok(Err(ClanError::new(
            step,
            with_stderr(format!("{what} {}", describe_status(Some(s)))),
        ))),
        End::Exited(_) => Ok(ran.whole_stdout().map(lossy).map_err(|_| {
            ClanError::new(
                step,
                format!("{what} exited, but its output was not read to the end"),
            )
        })),
    }
}

/// Which stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One line of `clan`'s output, sanitised (no terminal escapes, no control characters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClanLine {
    /// Where it was printed.
    pub stream: Stream,
    /// The line, without its newline.
    pub text: String,
}

/// [`run`]'s settings.
#[derive(Debug, Clone, Copy)]
pub struct RunParams {
    /// The whole run's deadline. Past it the process group is killed.
    pub timeout: Duration,
    /// The longest line shown, in bytes; a longer one is replaced by a note.
    pub line_cap: usize,
    /// The most lines sent; the rest are counted in [`ClanRun::lines_dropped`].
    pub max_lines: u64,
}

impl RunParams {
    /// 2 hours, 64 KiB per line, 200 000 lines.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            timeout: Duration::from_secs(2 * 60 * 60),
            line_cap: 64 * 1024,
            max_lines: 200_000,
        }
    }
}

impl Default for RunParams {
    fn default() -> Self {
        Self::new()
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    /// The process exited: with a code, or killed by a signal.
    Exited {
        /// Its exit code, if it exited.
        code: Option<i32>,
        /// The signal that killed it, if one did.
        signal: Option<i32>,
    },
    /// The deadline passed; the process group was killed.
    TimedOut,
}

impl RunEnd {
    /// Exited with status 0.
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self, Self::Exited { code: Some(0), .. })
    }
}

/// What a run did, apart from the lines it sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClanRun {
    /// How it ended.
    pub end: RunEnd,
    /// Lines past [`RunParams::max_lines`], counted and not sent.
    pub lines_dropped: u64,
    /// Lines longer than [`RunParams::line_cap`], replaced by a note.
    pub long_lines: u64,
}

fn run_end(status: ExitStatus) -> RunEnd {
    use std::os::unix::process::ExitStatusExt as _;
    RunEnd::Exited {
        code: status.code(),
        signal: status.signal(),
    }
}

/// The exact argv (after the program) of a run: the caller's arguments, then the flake.
///
/// `--flake` comes last because it is an option of the subcommand (`clan machines update
/// <m> --flake <dir>`), and the flake is the directory when it has one: `clan` takes a path,
/// and the normalised `git+file://` reference [`FlakeRef`] gives nix is for nix.
#[must_use]
pub fn run_argv(flake: &FlakeRef, args: &[OsString]) -> Vec<OsString> {
    let mut full = args.to_vec();
    full.push(OsString::from("--flake"));
    full.push(
        flake
            .local_dir()
            .map_or_else(|| OsString::from(flake.as_str()), PathBuf::into_os_string),
    );
    full
}

/// Runs `<program> <args> --flake <flake>` in the flake's directory, streaming its lines.
///
/// The program is `ClanBinary::path` (tests give a fake). It runs in a session of its own
/// with no terminal and no stdin, and is never run through a shell. Each stdout and stderr
/// line is sent on `tx` when it is printed, in order within its stream. When it exits, the
/// rest of its group is killed (`ssh`'s control master, `nixos-rebuild` helpers) and what is
/// still in the pipes is read.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired or `tx`'s receiver was dropped: the whole group has
/// been killed and reaped. A program that cannot be started is the inner [`ClanError`].
pub async fn run(
    program: &Path,
    flake: &FlakeRef,
    args: &[OsString],
    params: &RunParams,
    cancel: &CancellationToken,
    tx: UnboundedSender<ClanLine>,
) -> Result<Result<ClanRun, ClanError>, Cancelled> {
    let mut cmd = match command(program.as_os_str()) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(ClanError::new(
                ClanStep::Run,
                spawn_error("cannot run clan", program.as_os_str(), &e),
            )));
        }
    };
    cmd.args(run_argv(flake, args));
    if let Some(dir) = flake.local_dir() {
        cmd.current_dir(dir);
    }
    let mut sp = match spawn(cmd, false, OnExit::KillGroup, None) {
        Ok(s) => s,
        Err(e) => {
            return Ok(Err(ClanError::new(
                ClanStep::Run,
                spawn_error("cannot run clan", program.as_os_str(), &e),
            )));
        }
    };
    let mut out = sp
        .child
        .stdout
        .take()
        .map(|o| Lines::new(o, params.line_cap));
    let mut err = sp
        .child
        .stderr
        .take()
        .map(|o| Lines::new(o, params.line_cap));
    let mut sink = Sink {
        tx: &tx,
        max_lines: params.max_lines,
        line_cap: params.line_cap,
        sent: 0,
        dropped: 0,
        long: 0,
    };
    let deadline = Instant::now() + params.timeout;
    let mut exited: Option<ExitStatus> = None;
    let mut grace: Option<Instant> = None;
    let mut timed_out = false;
    loop {
        let out_done = out.as_ref().is_none_or(|l| l.done);
        let err_done = err.as_ref().is_none_or(|l| l.done);
        if exited.is_some() && out_done && err_done {
            break;
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                sp.kill_and_reap().await;
                return Err(Cancelled);
            }
            // Nobody is listening any more: the same as being told to stop.
            () = tx.closed() => {
                sp.kill_and_reap().await;
                return Err(Cancelled);
            }
            () = sleep_until(deadline), if exited.is_none() => {
                sp.kill_and_reap().await;
                timed_out = true;
                break;
            }
            status = sp.exited(), if exited.is_none() => match status {
                Ok(s) => {
                    exited = Some(s);
                    grace = Some(Instant::now() + PIPE_GRACE);
                }
                Err(e) => {
                    sp.kill_and_reap().await;
                    return Ok(Err(ClanError::new(ClanStep::Run, format!("waiting for clan failed: {e}"))));
                }
            },
            () = sleep_until(grace.unwrap_or(deadline)), if grace.is_some() => break,
            line = next_line(&mut out), if !out_done => sink.line(Stream::Stdout, line),
            line = next_line(&mut err), if !err_done => sink.line(Stream::Stderr, line),
        }
    }
    // After a kill, what was printed just before it is still in the pipes.
    if timed_out {
        drain(&mut out, &mut err, &mut sink).await;
    }
    let end = match exited {
        Some(s) if !timed_out => run_end(s),
        _ => RunEnd::TimedOut,
    };
    Ok(Ok(ClanRun {
        end,
        lines_dropped: sink.dropped,
        long_lines: sink.long,
    }))
}

/// Reads what is left in the pipes of a killed process, for at most [`PIPE_GRACE`].
async fn drain(
    out: &mut Option<Lines<tokio::process::ChildStdout>>,
    err: &mut Option<Lines<tokio::process::ChildStderr>>,
    sink: &mut Sink<'_>,
) {
    let until = Instant::now() + PIPE_GRACE;
    loop {
        let out_done = out.as_ref().is_none_or(|l| l.done);
        let err_done = err.as_ref().is_none_or(|l| l.done);
        if out_done && err_done {
            return;
        }
        tokio::select! {
            biased;
            () = sleep_until(until) => return,
            line = next_line(out), if !out_done => sink.line(Stream::Stdout, line),
            line = next_line(err), if !err_done => sink.line(Stream::Stderr, line),
        }
    }
}

async fn next_line<R: AsyncRead + Unpin>(lines: &mut Option<Lines<R>>) -> Option<Line> {
    match lines {
        Some(l) => l.next().await,
        None => None,
    }
}

/// Where lines go, with the caps applied.
struct Sink<'a> {
    tx: &'a UnboundedSender<ClanLine>,
    max_lines: u64,
    line_cap: usize,
    sent: u64,
    dropped: u64,
    long: u64,
}

impl Sink<'_> {
    fn line(&mut self, stream: Stream, line: Option<Line>) {
        let Some(line) = line else { return };
        let text = match line {
            Line::Full(bytes) => sanitize_line(&lossy(&bytes)),
            Line::TooLong => {
                self.long += 1;
                format!("[a line of more than {} bytes was left out]", self.line_cap)
            }
        };
        if self.sent >= self.max_lines {
            self.dropped += 1;
            return;
        }
        self.sent += 1;
        let _ = self.tx.send(ClanLine { stream, text });
        if self.sent == self.max_lines {
            let _ = self.tx.send(ClanLine {
                stream: Stream::Stderr,
                text: format!(
                    "[auspex: {} lines shown; further output is not shown]",
                    self.max_lines
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = "dbae2100000000000000000000000000000000aa";

    fn metadata(root_inputs: &str, nodes: &str) -> String {
        format!(
            r#"{{"locks":{{"nodes":{{{nodes},"root":{{"inputs":{{{root_inputs}}}}}}},"root":"root","version":7}}}}"#
        )
    }

    fn node(name: &str, locked: &str) -> String {
        format!(r#""{name}":{{"locked":{{{locked}}}}}"#)
    }

    #[test]
    fn the_pinned_revision_is_the_locked_rev() {
        let json = metadata(
            r#""clan-core":"clan-core""#,
            &node(
                "clan-core",
                &format!(r#""rev":"{REV}","narHash":"sha256-x","type":"github""#),
            ),
        );
        let l = clan_core_lock(&json).unwrap();
        assert_eq!(l.revision, REV);
        assert_eq!(l.lock_hash.len(), 64);
        assert_eq!(
            ClanSource::FleetLock {
                revision: l.revision
            }
            .to_string(),
            "fleet lock dbae210"
        );
        assert_eq!(ClanSource::Config.to_string(), "config");
    }

    #[test]
    fn an_input_without_a_rev_is_identified_by_its_nar_hash() {
        let json = metadata(
            r#""clan-core":"c""#,
            &node("c", r#""narHash":"sha256-abc","type":"path""#),
        );
        let l = clan_core_lock(&json).unwrap();
        assert_eq!(l.revision, "sha256-abc");
        assert_eq!(
            ClanSource::FleetLock {
                revision: l.revision
            }
            .to_string(),
            "fleet lock sha256-abc"
        );
    }

    #[test]
    fn a_lock_without_clan_core_names_the_input() {
        let json = metadata(r#""nixpkgs":"nixpkgs""#, &node("nixpkgs", r#""rev":"a""#));
        let e = clan_core_lock(&json).unwrap_err();
        assert!(e.contains("no input named clan-core"), "{e}");
        // Only the *root's* input counts: a transitive node of that name is not the fleet's.
        let json = metadata(
            r#""other":"other""#,
            &format!(
                "{},{}",
                node("other", r#""rev":"a""#),
                node("clan-core", r#""rev":"b""#)
            ),
        );
        assert!(clan_core_lock(&json).is_err());
    }

    #[test]
    fn follows_are_followed_and_loops_end() {
        // The root's clan-core follows other/clan-core.
        let nodes = format!(
            r#""other":{{"inputs":{{"clan-core":"deep"}}}},{}"#,
            node("deep", &format!(r#""rev":"{REV}""#))
        );
        let json = metadata(
            r#""other":"other","clan-core":["other","clan-core"]"#,
            &nodes,
        );
        assert_eq!(clan_core_lock(&json).unwrap().revision, REV);
        // A follows that points at itself must not hang.
        let json = metadata(r#""clan-core":["clan-core"]"#, "\"x\":{}");
        assert!(clan_core_lock(&json).is_err());
        // A path through something the lock does not have.
        let json = metadata(r#""clan-core":["nope","clan-core"]"#, "\"x\":{}");
        let e = clan_core_lock(&json).unwrap_err();
        assert!(e.contains("follows"), "{e}");
    }

    #[test]
    fn a_node_with_nothing_locked_is_refused() {
        let json = metadata(r#""clan-core":"c""#, r#""c":{}"#);
        assert!(
            clan_core_lock(&json)
                .unwrap_err()
                .contains("nothing locked")
        );
        assert!(clan_core_lock("not json").is_err());
        assert!(clan_core_lock("{}").is_err());
    }

    #[test]
    fn the_lock_hash_follows_the_whole_lock() {
        let a = metadata(
            r#""clan-core":"c""#,
            &format!(
                "{},{}",
                node("c", r#""rev":"1""#),
                node("n", r#""rev":"1""#)
            ),
        );
        let b = metadata(
            r#""clan-core":"c""#,
            &format!(
                "{},{}",
                node("c", r#""rev":"1""#),
                node("n", r#""rev":"2""#)
            ),
        );
        assert_ne!(
            clan_core_lock(&a).unwrap().lock_hash,
            clan_core_lock(&b).unwrap().lock_hash
        );
        assert_eq!(clan_core_lock(&a).unwrap(), clan_core_lock(&a).unwrap());
    }

    #[test]
    fn argv_is_data() {
        let f = FlakeRef::new("path:/fleet").unwrap();
        let m: Vec<_> = metadata_argv(&f)
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(m.last().map(String::as_str), Some("path:/fleet"));
        assert_eq!(m[m.len() - 2], "--");
        let b: Vec<_> = build_argv(&f, "x86_64-linux")
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(
            b.last().map(String::as_str),
            Some("clan-core#packages.x86_64-linux.clan-cli")
        );
        assert!(b.windows(2).any(|w| w == ["--inputs-from", "path:/fleet"]));
    }

    #[test]
    fn only_an_absolute_executable_regular_file_is_clan() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("clan");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_executable(&exe).is_ok());
        assert!(
            check_executable(&plain)
                .unwrap_err()
                .contains("not executable")
        );
        assert!(
            check_executable(dir.path())
                .unwrap_err()
                .contains("not a regular file")
        );
        assert!(check_executable(&dir.path().join("missing")).is_err());
        assert!(
            check_executable(Path::new("clan"))
                .unwrap_err()
                .contains("absolute")
        );
    }

    #[tokio::test]
    async fn the_lock_file_on_disk_must_pin_what_nix_reported() {
        let dir = tempfile::tempdir().unwrap();
        let lock = |rev: &str| {
            format!(
                r#"{{"nodes":{{"c":{{"locked":{{"rev":"{rev}"}}}},"root":{{"inputs":{{"clan-core":"c"}}}}}},"root":"root","version":7}}"#
            )
        };
        // No file at all.
        let e = check_lock_file(dir.path(), "a").await.unwrap_err();
        assert!(e.contains("no usable flake.lock"), "{e}");
        let file = dir.path().join("flake.lock");
        std::fs::write(&file, lock("a")).unwrap();
        check_lock_file(dir.path(), "a").await.unwrap();
        // The file pins one revision and nix resolved another: out of date.
        let e = check_lock_file(dir.path(), "b").await.unwrap_err();
        assert!(
            e.contains("pins clan-core at a") && e.contains("out of date"),
            "{e}"
        );
        // The file lacks the input: nix locked it afresh, which is not a pin.
        std::fs::write(&file, r#"{"nodes":{"root":{}},"root":"root","version":7}"#).unwrap();
        let e = check_lock_file(dir.path(), "b").await.unwrap_err();
        assert!(
            e.contains("no input named clan-core") && e.contains("not a pin"),
            "{e}"
        );
        std::fs::write(&file, "nope").unwrap();
        assert!(
            check_lock_file(dir.path(), "b")
                .await
                .unwrap_err()
                .contains("not JSON")
        );
    }

    #[test]
    fn the_local_flake_directory_is_what_clan_is_given() {
        let f = FlakeRef::new("path:/fleet").unwrap();
        assert_eq!(f.local_dir(), Some(PathBuf::from("/fleet")));
        let argv = run_argv(&f, &[OsString::from("machines"), OsString::from("update")]);
        assert_eq!(
            argv,
            ["machines", "update", "--flake", "/fleet"].map(OsString::from)
        );
        // A remote flake has no directory: the reference itself is passed.
        let r = FlakeRef::new("github:o/r").unwrap();
        assert_eq!(r.local_dir(), None);
        assert_eq!(run_argv(&r, &[]).last().unwrap(), "github:o/r");
    }
}
