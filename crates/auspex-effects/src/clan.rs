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
//!    - `nix flake metadata --json --no-update-lock-file <flake>` reads the lock. The node
//!      behind the root input `clan-core` gives the revision (`locked.rev`, or
//!      `locked.narHash` for an input with no revision) and its `locked` attributes, which
//!      must include a `narHash`. `--no-update-lock-file` *without* `--no-write-lock-file`:
//!      with both, nix only warns ("not writing modified lock file") and carries on with a
//!      lock it changed in memory; with this alone it refuses a lock that `flake.nix`
//!      disagrees with ("requires lock file changes but they're not allowed"), and writes
//!      nothing. **But only when every input of the fleet locks.** With any other input
//!      unlocked (a `git+file` input whose tree is dirty, say, common while an input is
//!      checked out for development) nix 2.35 skips that refusal, warns "not writing lock
//!      file … because it has an unlocked input", exits 0, and has relocked `clan-core` in
//!      memory to whatever `flake.nix` now names. `--allow-dirty-locks` would restore the
//!      refusal there; it is not passed (older nix may not know it). So the check of the lock
//!      file on disk below is **required**, not a second line of defence.
//!
//!      **A remote fleet.** Measured with a non-writable stand-in (a `tarball+file://` fleet,
//!      not a real `github:`): nix refuses a stale lock ("cannot write modified lock file …
//!      use '--no-write-lock-file' to ignore", exit 1), a missing lock and the unlocked-input
//!      case alike. The on-disk check does not apply to a remote fleet; the build consumes
//!      exactly the `locked` attributes this call returned, and nothing else looks at them.
//!    - `nix build --print-out-paths --out-link <root> --expr <BUILD_EXPR>` builds
//!      `packages.<system>.clan-cli` **of that locked node itself**: the expression is a
//!      constant that takes the node's `locked` attributes as a JSON string (`--argstr`),
//!      renders them with nix's own `builtins.flakeRefToString` into a fully locked flake
//!      reference (type, url, rev, `narHash`) and `builtins.getFlake`s it. There is no second
//!      look at the fleet's lock and no registry lookup: a `git pull` or `nix flake update`
//!      during a build that takes minutes changes nothing, and a user or global registry
//!      entry named `clan-core` is never consulted, because the reference is not indirect.
//!      nix checks what it fetched against what is locked: the `narHash` for a tarball,
//!      path or similar input, the commit `rev` for a git input. `builtins.flakeRefToString`
//!      is in nix 2.35; on a nix (or Lix) that lacks it the build fails with an undefined
//!      variable, loudly, never by building something else.
//!
//!    The binary is `<out>/bin/clan`, canonicalised, and the canonical path must be inside
//!    the nix store: a `bin/clan` that is a symlink out of it is an error, so the file that
//!    runs is the file that was locked. That canonical path is what is reported.
//!
//!    **Not the fleet's dev shell's closure, possibly.** The build uses clan-core's *own*
//!    `flake.lock` for clan-cli's dependencies (nixpkgs, python). A fleet that overrides
//!    them (`inputs.clan-core.inputs.nixpkgs.follows = "nixpkgs"`) gets a clan-cli in its dev
//!    shell that is the same revision with a different closure; such a fleet needs the
//!    configured path.
//! 3. **Nothing.** `PATH` is never a fallback. A `clan` on `PATH` is compared with the
//!    built one by canonical path (a store path names its contents), and the verdict is
//!    recorded as [`PathClan`]: [`PathClan::Same`], or [`PathClan::Different`] with the
//!    message "clan on PATH is a different clan-cli derivation than the fleet's locked
//!    clan-core builds". It says *derivation*, not clan-core: the revision may be the same
//!    with another closure (see above). Either way the binary used is the fleet's.
//!
//! **Why `nix flake metadata` and not parsing `flake.lock`.** The metadata call asks nix what
//! the lock is, for any kind of flake (a local checkout, a git reference, a remote one), and
//! nix is what builds, so no second implementation of how a lock is found and followed
//! (`follows`, a dirty git tree: nix reads the working tree's lock for tracked files, which
//! is also what the check below reads). It costs one process and is not an evaluation of the
//! fleet's outputs: the fleet's dev shell is never evaluated, and nothing here builds or
//! evaluates a machine.
//!
//! **The lock file on disk is checked too**, for a local fleet: the revision nix reports must
//! equal the one in the fleet's own `flake.lock`, or resolution fails saying the lock is out
//! of date (`check_lock_file`). Required (see above), and the only check for a lock with no
//! `clan-core` entry at all. For it to find the file the reference must name a directory: a
//! relative `path:` or `git+file:` reference (`path:./fleet`, `git+file:fleet`) is made
//! absolute by `FlakeRef::new`, as a plain path is, against the working directory at parse
//! time, and the resolution and [`run`] refuse any reference with a query (`?dir=`, `?ref=`,
//! any other, local or remote) and any local one with no directory they can find, with an
//! error that says so, rather than guess.
//!
//! Every step is recorded in [`ClanBinary::trail`], so the operator can see why this
//! binary and not another. The result is cached per `(flake, lock hash, system, nix program)`
//! for the life of a [`ClanLocator`], which the shell keeps for the whole run: the lock is read
//! each time (it is what the key is made of), the build only when the key is new or the build
//! has been garbage collected. Each build is rooted by a symlink in
//! `$XDG_STATE_HOME/auspex/gcroots/` (default `~/.local/state/auspex/gcroots/`, never inside
//! the fleet), named after a digest of the clan-core node and the system, which
//! `nix build --out-link` replaces atomically (it creates a temporary link and renames it),
//! so a garbage collection between [`ClanLocator::resolve`] and [`run`] cannot take the binary
//! away. A link per node, not per lock: a nixpkgs bump in the fleet changes the lock, not what
//! is built. After a successful build the links of earlier builds for the same system are
//! removed, so a clan-core that moves on does not keep every old closure rooted; only symlinks
//! in that directory named exactly as auspex names them are touched, nothing else. With no
//! usable state directory (neither `XDG_STATE_HOME` nor `HOME` an absolute path) the build
//! step fails, saying so, even though an unrooted build would have worked.
//!
//! # Running it
//!
//! [`run`] runs `clan <args> --flake <dir>` in the fleet's directory, in a session of its
//! own with no terminal and **stdin on `/dev/null`**, and streams each stdout and stderr
//! line to a channel as it is printed. Cancelling kills the whole process group (ssh and
//! nixos-rebuild included) and reaps it before returning. Once clan has exited, though, its
//! status is known, and a cancellation, a dropped receiver or the deadline only cuts short
//! the wait for the pipes to drain (or, for the deadline, is not a timeout): the status is
//! returned, because it is what gets logged. Output is capped without ever stopping the
//! process: a deploy killed because it was chatty would be worse than a log that stops
//! showing lines. A line longer than [`RunParams::line_cap`] is replaced by a note, and lines
//! past [`RunParams::max_lines`] are counted but not sent.
//!
//! **The environment policy is "no GUI prompts".** It follows the probe's (`ssh::probe`):
//! `SSH_ASKPASS` is removed and `SSH_ASKPASS_REQUIRE=never` is set, and the same goes for what
//! clan may start besides ssh: `GIT_ASKPASS` (git, for a `git+ssh` input) and `SUDO_ASKPASS`
//! (`sudo -A`) are removed. `DISPLAY` and `SSH_AUTH_SOCK` are inherited untouched: approval of
//! a key by an agent (the operator's Bitwarden SSH agent, say) goes through the agent socket,
//! and that is the one approval a deploy is meant to wait for. **A step of clan that prompts
//! on stdin therefore fails instead of hanging** (`clan vars` generators ask for secrets this
//! way), because stdin is `/dev/null`: step 2 must check the vars before it deploys, not
//! discover a missing one in the middle.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
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
    End, Limits, Line, Lines, OnExit, PIPE_GRACE, Spawned, command, describe_status, resolve_in,
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
/// The store directory the canonical binary must be inside.
const STORE_DIR: &str = "/nix/store";

