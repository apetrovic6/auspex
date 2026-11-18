//! The machine list and the evaluation.
//!
//! **Names and paths are data.** Nothing here builds Nix source text from a value:
//!
//! - The machine list is `nix eval --json --impure --no-write-lock-file --
//!   <flake>#clanInternals.inventoryClass.inventory.machines`: the flake reference is one
//!   argv element after `--`, the attribute path is a constant, and nix parses both itself.
//! - The evaluation runs a constant expression, [`EVAL_EXPR`], a function of two strings:
//!   `flake` (the flake reference, `--argstr`) and `machines` (a JSON list of
//!   `{name, class}` written by `serde_json` into a 0600 file in the per-user runtime
//!   directory, read with `--arg-from-file`, so a large fleet is not limited by the
//!   128 KiB a single argument may hold). A name nix would refuse in the list as a whole
//!   (a NUL, an empty or oversized one) gets its own error and is left out. The expression
//!   selects each configuration
//!   with `${m.name}`, a dynamic attribute *lookup* by a string value, so a name such as
//!   `x"; builtins.abort "pwned` is just an attribute that does not exist.
//!
//! A [`FlakeRef`] is refused when it could be read as an option (a leading `-`), carries a
//! `#` (which would change the attribute path), or contains control characters. No shell
//! is involved anywhere: `nix` and `nix-eval-jobs` are executed directly.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use auspex_domain::eval::{EvalFailure, EvalOutcome, parse_eval_line};
use auspex_domain::fleet::{Machine, MachineClass, parse_inventory};
use auspex_domain::text::sanitize_line;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::process::{
    End, Limits, Line, Lines, OnExit, PIPE_GRACE, Spawned, Tail, command, describe_status, run,
    spawn, spawn_error,
};
use crate::ssh::{default_runtime_path, prepare_private_dir};
use crate::text::{lossy, reason};

/// The default `nix` program, looked up on `PATH`.
pub const DEFAULT_NIX: &str = "nix";
/// The default `nix-eval-jobs` program, looked up on `PATH`.
pub const DEFAULT_NIX_EVAL_JOBS: &str = "nix-eval-jobs";

/// Where Clan keeps the inventory's machines.
pub const INVENTORY_ATTR: &str = "clanInternals.inventoryClass.inventory.machines";

/// How much of `nix eval`'s stdout is read for the machine list: 16 MiB. A fleet of
/// hundreds of machines is tens of kilobytes; past the cap the list is an error.
pub const INVENTORY_STDOUT_CAP: usize = 16 * 1024 * 1024;

/// How long one `nix-eval-jobs` output line may be: 4 MiB.
///
/// A line is one machine's result,
/// and an error's trace is the long part. A longer line is dropped with a note, and that
/// machine is left for the core's "no result" rule.
pub const EVAL_LINE_CAP: usize = 4 * 1024 * 1024;

/// How much of stderr's tail is kept, for both commands: 64 KiB. Only the last lines are
/// shown (`auspex_domain::eval::ERROR_TAIL_LINES`).
pub const STDERR_CAP: usize = 64 * 1024;

/// At most this many notes about unusable output lines per evaluation, so a broken
/// evaluator printing garbage cannot grow the channel without bound.
pub const MAX_LINE_NOTES: usize = 16;

/// The evaluation expression.
///
/// A function of `flake` (a flake reference) and `machines` (a
/// JSON list of `{"name", "class"}`), returning an attribute set named by machine, which
/// `nix-eval-jobs` walks, one job per attribute.
///
/// The attributes are the ones each tool deploys. NixOS:
/// `nixosConfigurations.<name>.config.system.build.toplevel`. nix-darwin:
/// `darwinConfigurations.<name>.system`, which nix-darwin's `eval-config.nix` defines as
/// `config.system.build.toplevel` and which `darwin-rebuild` builds (`$flake#$attr.system`).
pub const EVAL_EXPR: &str = r#"{ flake, machines }:
let
  f = builtins.getFlake flake;
  select = m:
    if m.class == "darwin"
    then f.darwinConfigurations.${m.name}.system
    else f.nixosConfigurations.${m.name}.config.system.build.toplevel;
in
builtins.listToAttrs (map (m: { name = m.name; value = select m; }) (builtins.fromJSON machines))
"#;

/// A flake reference that is safe to hand to nix as one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakeRef {
    reference: String,
    /// The flake directory, canonical, for a local git reference.
    worktree: Option<std::path::PathBuf>,
    /// The repository top level that reference names, canonical.
    top: Option<std::path::PathBuf>,
    queried: bool,
}

/// Why a flake reference was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakeRefError(pub String);

impl fmt::Display for FlakeRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlakeRefError {}

