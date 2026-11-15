//! The machine list and the evaluation.
//!
//! **Names and paths are data.** Nothing here builds Nix source text from a value:
//!
//! - The machine list is `nix eval --json --impure --no-write-lock-file --
//!   <flake>#clanInternals.inventoryClass.inventory.machines`: the flake reference is one
//!   argv element after `--`, the attribute path is a constant, and nix parses both itself.
//! - The evaluation runs a constant expression, [`EVAL_EXPR`], a function of two strings:
//!   `flake` (the flake reference, `--argstr`) and `machines` (a JSON list of
//!   `{name, class}` written by `serde_json` into a private temporary file, read with
//!   `--arg-from-file`, so a large fleet is not limited by the 128 KiB a single argument
//!   may hold). A name nix would
//!   refuse in the list as a whole (a NUL) gets its own error and is left out. The expression selects each configuration
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
pub struct FlakeRef(String);

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
    /// A flake reference: either a URL-like reference (`path:/abs`, `git+file:///abs`,
    /// `github:o/r`) or a filesystem path, which is made absolute (against the current
    /// directory), because `builtins.getFlake` refuses relative paths.
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
        if has_scheme(s) {
            return Ok(Self(s.to_owned()));
        }
        let abs = std::path::absolute(Path::new(s))
            .map_err(|e| FlakeRefError(format!("cannot make {s:?} absolute: {e}")))?;
        let abs = abs
            .to_str()
            .ok_or_else(|| FlakeRefError(format!("flake path {s:?} is not UTF-8")))?;
        Ok(Self(abs.to_owned()))
    }

    /// The reference as nix will see it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether nix will read this through git: a plain path (nix uses git when it is in a
    /// repository) or `git+file:`. Only these are cached: any other reference either sees
    /// files whose contents the key does not cover (`path:` reads untracked and ignored
    /// files) or is not the flake directory at all (`github:`).
    #[must_use]
    pub fn is_git(&self) -> bool {
        !has_scheme(&self.0) || self.0.starts_with("git+file:")
    }

    /// The scheme (`path`, `github`), or `path` for a plain path, for messages.
    #[must_use]
    pub fn scheme(&self) -> &str {
        if has_scheme(&self.0) {
            self.0.split_once(':').map_or("", |(s, _)| s)
        } else {
            "plain path"
        }
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
        End::Exited(_) => parse_inventory(&lossy(&ran.stdout)).map_err(|e| e.to_string()),
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

impl ListFile {
    /// Creates it in the system temporary directory, exclusively (`O_EXCL`, which also
    /// refuses a planted symlink) and readable only by us. Only names and classes are in it.
    fn create(contents: &str) -> std::io::Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let mut last = None;
        for _ in 0..8 {
            let path = std::env::temp_dir().join(format!(
                "auspex-machines-{}-{}-{nanos}.json",
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
        Err(last.unwrap_or_else(|| std::io::Error::other("no temporary file")))
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

enum Stop {
    Exited(ExitStatus),
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
    let list = match ListFile::create(&eval_machines_json(&unique)) {
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
        spawn(cmd, false, OnExit::KillGroup)
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

/// Reads the evaluator's output until it exits and its pipes
/// are drained (or [`PIPE_GRACE`] after exit), the deadline passes, or the call is
/// cancelled. On exit the rest of the group has already been killed ([`Spawned::exited`]).
async fn pump<R: tokio::io::AsyncRead + Unpin>(
    sp: &mut Spawned,
    lines: &mut Option<Lines<R>>,
    collect: &mut Collector<'_>,
    err: &mut Tail,
    err_pipe: &mut Option<tokio::process::ChildStderr>,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Stop {
    let mut grace: Option<Instant> = None;
    let mut exited: Option<ExitStatus> = None;
    loop {
        let out_done = lines.as_ref().is_none_or(|l| l.done);
        if let Some(s) = exited
            && out_done
            && err.done
        {
            break Stop::Exited(s);
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => break Stop::Cancelled,
            () = collect.tx.closed() => break Stop::Cancelled,
            () = sleep_until(deadline), if exited.is_none() => break Stop::TimedOut,
            status = sp.exited(), if exited.is_none() => match status {
                Ok(s) => {
                    exited = Some(s);
                    grace = Some(Instant::now() + PIPE_GRACE);
                }
                Err(e) => break Stop::WaitFailed(e),
            },
            () = sleep_until(grace.unwrap_or(deadline)), if grace.is_some() => {
                if let Some(s) = exited {
                    break Stop::Exited(s);
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
        let rel = FlakeRef::new("some/dir").unwrap();
        assert!(rel.as_str().starts_with('/'), "{rel:?}");
        assert!(rel.as_str().ends_with("/some/dir"));
        // A relative path that merely starts with a dash-free name but has a colon later
        // is not a scheme unless the part before the colon is one.
        assert!(FlakeRef::new("./a:b").unwrap().as_str().starts_with('/'));
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