/// What `nix build` evaluates.
///
/// A function of `locked` (the `locked` attributes of the lock's `clan-core` node, as a JSON
/// string) and `system`. `flakeRefToString` is nix's own rendering
/// of those attributes into a locked flake reference (with `narHash`), `getFlake` of a locked
/// reference needs neither `--impure` nor a registry, and the package is
/// `packages.<system>.clan-cli` of exactly that node. Both arguments are data (`--argstr`),
/// never spliced into this text.
pub const BUILD_EXPR: &str = r"{ locked, system }:
let f = builtins.getFlake (builtins.flakeRefToString (builtins.fromJSON locked));
in f.packages.${system}.clan-cli
";

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
    pub(crate) fn new(step: ClanStep, reason: impl Into<String>) -> Self {
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
    /// Where the GC root links go; `None` for `$XDG_STATE_HOME/auspex/gcroots`
    /// ([`default_gcroot_dir`]). Created private (0700) when missing.
    pub gcroot_dir: Option<&'a Path>,
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
            gcroot_dir: None,
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
    /// The node's `locked` attributes as compact JSON: what [`BUILD_EXPR`] builds from.
    pub locked: String,
    /// A digest of `locked`: what the GC root link is named after.
    pub node_hash: String,
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
    let (revision, locked) = locked_node(locks)?;
    // What nix verifies the fetched tree against. A lock entry without one is not something
    // to build: the content would be whatever the URL serves.
    if !locked.get("narHash").is_some_and(Value::is_string) {
        return Err(format!(
            "the locked {CLAN_CORE_INPUT} has no narHash, so what is fetched could not be \
             checked against the lock"
        ));
    }
    let mut hash = Sha256::new();
    hash.update(locks.to_string().as_bytes());
    let locked = locked.to_string();
    let node_hash = hex(&Sha256::digest(locked.as_bytes()));
    Ok(LockedClanCore {
        revision,
        lock_hash: hex(&hash.finalize()),
        locked,
        node_hash,
    })
}