impl FlakeRef {
    /// A URL-like reference (`path:/abs`, `git+file:///abs`, `github:o/r`) or a
    /// filesystem path. Paths inside a repository become an explicit `git+file://`
    /// reference to its root, with `?dir=` for a nested flake. Both Nix entry points
    /// therefore read tracked files: `builtins.getFlake "/abs"` alone reads a path
    /// source, unlike the `nix eval /abs#…` CLI.
    ///
    /// # Errors
    ///
    /// When it is empty, starts with `-`, contains `#`, or contains a control character.
    pub fn new(s: &str) -> Result<Self, FlakeRefError> {
        if s.is_empty() {
            return Err(FlakeRefError("the flake reference is empty".to_owned()));
        }
        if s.starts_with('-') {
            return Err(FlakeRefError(format!(
                "flake reference {s:?} starts with '-' and would be read as an option"
            )));
        }
        if s.contains('#') {
            return Err(FlakeRefError(format!(
                "flake reference {s:?} contains '#'; auspex chooses the attribute itself"
            )));
        }
        if s.chars().any(char::is_control) {
            return Err(FlakeRefError(
                "flake reference contains a control character".to_owned(),
            ));
        }
        let queried = has_scheme(s) && s.contains('?');
        let local = if let Some(file) = s.strip_prefix("git+") {
            (!queried)
                .then(|| url::Url::parse(file).ok()?.to_file_path().ok())
                .flatten()
        } else if has_scheme(s) {
            None
        } else {
            Some(
                std::path::absolute(Path::new(s))
                    .map_err(|e| FlakeRefError(format!("cannot make {s:?} absolute: {e}")))?,
            )
        };
        if let Some(path) = local {
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            if let Some(top) = path.ancestors().find(|p| p.join(".git").exists()) {
                let rel = path
                    .strip_prefix(top)
                    .map_err(|e| FlakeRefError(e.to_string()))?;
                let mut file = url::Url::from_directory_path(top)
                    .map_err(|()| FlakeRefError("cannot encode the repository path".to_owned()))?;
                if !rel.as_os_str().is_empty() {
                    let rel = rel.to_str().ok_or_else(|| {
                        FlakeRefError("the flake directory is not UTF-8".to_owned())
                    })?;
                    file.query_pairs_mut().append_pair("dir", rel);
                    // Nix decodes URI escapes, not HTML form '+' escapes.
                    if let Some(query) = file.query() {
                        let query = query.replace('+', "%20");
                        file.set_query(Some(&query));
                    }
                }
                return Ok(Self {
                    reference: format!("git+{file}"),
                    top: Some(top.to_path_buf()),
                    worktree: Some(path),
                    queried,
                });
            }
            if !has_scheme(s) {
                let path = path
                    .to_str()
                    .ok_or_else(|| FlakeRefError(format!("flake path {s:?} is not UTF-8")))?;
                // Explicit path syntax also prevents '?' and '%' in a filename from
                // being mistaken for URL syntax.
                let file = url::Url::from_file_path(path)
                    .map_err(|()| FlakeRefError("cannot encode the flake path".to_owned()))?;
                return Ok(Self {
                    reference: format!("path:{}", file.path()),
                    worktree: None,
                    top: None,
                    queried: false,
                });
            }
        }
        Ok(Self {
            reference: s.to_owned(),
            worktree: None,
            top: None,
            queried,
        })
    }

    /// The reference as nix will see it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.reference
    }

    /// Whether this is an explicit git worktree reference eligible for caching.
    /// Plain paths in repositories are normalised by [`Self::new`]; explicit `path:`
    /// sources include untracked files and are never cached.
    #[must_use]
    pub fn is_git(&self) -> bool {
        self.reference.starts_with("git+file:") && !self.queried
    }

    /// Whether the input carried a query (`?ref=`, `?rev=`, `?dir=`, `?submodules=`).
    /// Such inputs are not cached. A `dir` added by [`Self::new`] is different: it names
    /// exactly the worktree directory recorded alongside the normalised reference.
    #[must_use]
    pub const fn has_query(&self) -> bool {
        self.queried
    }

    /// The scheme (`path`, `github`), or `path` for a plain path, for messages.
    #[must_use]
    pub fn scheme(&self) -> &str {
        if has_scheme(&self.reference) {
            self.reference.split_once(':').map_or("", |(s, _)| s)
        } else {
            "plain path"
        }
    }

    /// The reference for a command that reads the flake's *metadata* (`nix flake metadata`,
    /// `nix build --inputs-from`): [`Self::as_str`], plus `shallow=1` for a local git
    /// reference.
    ///
    /// Without it nix refuses a shallow clone outright ("is a shallow Git repository, so
    /// 'revCount' is not available"), because metadata always includes `revCount`. A fleet
    /// checked out with `--depth` is an ordinary setup. Merged into an existing query rather
    /// than appended blindly, and left alone when the operator already chose a `shallow`
    /// value. Not used by evaluation: `nix eval` does not ask for `revCount` and works on a
    /// shallow clone as it is.
    #[must_use]
    pub fn shallow_safe(&self) -> String {
        if !self.reference.starts_with("git+file:") {
            return self.reference.clone();
        }
        match self.reference.split_once('?') {
            None => format!("{}?shallow=1", self.reference),
            Some((_, query)) if query.split('&').any(|p| p.starts_with("shallow=")) => {
                self.reference.clone()
            }
            Some(_) => format!("{}&shallow=1", self.reference),
        }
    }

    /// The flake's directory on this machine, when it has one: the worktree of a git
    /// reference, or the path of a `path:` reference. What `clan` is run from and given as
    /// `--flake`, since it takes a directory, not a normalised `git+file://` URL.
    pub(crate) fn local_dir(&self) -> Option<std::path::PathBuf> {
        if let Some(dir) = &self.worktree {
            return Some(dir.clone());
        }
        let rest = self.reference.strip_prefix("path:")?;
        // `path:` references are built by `new` from `Url::path()`, which percent-encodes.
        url::Url::parse(&format!("file://{rest}"))
            .ok()?
            .to_file_path()
            .ok()
    }

    pub(crate) fn worktree_dir(&self) -> Option<&Path> {
        self.worktree.as_deref()
    }

    pub(crate) fn repo_top(&self) -> Option<&Path> {
        self.top.as_deref()
    }
}