/// The revision of the root `clan-core` input of a lock (`locks` of the metadata, or the
/// whole of a `flake.lock`, which is the same shape).
fn locked_revision(locks: &Value) -> Result<String, String> {
    locked_node(locks).map(|(revision, _)| revision)
}

/// The revision and the `locked` attributes of the root `clan-core` input of a lock.
fn locked_node(locks: &Value) -> Result<(String, &Value), String> {
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
    Ok((sanitize_line(revision), locked))
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
            "clan on PATH is a different clan-cli derivation than the fleet's locked clan-core \
             builds ({} is not {}); auspex uses the one the fleet's lock builds",
            found.display(),
            pinned.display()
        ))
    }
}

/// Resolves `clan` for a fleet, and remembers the answer.
///
/// Keep one for the whole run. The cache is per `(flake, lock hash, system, nix program)`:
/// a fleet whose lock is updated meanwhile is resolved afresh (the old entry is simply never
/// asked for), and so is another system or another `nix`, which may be another store.
#[derive(Debug, Default)]
pub struct ClanLocator {
    cache: Mutex<HashMap<CacheKey, ClanBinary>>,
}

/// `(flake, lock hash, system, nix program)`.
type CacheKey = (String, String, String, OsString);

/// Where GC root links go by default.
///
/// `$XDG_STATE_HOME/auspex/gcroots` when that variable is an absolute path, else
/// `$HOME/.local/state/auspex/gcroots`; `None` when neither is usable. The one place this
/// module reads the environment for it.
#[must_use]
pub fn default_gcroot_dir() -> Option<PathBuf> {
    gcroot_dir_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn gcroot_dir_from(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    Some(state_dir_from(xdg_state_home, home)?.join("gcroots"))
}

/// auspex's own state directory: `$XDG_STATE_HOME/auspex` when that variable is an absolute
/// path (a relative one is ignored, as the XDG specification says), else
/// `$HOME/.local/state/auspex`; `None` when neither is usable. The GC roots and the action
/// log (`deploylog`) both live under it.
pub(crate) fn state_dir_from(
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    let state = xdg_state_home
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.map(Path::new)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".local").join("state"))
        })?;
    Some(state.join("auspex"))
}

/// Creates `dir` and any missing parents, private (0700), and checks that `dir` is a real
/// directory of ours and still private: a link directory others could write to would let them
/// point a root anywhere.
fn prepare_gcroot_dir(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create the GC root directory {}: {e}", dir.display()))?;
    crate::ssh::prepare_private_dir(dir, "GC root directory")
}

/// The root link for one build: a file name made of the digest of the clan-core node and the
/// system, which are hex and `[A-Za-z0-9_-]` (see [`bad_system`]). Keyed on the node, not on
/// the whole lock: a nixpkgs bump in the fleet changes the lock but not what is built.
fn gcroot_link(dir: &Path, node_hash: &str, system: &str) -> PathBuf {
    dir.join(format!("{}-{system}", &node_hash[..LINK_HASH_LEN]))
}

/// How many hex digits of the node digest a link name has.
const LINK_HASH_LEN: usize = 16;

/// Removes the links of earlier builds for `system` from the GC root directory, so that a
/// clan-core that moves on does not keep every old closure rooted for ever. Returns how many.
///
/// Only what auspex made: a symbolic link, in this directory, named exactly
/// `<16 lowercase hex digits>-<system>` (what [`gcroot_link`] makes), and not `keep`. Any other
/// file, directory or link, and the links of other systems, are left alone. A link that cannot
/// be removed is skipped: a root that stays is harmless, one removed by mistake is not.
fn remove_old_links(dir: &Path, keep: &Path, system: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let suffix = format!("-{system}");
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let ours = name.strip_suffix(&suffix).is_some_and(|digits| {
            digits.len() == LINK_HASH_LEN
                && digits
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if ours && is_link && path != keep && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
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
        if let Some(e) = bad_system(&system).or_else(|| refuse_local_query(flake)) {
            return Ok(Err(e));
        }

        let mut trail = vec!["no explicit clan path configured".to_owned()];
        let locked = match read_lock(params, flake, cancel, &mut trail).await? {
            Ok(l) => l,
            Err(e) => return Ok(Err(e)),
        };

        let key: CacheKey = (
            flake.as_str().to_owned(),
            locked.lock_hash.clone(),
            system.clone(),
            params.nix.to_owned(),
        );
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
            let path = match build(params, &locked, &system, cancel, &mut trail).await? {
                Ok(p) => p,
                Err(e) => return Ok(Err(e)),
            };
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

/// A local reference with a query has a lock file and a directory that cannot be found from it
/// without guessing (`path:/x?dir=sub` is `/x/sub`, not `/x`).
fn refuse_local_query(flake: &FlakeRef) -> Option<ClanError> {
    reference_problem(flake).map(|why| ClanError::new(ClanStep::Lock, why))
}

/// Why `flake` cannot be checked against a lock file and given to `clan`, if it cannot: a
/// reference with a query (`path:/x?dir=sub` is `/x/sub`, and a remote one has nothing local
/// to check), or a local one with no directory that can be found (a `git+file://` with a host,
/// or one that is not a git repository). Resolution and `run` refuse the same references.
fn reference_problem(flake: &FlakeRef) -> Option<String> {
    let shown = sanitize_line(flake.as_str());
    if flake.has_query() {
        return Some(format!(
            "the flake reference {shown:?} has a query, which auspex cannot check against a \
             lock file or give to clan safely; give the fleet as a plain directory"
        ));
    }
    if flake.is_local() && flake.local_dir().is_none() {
        return Some(format!(
            "the flake reference {shown:?} is local but has no directory auspex can find \
             (a relative path is made absolute, so this one is neither a path nor a git \
             repository); give the fleet as a plain directory"
        ));
    }
    None
}

/// Reads the fleet's lock, and checks it against the file on disk for a local fleet.
async fn read_lock(
    params: &ResolveParams<'_>,
    flake: &FlakeRef,
    cancel: &CancellationToken,
    trail: &mut Vec<String>,
) -> Result<Result<LockedClanCore, ClanError>, Cancelled> {
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
    // (`github:…`) has no file to look at.
    if let Some(dir) = flake.local_dir() {
        if let Err(e) = check_lock_file(&dir, &locked.revision).await {
            return Ok(Err(ClanError::new(ClanStep::Lock, e)));
        }
        trail.push("flake.lock on disk agrees".to_owned());
    } else {
        trail.push("flake.lock not checked on disk: the flake is not a local directory".to_owned());
    }
    Ok(Ok(locked))
}

/// Builds `clan-cli` of the locked node, rooted by a link under the GC root directory, and
/// returns its canonical `bin/clan`.
async fn build(
    params: &ResolveParams<'_>,
    locked: &LockedClanCore,
    system: &str,
    cancel: &CancellationToken,
    trail: &mut Vec<String>,
) -> Result<Result<PathBuf, ClanError>, Cancelled> {
    let Some(root_dir) = params
        .gcroot_dir
        .map(Path::to_path_buf)
        .or_else(default_gcroot_dir)
    else {
        return Ok(Err(ClanError::new(
            ClanStep::Build,
            "no directory for the GC root: neither XDG_STATE_HOME nor HOME is an absolute path",
        )));
    };
    if let Err(e) = prepare_gcroot_dir(&root_dir) {
        return Ok(Err(ClanError::new(ClanStep::Build, e)));
    }
    let link = gcroot_link(&root_dir, &locked.node_hash, system);
    let out = match nix_step(
        params.nix,
        ClanStep::Build,
        build_argv(&locked.locked, system, &link),
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
        "nix build packages.{system}.{CLAN_CLI_PACKAGE} of the locked {CLAN_CORE_INPUT} node \
         itself (rooted by {}): {}",
        link.display(),
        path.display()
    ));
    let removed = remove_old_links(&root_dir, &link, system);
    if removed > 0 {
        trail.push(format!(
            "removed {removed} GC root link(s) of earlier {CLAN_CORE_INPUT} builds for {system}"
        ));
    }
    Ok(Ok(path))
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
        "--no-update-lock-file",
        "--",
        &flake.shallow_safe(),
    ]
    .map(OsString::from)
    .to_vec()
}

/// The exact argv (after the program) of the build: [`BUILD_EXPR`] with the node's `locked`
/// attributes and the system as strings, the result rooted by `out_link`.
#[must_use]
pub fn build_argv(locked: &str, system: &str, out_link: &Path) -> Vec<OsString> {
    let mut argv: Vec<OsString> = [
        "build",
        "--print-out-paths",
        "--no-write-lock-file",
        "--out-link",
    ]
    .map(OsString::from)
    .to_vec();
    argv.push(out_link.as_os_str().to_owned());
    argv.extend(
        [
            "--expr", BUILD_EXPR, "--argstr", "locked", locked, "--argstr", "system", system,
        ]
        .map(OsString::from),
    );
    argv
}

/// `bin/clan` of the first printed output that has one, canonicalised, which must be inside the
/// nix store: a symlink out of it is refused, so the file that runs is the file that was
/// locked.
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
            Ok(()) => {}
            Err(e) => {
                last = e;
                continue;
            }
        }
        let canonical = std::fs::canonicalize(&candidate).map_err(|e| {
            ClanError::new(ClanStep::Binary, format!("{}: {e}", candidate.display()))
        })?;
        let store = std::fs::canonicalize(STORE_DIR).unwrap_or_else(|_| PathBuf::from(STORE_DIR));
        if !canonical.starts_with(&store) {
            return Err(ClanError::new(
                ClanStep::Binary,
                format!(
                    "{} resolves to {}, which is outside the nix store {}: the file that would \
                     run is not the one that was locked",
                    candidate.display(),
                    canonical.display(),
                    store.display()
                ),
            ));
        }
        return Ok(canonical);
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
    run_argv_with_tail(flake, args, &[])
}