/// `scheme:` at the start, as in a URL: a letter, then letters, digits, `+`, `-`, `.`.
fn has_scheme(s: &str) -> bool {
    s.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// The exact argv (after the program) of the machine-list evaluation.
#[must_use]
pub fn inventory_argv(flake: &FlakeRef) -> Vec<String> {
    vec![
        "eval".to_owned(),
        "--json".to_owned(),
        "--impure".to_owned(),
        "--no-write-lock-file".to_owned(),
        "--".to_owned(),
        format!("{}#{INVENTORY_ATTR}", flake.as_str()),
    ]
}

/// Reads the machine list. Serves the core's `Effect::Inventory`.
///
/// One cheap `nix eval` ([`inventory_argv`]), then `parse_inventory`. Every failure (nix
/// missing, the timeout, a non-zero exit with nix's last stderr lines, oversized output, a
/// malformed inventory) is an `Err` with a reason to show.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; `nix`'s process group has been killed and reaped.
pub async fn inventory(
    nix: &OsStr,
    flake: &FlakeRef,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<Vec<Machine>, String>, Cancelled> {
    let mut cmd = match command(nix) {
        Ok(c) => c,
        Err(e) => return Ok(Err(spawn_error("cannot list machines", nix, &e))),
    };
    cmd.args(inventory_argv(flake));
    let limits = Limits {
        stdout: INVENTORY_STDOUT_CAP,
        stderr: STDERR_CAP,
        memory: None,
    };
    let deadline = Instant::now() + timeout;
    let ran = match run(cmd, None, OnExit::KillGroup, limits, deadline, cancel).await {
        Ok(r) => r,
        Err(e) => return Ok(Err(spawn_error("cannot list machines", nix, &e))),
    };
    let stderr = lossy(&ran.stderr);
    let with_stderr = |what: String| match reason(&stderr) {
        Some(r) => format!("{what}: {r}"),
        None => what,
    };
    Ok(match ran.end {
        End::Cancelled => return Err(Cancelled),
        End::TimedOut => Err(with_stderr(format!(
            "listing machines timed out after {timeout:?}"
        ))),
        End::Overflow => Err(format!(
            "listing machines: nix eval printed more than {INVENTORY_STDOUT_CAP} bytes"
        )),
        End::Exited(s) if !s.success() => Err(with_stderr(format!(
            "listing machines: nix eval {}",
            describe_status(Some(s))
        ))),
        End::Exited(_) => match ran.whole_stdout() {
            Ok(out) => parse_inventory(&lossy(out)).map_err(|e| e.to_string()),
            Err(true) => Err(format!(
                "listing machines: nix eval printed more than {INVENTORY_STDOUT_CAP} bytes"
            )),
            Err(false) => Err(
                "listing machines: nix eval exited, but its output was not read to the end \
                 (something still held it open)"
                    .to_owned(),
            ),
        },
    })
}

/// One machine to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalMachine {
    /// The machine's name: the attribute under `nixosConfigurations`/`darwinConfigurations`.
    pub name: String,
    /// Which of the two.
    pub class: MachineClass,
}

/// `nix-eval-jobs`'s settings.
#[derive(Debug, Clone)]
pub struct EvalParams<'a> {
    /// The program: [`DEFAULT_NIX_EVAL_JOBS`] (from `PATH`) or a path.
    pub program: &'a OsStr,
    /// `--workers`: evaluator processes in parallel.
    pub workers: u32,
    /// `--max-memory-size`, MiB per worker.
    pub max_memory_mib: u32,
    /// The whole evaluation's deadline. Past it the process group is killed, and the
    /// final item says so.
    pub timeout: Duration,
    /// Where the machine list file goes: a private directory (0700, ours, no symlink),
    /// without a socket-path length limit. `None` uses `$XDG_RUNTIME_DIR/auspex`, or
    /// `auspex-<uid>` under the system temporary directory if it is unset or relative.
    pub runtime_dir: Option<std::path::PathBuf>,
}

impl EvalParams<'_> {
    /// `nix-eval-jobs` from `PATH`, 4 workers, 4096 MiB each (nix-eval-jobs's own
    /// default), 5 minutes (8 machines take about 44 s on omnissiah).
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: OsStr::new(DEFAULT_NIX_EVAL_JOBS),
            workers: 4,
            max_memory_mib: 4096,
            timeout: Duration::from_secs(300),
            runtime_dir: None,
        }
    }
}

impl Default for EvalParams<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// One item of an evaluation's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalItem {
    /// A machine's result, as it arrived. `outcome`'s `attr` is the machine's name and its
    /// `attr_path` is `[name]` (nix-eval-jobs quotes names that are not bare Nix
    /// identifiers, such as `"1odd"`; the name is recovered from `attrPath`).
    Machine {
        /// The machine.
        machine: String,
        /// Its result.
        outcome: EvalOutcome,
        /// Read from the eval cache rather than evaluated (log it).
        from_cache: bool,
    },
    /// Something the operator may want logged that is not anyone's result: an unusable
    /// output line, a cache entry that could not be written. Never fatal.
    Note(String),
    /// The last item. `error` is the evaluator's own failure (it could not start, crashed,
    /// exited non-zero, or timed out), with its last stderr lines; `None` when it exited 0.
    Finished {
        /// Why the evaluator failed, if it did.
        error: Option<String>,
    },
}

/// The exact argv (after the program) of an evaluation.
///
/// The machine list is not in it: it is
/// read from `list`, a file holding [`eval_machines_json`] (`--arg-from-file machines`), so
/// its size is not bounded by the kernel's 128 KiB limit on one argument. Not stdin:
/// `--arg-from-stdin` is read again by every worker, and all but the first see it empty.
#[must_use]
pub fn eval_argv(
    flake: &FlakeRef,
    list: &Path,
    params: &EvalParams<'_>,
) -> Vec<std::ffi::OsString> {
    vec![
        "--impure".to_owned(),
        "--workers".to_owned(),
        params.workers.max(1).to_string(),
        "--max-memory-size".to_owned(),
        params.max_memory_mib.max(1).to_string(),
        "--argstr".to_owned(),
        "flake".to_owned(),
        flake.as_str().to_owned(),
        "--arg-from-file".to_owned(),
        "machines".to_owned(),
    ]
    .into_iter()
    .map(Into::into)
    .chain([
        list.as_os_str().to_owned(),
        "--expr".into(),
        EVAL_EXPR.into(),
    ])
    .collect()
}

/// The machine list file, private to this call and removed when dropped.
struct ListFile(std::path::PathBuf);

static LISTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The list files' name: `machines-<pid>-<n>.json`.
fn list_file_pid(name: &str) -> Option<i32> {
    let rest = name.strip_prefix("machines-")?.strip_suffix(".json")?;
    let (pid, n) = rest.split_once('-')?;
    n.bytes().all(|b| b.is_ascii_digit()).then_some(())?;
    pid.parse().ok()
}

impl ListFile {
    /// Creates it in `dir` exclusively (`O_EXCL`, which also refuses a planted symlink)
    /// and readable only by us. The directory must already be checked 0700 and ours.
    /// Only names and classes are in it.
    ///
    /// A crash or `SIGKILL` leaves the file behind (review finding N7), so each call first
    /// removes the list files whose creating process is gone: the pid in the name no
    /// longer exists (`kill(pid, 0)` answers `ESRCH`). A pid reused since then only keeps a
    /// stale file longer; it never makes a live call's file look stale.
    fn create(dir: &Path, contents: &str) -> std::io::Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name();
                let Some(pid) = name.to_str().and_then(list_file_pid) else {
                    continue;
                };
                let gone = rustix::process::Pid::from_raw(pid).is_none_or(|p| {
                    rustix::process::test_kill_process(p) == Err(rustix::io::Errno::SRCH)
                });
                if gone {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let mut last = None;
        for _ in 0..8 {
            let path = dir.join(format!(
                "machines-{}-{}.json",
                std::process::id(),
                LISTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut f) => {
                    let file = Self(path);
                    f.write_all(contents.as_bytes())?;
                    return Ok(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no list file")))
    }
}

impl Drop for ListFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The machine list as the evaluator reads it: a JSON list of `{name, class}`, written by
/// `serde_json`.
#[must_use]
pub fn eval_machines_json(machines: &[EvalMachine]) -> String {
    let list: Vec<_> = machines
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "class": match m.class {
                    MachineClass::Nixos => "nixos",
                    MachineClass::Darwin => "darwin",
                },
            })
        })
        .collect();
    serde_json::Value::Array(list).to_string()
}