/// [`run_argv`] with `tail` after `--flake <dir>`.
///
/// For a command that ends its options with `--` and names its operands after it (`clan
/// machines update --flake <dir> -- <machine>`): the flake must come before the `--`, or it
/// would be read as an operand.
#[must_use]
pub fn run_argv_with_tail(flake: &FlakeRef, args: &[OsString], tail: &[OsString]) -> Vec<OsString> {
    let mut full = args.to_vec();
    full.push(OsString::from("--flake"));
    full.push(
        flake
            .local_dir()
            .map_or_else(|| OsString::from(flake.as_str()), PathBuf::into_os_string),
    );
    full.extend(tail.iter().cloned());
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
    run_with_tail(program, flake, args, &[], params, cancel, tx).await
}

/// [`run`] with `tail` after `--flake <dir>` ([`run_argv_with_tail`]).
///
/// # Errors
///
/// As [`run`].
pub async fn run_with_tail(
    program: &Path,
    flake: &FlakeRef,
    args: &[OsString],
    tail: &[OsString],
    params: &RunParams,
    cancel: &CancellationToken,
    tx: UnboundedSender<ClanLine>,
) -> Result<Result<ClanRun, ClanError>, Cancelled> {
    let sp = match start(program, flake, args, tail) {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    stream(sp, params, cancel, tx).await
}

/// Streams a spawned process's lines until it ends: the body of [`run`], shared with the
/// other long actions (`nix build`), which need the same caps, the same cancellation and the
/// same rule that an exit that is already there is what gets reported.
pub(crate) async fn stream(
    mut sp: Spawned,
    params: &RunParams,
    cancel: &CancellationToken,
    tx: UnboundedSender<ClanLine>,
) -> Result<Result<ClanRun, ClanError>, Cancelled> {
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
            // First: an exit that is already there wins over a cancellation that is ready at
            // the same moment, because the status is known and is what gets logged.
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
            () = cancel.cancelled(), if exited.is_none() => {
                exited = Some(stop(&mut sp).await?);
                break;
            }
            // Nobody is listening any more: the same as being told to stop.
            () = tx.closed(), if exited.is_none() => {
                exited = Some(stop(&mut sp).await?);
                break;
            }
            // After the exit the same two only cut the drain of the pipes short: clan is gone
            // (its group was killed at the exit), the status is known, and it is returned.
            () = cancel.cancelled(), if exited.is_some() => break,
            () = tx.closed(), if exited.is_some() => break,
            () = sleep_until(deadline), if exited.is_none() => {
                // The same question as for a cancel: an exit that landed with the deadline is
                // an exit, and its status is kept; only a process still running is killed.
                if let Ok(status) = stop(&mut sp).await {
                    exited = Some(status);
                    grace = Some(Instant::now() + PIPE_GRACE);
                } else {
                    timed_out = true;
                    break;
                }
            }
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

/// Spawns `<program> <args> --flake <dir>` in the fleet's directory, refusing a flake
/// reference `clan` cannot be given safely.
fn start(
    program: &Path,
    flake: &FlakeRef,
    args: &[OsString],
    tail: &[OsString],
) -> Result<Spawned, ClanError> {
    // `clan` takes a directory (or a URL), and neither can be got from a query or from a local
    // reference with no directory without guessing: `path:/x?dir=sub` would run clan against
    // `/x`, and a reference clan resolves against auspex's own working directory is the wrong
    // flake.
    if let Some(why) = reference_problem(flake) {
        return Err(ClanError::new(ClanStep::Run, why));
    }
    let cannot = |e: &std::io::Error| {
        ClanError::new(
            ClanStep::Run,
            spawn_error("cannot run clan", program.as_os_str(), e),
        )
    };
    let mut cmd = command(program.as_os_str()).map_err(|e| cannot(&e))?;
    cmd.args(run_argv_with_tail(flake, args, tail));
    apply_run_environment(&mut cmd);
    if let Some(dir) = flake.local_dir() {
        cmd.current_dir(dir);
    }
    spawn(cmd, false, OnExit::KillGroup, None).map_err(|e| cannot(&e))
}

/// The environment policy for what `clan` starts: no GUI prompts.
///
/// The probe's policy for ssh (`ssh::probe`): no `SSH_ASKPASS` program and
/// `SSH_ASKPASS_REQUIRE=never`; and the same for what clan may start besides ssh: `GIT_ASKPASS`
/// (git, for a `git+ssh` input) and `SUDO_ASKPASS` (`sudo -A`) are removed. `DISPLAY` and
/// `SSH_AUTH_SOCK` are left alone: an agent's approval (the operator's Bitwarden agent) goes
/// through the socket, and that is the one approval a deploy is meant to wait for.
pub(crate) fn apply_run_environment(cmd: &mut tokio::process::Command) {
    cmd.env_remove("SSH_ASKPASS")
        .env_remove("GIT_ASKPASS")
        .env_remove("SUDO_ASKPASS")
        .env("SSH_ASKPASS_REQUIRE", "never");
}

/// Told to stop: the exit status when clan has in fact exited already (it is what gets logged),
/// else kills the whole group and reaps it, and says so.
async fn stop(sp: &mut Spawned) -> Result<ExitStatus, Cancelled> {
    if let Some(status) = sp.exited_now().await {
        return Ok(status);
    }
    sp.kill_and_reap().await;
    Err(Cancelled)
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
            node("deep", &format!(r#""rev":"{REV}","narHash":"sha256-d""#),)
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
                node("c", r#""rev":"1","narHash":"sha256-1""#),
                node("n", r#""rev":"1""#)
            ),
        );
        let b = metadata(
            r#""clan-core":"c""#,
            &format!(
                "{},{}",
                node("c", r#""rev":"1","narHash":"sha256-1""#),
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
        // `--no-update-lock-file` alone: next to `--no-write-lock-file` nix only warns.
        assert!(m.contains(&"--no-update-lock-file".to_owned()), "{m:?}");
        assert!(!m.contains(&"--no-write-lock-file".to_owned()), "{m:?}");

        // The build is of the locked node, by value: no fleet, no registry name.
        let locked = r#"{"narHash":"sha256-x","rev":"abc","type":"git","url":"file:///c"}"#;
        let b: Vec<_> = build_argv(
            locked,
            "x86_64-linux",
            Path::new("/state/gcroots/k-x86_64-linux"),
        )
        .into_iter()
        .map(|s| s.into_string().unwrap())
        .collect();
        for pair in [
            ["--out-link", "/state/gcroots/k-x86_64-linux"],
            ["--expr", BUILD_EXPR],
        ] {
            assert!(b.windows(2).any(|w| w == pair), "{pair:?} not in {b:?}");
        }
        assert!(
            b.windows(3).any(|w| w == ["--argstr", "locked", locked]),
            "{b:?}"
        );
        assert!(
            b.windows(3)
                .any(|w| w == ["--argstr", "system", "x86_64-linux"]),
            "{b:?}"
        );
        assert!(
            !b.iter()
                .any(|a| a.contains("inputs-from") || a.contains("clan-core#")),
            "{b:?}"
        );
        assert!(!b.contains(&"--no-link".to_owned()), "{b:?}");
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
    fn a_locked_clan_core_without_a_nar_hash_is_refused_and_the_node_is_kept_whole() {
        let json = metadata(
            r#""clan-core":"c""#,
            &node("c", r#""rev":"abc","type":"git""#),
        );
        let e = clan_core_lock(&json).unwrap_err();
        assert!(e.contains("no narHash"), "{e}");
        let json = metadata(
            r#""clan-core":"c""#,
            &node("c", r#""rev":"abc","narHash":"sha256-n","type":"git""#),
        );
        let locked: Value = serde_json::from_str(&clan_core_lock(&json).unwrap().locked).unwrap();
        assert_eq!(locked["type"], "git");
        assert_eq!(locked["narHash"], "sha256-n");
    }

    #[test]
    fn the_gc_root_directory_is_under_the_state_directory() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            gcroot_dir_from(xdg.map(OsStr::new), home.map(OsStr::new))
        };
        assert_eq!(
            dir(Some("/state"), Some("/home/u")),
            Some(PathBuf::from("/state/auspex/gcroots"))
        );
        assert_eq!(
            dir(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/auspex/gcroots"))
        );
        // Relative values are ignored, as the cache directory does.
        assert_eq!(
            dir(Some("rel"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/auspex/gcroots"))
        );
        assert_eq!(dir(Some("rel"), Some("also-rel")), None);
        assert_eq!(dir(None, None), None);
    }

    #[test]
    fn the_gc_root_is_named_after_the_node_not_the_whole_lock() {
        let json = |other: &str| {
            metadata(
                r#""clan-core":"c""#,
                &format!(
                    "{},{}",
                    node("c", r#""rev":"1","narHash":"sha256-1""#),
                    node("n", &format!(r#""rev":"{other}""#))
                ),
            )
        };
        let (a, b) = (
            clan_core_lock(&json("1")).unwrap(),
            clan_core_lock(&json("2")).unwrap(),
        );
        // A nixpkgs bump changes the lock and not the node: the same link, a different key.
        assert_ne!(a.lock_hash, b.lock_hash);
        assert_eq!(a.node_hash, b.node_hash);
        assert_eq!(a.node_hash.len(), 64);
        // Another clan-core is another node.
        let c = metadata(
            r#""clan-core":"c""#,
            &node("c", r#""rev":"9","narHash":"sha256-9""#),
        );
        assert_ne!(a.node_hash, clan_core_lock(&c).unwrap().node_hash);
    }

    #[test]
    fn only_auspexs_own_links_for_the_same_system_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let target = d.join("target");
        std::fs::write(&target, "x").unwrap();
        let link = |name: &str| {
            std::os::unix::fs::symlink(&target, d.join(name)).unwrap();
            d.join(name)
        };
        let keep = link("aaaaaaaaaaaaaaaa-x86_64-linux");
        let old = link("0123456789abcdef-x86_64-linux");
        // What must stay: another system's, a wrong-looking name, a wrong digit set or length,
        // a name with something after the system, a regular file and a directory with the
        // exact name, and a plain file.
        let other_system = link("0123456789abcdef-aarch64-linux");
        let short = link("0123456789abcde-x86_64-linux");
        let upper = link("0123456789ABCDEF-x86_64-linux");
        let suffixed = link("0123456789abcdef-x86_64-linux-dev");
        let named = link("notes-x86_64-linux");
        std::fs::write(d.join("1111111111111111-x86_64-linux"), "file").unwrap();
        std::fs::create_dir(d.join("2222222222222222-x86_64-linux")).unwrap();
        std::fs::write(d.join("README"), "file").unwrap();

        assert_eq!(remove_old_links(d, &keep, "x86_64-linux"), 1);
        assert!(!old.exists() && old.symlink_metadata().is_err());
        for survivor in [
            &keep,
            &other_system,
            &short,
            &upper,
            &suffixed,
            &named,
            &target,
        ] {
            assert!(
                survivor.symlink_metadata().is_ok(),
                "{survivor:?} was removed"
            );
        }
        assert!(d.join("1111111111111111-x86_64-linux").is_file());
        assert!(d.join("2222222222222222-x86_64-linux").is_dir());
        assert!(d.join("README").is_file());
        // Nothing to remove is not an error, and a missing directory is not either.
        assert_eq!(remove_old_links(d, &keep, "x86_64-linux"), 0);
        assert_eq!(
            remove_old_links(&d.join("absent"), &keep, "x86_64-linux"),
            0
        );
    }

    #[test]
    fn a_relative_or_directoryless_reference_is_refused_and_a_plain_one_is_not() {
        let problem = |r: &str| reference_problem(&FlakeRef::new(r).unwrap());
        assert!(problem("path:/fleet").is_none());
        assert!(problem("github:o/r").is_none());
        assert!(problem("path:/fleet?dir=sub").unwrap().contains("query"));
        assert!(problem("github:o/r?dir=x").unwrap().contains("query"));
        assert!(
            problem("git+file://host/x")
                .unwrap()
                .contains("no directory")
        );
        assert!(
            problem("git+file:///not/a/repository")
                .unwrap()
                .contains("no directory")
        );
    }

    #[test]
    fn one_gc_root_link_per_lock_and_system() {
        let d = Path::new("/g");
        let hash = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            gcroot_link(d, hash, "x86_64-linux"),
            Path::new("/g/0123456789abcdef-x86_64-linux")
        );
        assert_ne!(
            gcroot_link(d, hash, "x86_64-linux"),
            gcroot_link(d, hash, "aarch64-linux")
        );
    }

    #[tokio::test]
    async fn the_gc_root_directory_is_private_and_not_a_symlink() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("a").join("gcroots");
        prepare_gcroot_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Run again: an existing private directory is fine.
        prepare_gcroot_dir(&dir).unwrap();
        // One that others could write to is refused.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_gcroot_dir(&dir).unwrap_err().contains("0700"));
        // And a symlink is not followed.
        let link = base.path().join("link");
        std::os::unix::fs::symlink(base.path(), &link).unwrap();
        assert!(prepare_gcroot_dir(&link).is_err());
    }

    #[test]
    fn clan_gets_no_askpass_but_keeps_the_agent_socket() {
        let mut cmd = tokio::process::Command::new("/bin/true");
        apply_run_environment(&mut cmd);
        let envs: Vec<_> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
            .collect();
        // The askpass programs are removed, SSH_ASKPASS_REQUIRE is `never`, and SSH_AUTH_SOCK and
        // DISPLAY are not touched.
        for name in ["SSH_ASKPASS", "GIT_ASKPASS", "SUDO_ASKPASS"] {
            assert!(
                envs.contains(&(OsString::from(name), None)),
                "{name} not removed: {envs:?}"
            );
        }
        assert!(
            envs.contains(&(
                OsString::from("SSH_ASKPASS_REQUIRE"),
                Some(OsString::from("never"))
            )),
            "{envs:?}"
        );
        assert!(
            envs.iter()
                .all(|(k, _)| k != "SSH_AUTH_SOCK" && k != "DISPLAY"),
            "{envs:?}"
        );
    }

    #[test]
    fn a_different_clan_on_path_is_described_as_a_different_derivation() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = dir.path().join("clan");
        std::fs::write(&theirs, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ours = dir.path().join("ours");
        std::fs::write(&ours, "x").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        let PathClan::Different(msg) = compare_path_clan(Some(path.as_os_str()), &ours) else {
            panic!("not different");
        };
        // Not "a different clan-core": the same revision may build another closure.
        assert!(
            msg.contains(
                "a different clan-cli derivation than the fleet's locked clan-core builds"
            ),
            "{msg}"
        );
        assert!(!msg.contains("different clan-core than"), "{msg}");
        assert_eq!(
            compare_path_clan(Some(path.as_os_str()), &theirs),
            PathClan::Same
        );
        assert_eq!(
            compare_path_clan(Some(OsStr::new("")), &ours),
            PathClan::Absent
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
        // A reference with a query names a directory this cannot work out: none is guessed.
        let q = FlakeRef::new("path:/fleet?dir=sub").unwrap();
        assert!(q.has_query() && q.is_local());
        assert_eq!(q.local_dir(), None);
        assert!(!r.is_local());
    }
}