/// The longest machine name passed to nix. Inventory names are at most 63 characters; this
/// bound is only so one absurd name cannot make the list huge.
pub const MAX_NAME_BYTES: usize = 1024;

/// Why `name` cannot be evaluated, or `None` when it can. Names are data, so almost
/// anything is fine; what is refused is what would make nix reject the *whole* list
/// (review finding F12): `builtins.fromJSON` refuses a NUL anywhere in it.
fn unusable(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("it is empty")
    } else if name.contains('\0') {
        Some("it contains a NUL byte")
    } else if name.len() > MAX_NAME_BYTES {
        Some("it is longer than 1024 bytes")
    } else {
        None
    }
}

/// The error a machine with an unusable name gets, instead of failing the others.
fn refused(name: &str, why: &str) -> EvalOutcome {
    EvalOutcome::Error(EvalFailure {
        attr: sanitize_line(name),
        attr_path: vec![name.to_owned()],
        error_tail: vec![format!("error: auspex cannot evaluate this machine: {why}")],
    })
}

/// Which requested machine a result belongs to: the domain's rule, the one element of
/// `attrPath` (`EvalOutcome::machine`).
fn machine_of(outcome: &EvalOutcome, requested: &BTreeSet<&str>) -> Option<String> {
    outcome
        .machine()
        .filter(|m| requested.contains(m))
        .map(str::to_owned)
}

/// Sets a result's display-only `attr` to its machine. nix-eval-jobs prints `"\"1odd\""`
/// for a name that is not a bare identifier, and a cache entry's `attr` is not checked on
/// read, so without this the shell could be shown either, escape sequences included.
fn named(outcome: EvalOutcome, machine: &str) -> EvalOutcome {
    match outcome {
        EvalOutcome::Success(mut s) => {
            machine.clone_into(&mut s.attr);
            s.attr_path = vec![machine.to_owned()];
            EvalOutcome::Success(s)
        }
        EvalOutcome::Error(mut e) => {
            machine.clone_into(&mut e.attr);
            e.attr_path = vec![machine.to_owned()];
            EvalOutcome::Error(e)
        }
    }
}

/// Per-run bookkeeping for the output lines.
struct Collector<'a> {
    requested: BTreeSet<&'a str>,
    delivered: BTreeSet<String>,
    notes: usize,
    tx: &'a UnboundedSender<EvalItem>,
}

impl Collector<'_> {
    fn note(&mut self, text: String) {
        if self.notes < MAX_LINE_NOTES {
            self.notes += 1;
            let _ = self.tx.send(EvalItem::Note(text));
        }
    }

    fn line(&mut self, line: Line) {
        let bytes = match line {
            Line::Full(b) => b,
            Line::TooLong => {
                self.note(format!(
                    "nix-eval-jobs printed a line longer than {EVAL_LINE_CAP} bytes; ignored"
                ));
                return;
            }
        };
        let text = lossy(&bytes);
        if text.trim().is_empty() {
            return;
        }
        let outcome = match parse_eval_line(&text) {
            Ok(o) => o,
            Err(e) => {
                self.note(format!("nix-eval-jobs printed an unusable line: {e}"));
                return;
            }
        };
        let Some(machine) = machine_of(&outcome, &self.requested) else {
            self.note(format!(
                "nix-eval-jobs printed a result for an attribute nobody asked for: {}",
                crate::text::last_line(outcome.attr()).unwrap_or_default()
            ));
            return;
        };
        if !self.delivered.insert(machine.clone()) {
            self.note(format!(
                "nix-eval-jobs printed a second result for {machine}; ignored"
            ));
            return;
        }
        let outcome = named(outcome, &machine);
        let _ = self.tx.send(EvalItem::Machine {
            machine,
            outcome,
            from_cache: false,
        });
    }
}

/// The run error of an evaluator whose output was not read to its end.
const UNFINISHED: &str = "nix-eval-jobs exited, but its output was not read to the end \
                          (something still held it open); machines without a result were \
                          not evaluated";

enum Stop {
    Exited(ExitStatus),
    /// Exited 0, but its stdout was not read to the end: a process outside its group still
    /// held it open at the deadline, or reading it failed.
    Unfinished,
    TimedOut,
    Cancelled,
    WaitFailed(std::io::Error),
}

/// Evaluates machines' toplevels with `nix-eval-jobs`. Serves the core's `Effect::Evaluate`
/// (behind [`crate::cache::evaluate_cached`], which the shell should call instead).
///
/// Each result is sent on `tx` as an [`EvalItem::Machine`] the moment its line arrives, and
/// the stream ends with exactly one [`EvalItem::Finished`]. One machine failing never stops
/// the others: that is what nix-eval-jobs is for. A machine with no line at all (the
/// evaluator crashed first) simply has no item; the core's `Finished` rule covers it.
///
/// Duplicate names are evaluated once. An empty list sends `Finished { error: None }`
/// without running anything.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired or `tx`'s receiver was dropped; the process group
/// (nix-eval-jobs and its workers) has been killed and reaped, and no `Finished` is sent.
pub async fn evaluate(
    flake: &FlakeRef,
    machines: &[EvalMachine],
    params: &EvalParams<'_>,
    cancel: &CancellationToken,
    tx: UnboundedSender<EvalItem>,
) -> Result<(), Cancelled> {
    let mut unique: Vec<EvalMachine> = Vec::with_capacity(machines.len());
    for m in machines {
        if unique.iter().any(|u| u.name == m.name) {
            continue;
        }
        if let Some(why) = unusable(&m.name) {
            let _ = tx.send(EvalItem::Machine {
                machine: m.name.clone(),
                outcome: refused(&m.name, why),
                from_cache: false,
            });
            continue;
        }
        unique.push(m.clone());
    }
    if unique.is_empty() {
        let _ = tx.send(EvalItem::Finished { error: None });
        return Ok(());
    }
    let dir = params
        .runtime_dir
        .clone()
        .unwrap_or_else(default_runtime_path);
    let list = match prepare_private_dir(&dir, "runtime directory")
        .map_err(std::io::Error::other)
        .and_then(|()| ListFile::create(&dir, &eval_machines_json(&unique)))
    {
        Ok(l) => l,
        Err(e) => {
            let _ = tx.send(EvalItem::Finished {
                error: Some(format!(
                    "cannot write the machine list for nix-eval-jobs: {e}"
                )),
            });
            return Ok(());
        }
    };
    let spawned = command(params.program).and_then(|mut cmd| {
        cmd.args(eval_argv(flake, &list.0, params));
        spawn(cmd, false, OnExit::KillGroup, None)
    });
    let mut sp = match spawned {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(EvalItem::Finished {
                error: Some(spawn_error("cannot evaluate", params.program, &e)),
            });
            return Ok(());
        }
    };

    let mut lines = sp.child.stdout.take().map(|o| Lines::new(o, EVAL_LINE_CAP));
    let mut err_pipe = sp.child.stderr.take();
    let mut err = Tail::new(STDERR_CAP);
    let mut collect = Collector {
        requested: unique.iter().map(|m| m.name.as_str()).collect(),
        delivered: BTreeSet::new(),
        notes: 0,
        tx: &tx,
    };
    let deadline = Instant::now() + params.timeout;
    let stop = pump(
        &mut sp,
        &mut lines,
        &mut collect,
        &mut err,
        &mut err_pipe,
        deadline,
        cancel,
    )
    .await;

    let error = match stop {
        Stop::Cancelled => {
            sp.kill_and_reap().await;
            return Err(Cancelled);
        }
        Stop::Unfinished => Some(UNFINISHED.to_owned()),
        Stop::Exited(s) if s.success() => None,
        Stop::Exited(s) => Some(format!("nix-eval-jobs {}", describe_status(Some(s)))),
        Stop::TimedOut => {
            sp.kill_and_reap().await;
            // Results printed just before the deadline are still in the pipe.
            drain(&mut lines, &mut collect, &mut err, &mut err_pipe).await;
            Some(format!("evaluation timed out after {:?}", params.timeout))
        }
        Stop::WaitFailed(e) => {
            sp.kill_and_reap().await;
            Some(format!("waiting for nix-eval-jobs failed: {e}"))
        }
    };
    drop(list);
    let error = error.map(|what| match reason(&lossy(&err.buf)) {
        Some(r) => format!("{what}: {r}"),
        None => what,
    });
    let _ = tx.send(EvalItem::Finished { error });
    Ok(())
}

/// Reads the evaluator's output until it exits and its pipes are drained, the deadline
/// passes, or the call is cancelled. On exit the rest of the group has already been killed
/// ([`Spawned::exited`]), so what is left is at most a pipe buffer: stdout is read until it
/// ends or the deadline (at least [`PIPE_GRACE`] after the exit), stderr for
/// [`PIPE_GRACE`]. A stdout that did not end is [`Stop::Unfinished`], never a complete
/// run.
async fn pump<R: tokio::io::AsyncRead + Unpin>(
    sp: &mut Spawned,
    lines: &mut Option<Lines<R>>,
    collect: &mut Collector<'_>,
    err: &mut Tail,
    err_pipe: &mut Option<tokio::process::ChildStderr>,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Stop {
    let mut grace: Option<(Instant, Instant)> = None;
    let mut exited: Option<ExitStatus> = None;
    let finish = |s: ExitStatus, lines: &Option<Lines<R>>| {
        // A failed exit is reported as such; an unfinished output adds nothing to it.
        if !s.success() || lines.as_ref().is_none_or(|l| l.eof) {
            Stop::Exited(s)
        } else {
            Stop::Unfinished
        }
    };
    loop {
        let out_done = lines.as_ref().is_none_or(|l| l.done);
        if let Some(s) = exited
            && out_done
            && err.done
        {
            break finish(s, lines);
        }
        let (grace_at, limit_at) = grace.unwrap_or((deadline, deadline));
        tokio::select! {
            biased;
            () = cancel.cancelled() => break Stop::Cancelled,
            () = collect.tx.closed() => break Stop::Cancelled,
            () = sleep_until(deadline), if exited.is_none() => break Stop::TimedOut,
            status = sp.exited(), if exited.is_none() => match status {
                Ok(s) => {
                    exited = Some(s);
                    let at = Instant::now() + PIPE_GRACE;
                    grace = Some((at, at.max(deadline)));
                }
                Err(e) => break Stop::WaitFailed(e),
            },
            () = sleep_until(limit_at), if grace.is_some() => {
                if let Some(s) = exited {
                    break finish(s, lines);
                }
            }
            // Past the grace, only an unfinished stdout is waited for.
            () = sleep_until(grace_at), if grace.is_some() && out_done => {
                if let Some(s) = exited {
                    break finish(s, lines);
                }
            }
            line = next_line(lines), if !out_done => {
                if let Some(l) = line {
                    collect.line(l);
                }
            }
            () = err.step_opt(err_pipe), if !err.done => {}
        }
    }
}

async fn next_line<R: tokio::io::AsyncRead + Unpin>(lines: &mut Option<Lines<R>>) -> Option<Line> {
    match lines {
        Some(l) => l.next().await,
        None => None,
    }
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(
    lines: &mut Option<Lines<R>>,
    collect: &mut Collector<'_>,
    err: &mut Tail,
    err_pipe: &mut Option<tokio::process::ChildStderr>,
) {
    let grace = tokio::time::sleep(PIPE_GRACE);
    tokio::pin!(grace);
    loop {
        let out_done = lines.as_ref().is_none_or(|l| l.done);
        if out_done && err.done {
            return;
        }
        tokio::select! {
            biased;
            () = &mut grace => return,
            line = next_line(lines), if !out_done => {
                if let Some(l) = line {
                    collect.line(l);
                }
            }
            () = err.step_opt(err_pipe), if !err.done => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_references_are_shallow_safe_only_for_local_git() {
        let s = |r: &str| FlakeRef::new(r).unwrap().shallow_safe();
        assert_eq!(s("git+file:///x"), "git+file:///x?shallow=1");
        // Merged into the query the reference already has, not appended as a second one.
        assert_eq!(s("git+file:///x?dir=a"), "git+file:///x?dir=a&shallow=1");
        // The operator's own choice stays.
        assert_eq!(
            s("git+file:///x?shallow=0&dir=a"),
            "git+file:///x?shallow=0&dir=a"
        );
        for other in ["path:/x", "github:o/r", "git+https://h/r"] {
            assert_eq!(s(other), other);
        }
    }

    #[test]
    fn flake_refs() {
        assert!(FlakeRef::new("-x").is_err());
        assert!(FlakeRef::new("--impure").is_err());
        assert!(FlakeRef::new("path:/x#evil").is_err());
        assert!(FlakeRef::new("").is_err());
        assert!(FlakeRef::new("/a\nb").is_err());
        assert_eq!(FlakeRef::new("path:/x").unwrap().as_str(), "path:/x");
        assert_eq!(
            FlakeRef::new("git+file:///x?dir=a").unwrap().as_str(),
            "git+file:///x?dir=a"
        );
        // A query the operator wrote is kept, and disables caching.
        let q = FlakeRef::new("git+file:///x?dir=a").unwrap();
        assert!(q.has_query() && !q.is_git());
        assert!(!FlakeRef::new("github:o/r").unwrap().is_git());
    }

    /// Review finding B1: a plain path is never handed to `builtins.getFlake` as a plain
    /// path (which nix reads as `path:`, untracked files and `.git/` included). Inside a
    /// repository it becomes `git+file://<top>` with `?dir=` for a nested flake; outside
    /// one, an explicit `path:` that is never cached.
    #[test]
    fn plain_paths_become_explicit_references() {
        let tmp = tempfile::tempdir().unwrap();
        let top = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(top.join(".git")).unwrap();
        let nested = top.join("fleet a+b");
        std::fs::create_dir(&nested).unwrap();

        let root = FlakeRef::new(top.to_str().unwrap()).unwrap();
        assert_eq!(root.as_str(), format!("git+file://{}/", top.display()));
        assert!(root.is_git() && !root.has_query());
        assert_eq!(root.repo_top(), Some(top.as_path()));
        assert_eq!(root.worktree_dir(), Some(top.as_path()));

        let sub = FlakeRef::new(nested.to_str().unwrap()).unwrap();
        assert_eq!(
            sub.as_str(),
            format!("git+file://{}/?dir=fleet%20a%2Bb", top.display())
        );
        assert!(
            sub.is_git(),
            "the crate's own dir= is not the operator's query"
        );
        assert_eq!(sub.worktree_dir(), Some(nested.as_path()));

        // Through a symlink: the canonical repository, the same reference.
        let link = top.join("link");
        std::os::unix::fs::symlink(&nested, &link).unwrap();
        assert_eq!(FlakeRef::new(link.to_str().unwrap()).unwrap(), sub);

        // Outside any repository: `path:`, never cached.
        let other = tempfile::tempdir().unwrap();
        let plain = FlakeRef::new(other.path().to_str().unwrap()).unwrap();
        assert!(plain.as_str().starts_with("path:/"), "{plain:?}");
        assert!(!plain.is_git() && plain.repo_top().is_none());

        // A relative path is made absolute against the current directory first.
        let rel = FlakeRef::new("some/dir").unwrap();
        let cwd = std::fs::canonicalize(".").unwrap();
        assert!(
            rel.as_str().starts_with("git+file:///") || rel.as_str().starts_with("path:/"),
            "{rel:?}"
        );
        assert!(
            rel.as_str().contains("some%2Fdir") || rel.as_str().ends_with("/some/dir"),
            "{rel:?} under {}",
            cwd.display()
        );
    }

    /// Review item T1 (mutation X01): a `.git` *file* (a linked worktree or a submodule)
    /// makes its directory the top, even inside another checkout.
    #[test]
    fn a_git_file_marks_its_own_top() {
        let tmp = tempfile::tempdir().unwrap();
        let top = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(top.join(".git")).unwrap();
        let wt = top.join("nested-wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../.git/worktrees/nested-wt\n").unwrap();
        let f = FlakeRef::new(wt.to_str().unwrap()).unwrap();
        assert_eq!(f.as_str(), format!("git+file://{}/", wt.display()));
        assert_eq!(f.repo_top(), Some(wt.as_path()));
    }

    /// Review item T2 (mutation X18): a `path:` reference is percent-encoded, so a
    /// directory named like a query names that directory, not a `dir=` inside another.
    #[test]
    fn a_path_reference_encodes_query_syntax() {
        let tmp = tempfile::tempdir().unwrap();
        let q = tmp.path().join("q?dir=sub");
        std::fs::create_dir(&q).unwrap();
        let f = FlakeRef::new(q.to_str().unwrap()).unwrap();
        assert!(f.as_str().ends_with("/q%3Fdir=sub"), "{f:?}");
    }

    #[test]
    fn the_machine_is_the_attr_path_not_the_quoted_attr() {
        let line = r#"{"attr":"\"1odd\"","attrPath":["1odd"],"error":"error: x"}"#;
        let o = parse_eval_line(line).unwrap();
        let req: BTreeSet<&str> = ["1odd"].into();
        assert_eq!(machine_of(&o, &req).as_deref(), Some("1odd"));
        let other: BTreeSet<&str> = ["terra"].into();
        assert_eq!(machine_of(&o, &other), None);
        let named = named(o, "1odd");
        assert_eq!(named.attr(), "1odd");
        assert_eq!(named.machine(), Some("1odd"));
        // Without an attrPath a line names no machine at all.
        assert!(parse_eval_line(r#"{"attr":"1odd","error":"error: x"}"#).is_err());
    }

    #[test]
    fn eval_argv_passes_names_as_data() {
        let flake = FlakeRef::new("path:/f").unwrap();
        let nasty = r#"x"; builtins.abort "pwned"#;
        let ms = [EvalMachine {
            name: nasty.to_owned(),
            class: MachineClass::Darwin,
        }];
        let argv = eval_argv(&flake, Path::new("/tmp/list.json"), &EvalParams::new());
        // The expression is the constant, and no name is anywhere in argv.
        assert_eq!(argv.last().unwrap(), EVAL_EXPR);
        assert!(argv.iter().all(|a| !a.to_string_lossy().contains("pwned")));
        let i = argv.iter().position(|a| a == "--arg-from-file").unwrap();
        assert_eq!(argv[i + 1], "machines");
        assert_eq!(argv[i + 2], "/tmp/list.json");
        let list: serde_json::Value = serde_json::from_str(&eval_machines_json(&ms)).unwrap();
        assert_eq!(list[0]["name"], nasty);
        assert_eq!(list[0]["class"], "darwin");
        let i = argv.iter().position(|a| a == "flake").unwrap();
        assert_eq!(argv[i + 1], "path:/f");
    }
}
