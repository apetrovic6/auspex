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

/// The default `nix-store` program, looked up on `PATH`: the local closure query.
pub const DEFAULT_NIX_STORE: &str = "nix-store";

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
    /// **This does filesystem I/O**: it canonicalizes the path and looks for `.git` in each
    /// ancestor (a file or a directory: a linked worktree or a submodule has a file), so the
    /// result depends on the disk as it is now, and a path that does not exist keeps its
    /// lexical form.
    ///
    /// **A `git+file:` reference is rewritten too** when it has no query: it goes through
    /// the same walk-up, so `git+file:///repo/x/y` becomes `git+file:///repo/?dir=x%2Fy`
    /// (nix itself refuses the original, "Failed to fetch git repository"), and
    /// `git+file://localhost/…` and `git+file:/…` are normalised alike (review of PR #8, N9).
    /// Only a reference the operator gave a query (`?ref=`, `?rev=`, `?dir=`, …) is used as
    /// written, and is not cached; other schemes (`github:`, `git+https:`) are untouched.
    ///
    /// The path is percent-encoded for nix's reference grammar, `[`, `]`, `|` and `^` included.
    ///
    /// # Errors
    ///
    /// When it is empty, starts with `-`, contains `#`, or contains a control character;
    /// when a path that must be put in the reference is not UTF-8, or has a character that
    /// cannot be encoded (nothing is passed to nix that it would read as something else); when
    /// the working directory cannot be found or a path cannot be made absolute.
    pub fn new(s: &str) -> Result<Self, FlakeRefError> {
        let cwd = std::env::current_dir()
            .map_err(|e| FlakeRefError(format!("cannot find the working directory: {e}")))?;
        Self::new_in(s, &cwd)
    }

    /// As [`Self::new`], with relative paths taken from `cwd` instead of the process's working
    /// directory.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn new_in(s: &str, cwd: &Path) -> Result<Self, FlakeRefError> {
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
        let absolute = absolute_local_scheme(s, cwd)?;
        let s = absolute.as_str();
        let queried = has_scheme(s) && s.contains('?');
        let local = if let Some(file) = s.strip_prefix("git+") {
            (!queried)
                .then(|| url::Url::parse(file).ok()?.to_file_path().ok())
                .flatten()
        } else if has_scheme(s) {
            None
        } else {
            Some(
                std::path::absolute(cwd.join(s))
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
                let mut reference = format!("git+file://{}", nix_path(file.path())?);
                if let Some(query) = file.query() {
                    reference.push('?');
                    reference.push_str(query);
                }
                return Ok(Self {
                    reference,
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
                    reference: format!("path:{}", nix_path(file.path())?),
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

    /// Whether this names something on this machine (`path:` or `git+file:`), as opposed to a
    /// remote flake.
    pub(crate) fn is_local(&self) -> bool {
        self.reference.starts_with("path:") || self.reference.starts_with("git+file:")
    }

    /// The flake's directory on this machine, when it has one: the worktree of a git
    /// reference, or the path of a `path:` reference. What `clan` is run from and given as
    /// `--flake`, since it takes a directory, not a normalised `git+file://` URL.
    ///
    /// `None` for a reference the operator wrote with a query (`?dir=`, `?ref=`, anything):
    /// the directory it names is not the path before the `?` (`path:/x?dir=sub` is `/x/sub`),
    /// and guessing would run clan against the wrong flake. See [`Self::has_query`].
    pub(crate) fn local_dir(&self) -> Option<std::path::PathBuf> {
        if self.queried {
            return None;
        }
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

/// `path:` and `git+file:` references that name a directory relative to `cwd` (`path:./fleet`,
/// `path:fleet`, `git+file:fleet`) made absolute, the way a plain path already is.
///
/// Left alone they are not a directory auspex can find: nix resolves them against its own
/// working directory, which is neither where `clan` would be run from nor where the lock file
/// is looked for. Anything else comes back unchanged. The path is taken as written (not
/// percent-decoded) and the query, if any, is kept.
fn absolute_local_scheme(s: &str, cwd: &Path) -> Result<String, FlakeRefError> {
    let (git, rest) = if let Some(rest) = s.strip_prefix("path:") {
        (false, rest)
    } else if let Some(rest) = s.strip_prefix("git+file:") {
        // `git+file://…` has an authority (a host, normally empty) and is a URL already.
        if rest.starts_with("//") {
            return Ok(s.to_owned());
        }
        (true, rest)
    } else {
        return Ok(s.to_owned());
    };
    let (path, query) = rest
        .split_once('?')
        .map_or((rest, None), |(p, q)| (p, Some(q)));
    if !git && path.starts_with('/') {
        return Ok(s.to_owned());
    }
    if path.is_empty() {
        return Err(FlakeRefError(format!(
            "flake reference {s:?} names no path"
        )));
    }
    let abs = std::path::absolute(cwd.join(path))
        .map_err(|e| FlakeRefError(format!("cannot make {s:?} absolute: {e}")))?;
    let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
    let file = url::Url::from_file_path(&abs)
        .map_err(|()| FlakeRefError(format!("cannot encode the path of {s:?}")))?;
    let path = nix_path(file.path())?;
    let mut out = if git {
        format!("git+file://{path}")
    } else {
        format!("path:{path}")
    };
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    Ok(out)
}

/// The path part of a `file:` URL (`Url::path`, already percent-encoded by the `url` crate) as
/// nix will accept it, or an error.
///
/// nix's flake reference parser takes only RFC 3986 path characters, unencoded or as `%XX`.
/// The `url` crate follows the WHATWG URL standard instead, which leaves `[`, `]`, `|` and
/// `^` unencoded in a path, and nix then refuses the whole reference ("is not an absolute
/// path", review of PR #8, N3), so a repository in `~/t[1]` could be neither listed nor
/// evaluated. They are encoded here. Everything else is checked against the grammar, so a
/// character nobody thought of fails closed with this error instead of reaching nix as a
/// reference that names something else, or nothing.
fn nix_path(path: &str) -> Result<String, FlakeRefError> {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            '[' => out.push_str("%5B"),
            ']' => out.push_str("%5D"),
            '|' => out.push_str("%7C"),
            '^' => out.push_str("%5E"),
            // An escape the `url` crate wrote: two hex digits must follow.
            '%' => {
                let rest = chars.as_str().as_bytes();
                if rest.len() < 2 || !rest[..2].iter().all(u8::is_ascii_hexdigit) {
                    return Err(unencodable(path));
                }
                out.push('%');
            }
            c if c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '/' | '-'
                        | '.'
                        | '_'
                        | '~'
                        | '!'
                        | '$'
                        | '&'
                        | '\''
                        | '('
                        | ')'
                        | '*'
                        | '+'
                        | ','
                        | ';'
                        | '='
                        | ':'
                        | '@'
                ) =>
            {
                out.push(c);
            }
            _ => return Err(unencodable(path)),
        }
    }
    Ok(out)
}

fn unencodable(path: &str) -> FlakeRefError {
    FlakeRefError(format!(
        "cannot encode the path {path:?} as a flake reference: it has a character that nix's \
         reference syntax does not take"
    ))
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
    /// The git the untracked-file note asks which files are tracked (`git ls-files`). Set from
    /// the cache parameters by [`crate::cache::evaluate_cached`], so the operator's own git
    /// program is used, as for the key.
    pub git: crate::cache::GitParams<'a>,
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
            git: crate::cache::GitParams::new(),
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
    /// The repository top level of a git reference: where an untracked file that nix said
    /// it could not find would be.
    top: Option<&'a Path>,
    /// Paths already hinted about, so one missing module is one note, not one per machine.
    hinted: BTreeSet<String>,
    /// (machine, path) pairs to check against the index once the run is over.
    candidates: Vec<(String, String)>,
    /// The git for the check, and the address-space limit it runs under ([`crate::cache::hint_memory`]).
    git: crate::cache::GitParams<'a>,
    git_memory: Option<u64>,
}

impl<'a> Collector<'a> {
    /// The bookkeeping for one evaluation of `machines` of `flake`.
    fn new(
        machines: &'a [EvalMachine],
        tx: &'a UnboundedSender<EvalItem>,
        flake: &'a FlakeRef,
        params: &EvalParams<'a>,
    ) -> Self {
        Self {
            requested: machines.iter().map(|m| m.name.as_str()).collect(),
            delivered: BTreeSet::new(),
            notes: 0,
            tx,
            top: flake.repo_top(),
            hinted: BTreeSet::new(),
            candidates: Vec::new(),
            git: params.git,
            git_memory: crate::cache::hint_memory(cfg!(target_os = "linux")),
        }
    }

    fn note(&mut self, text: String) {
        if self.notes < MAX_LINE_NOTES {
            self.notes += 1;
            let _ = self.tx.send(EvalItem::Note(text));
        }
    }

    /// One note when an evaluation error names a file in the repository that nix could not
    /// find and that exists in the worktree (review of PR #8, N6).
    ///
    /// A plain path in a git repository is evaluated as `git+file`, which reads the tracked
    /// files only, as `nix build .#…` does (review finding B1). A configuration that imports a
    /// new, untracked module then fails with nix's "path '/nix/store/…-source/new.nix' does not
    /// exist", which names neither git nor tracking, and an import guarded by `pathExists`
    /// changes silently. The path after `-source/` is relative to the repository top level.
    fn untracked_hint(&mut self, machine: &str, tail: &[String]) {
        let Some(top) = self.top else { return };
        for rel in tail.iter().flat_map(|l| missing_paths(l)) {
            if self.candidates.len() >= MAX_LINE_NOTES
                || self.hinted.contains(&rel)
                || std::fs::symlink_metadata(top.join(&rel)).is_err()
            {
                continue;
            }
            self.hinted.insert(rel.clone());
            self.candidates.push((machine.to_owned(), rel));
        }
    }

    /// The notes for the candidates [`Self::untracked_hint`] collected, once the run is over:
    /// only for a path that is **not in the index** (`git ls-files` lists nothing for it), since
    /// a tracked file that nix reports on is not an untracked file (review of PR #37, finding 2).
    /// When git cannot answer, there is no note: it is a hint, never a guess.
    async fn explain_untracked(&mut self, cancel: &CancellationToken) -> Result<(), Cancelled> {
        let Some(top) = self.top else { return Ok(()) };

        for (machine, rel) in std::mem::take(&mut self.candidates) {
            // `:(literal)`: the name is data, not a pathspec with wildcards.
            let spec = format!(":(literal){rel}");
            let args = ["ls-files", "-z", "--", spec.as_str()];
            if let Ok(listed) =
                crate::cache::git_hint(&self.git, top, &args, 64 * 1024, self.git_memory, cancel)
                    .await?
                && listed.is_empty()
            {
                self.note(format!(
                    "{machine}: nix could not find {:?}, which exists in the worktree but is \
                     not tracked by git. A plain path in a git repository is evaluated as a \
                     git+file reference, which sees tracked files only (as `nix build .#…` \
                     does): `git add` the file",
                    sanitize_line(&rel)
                ));
            }
        }
        Ok(())
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
        if let EvalOutcome::Error(failure) = &outcome {
            self.untracked_hint(&machine, &failure.error_tail);
        }
        let _ = self.tx.send(EvalItem::Machine {
            machine,
            outcome,
            from_cache: false,
        });
    }
}

/// [`source_paths`] of a line that says the file is missing: nix's `does not exist`, the system's
/// `No such file or directory`, or `is not tracked by Git`. Any other line that quotes a source
/// path (the module system's "- In" line, a trace, a warning) says nothing about a
/// file being absent, and gets nothing (review of PR #37, finding 2).
fn missing_paths(line: &str) -> Vec<String> {
    let lower = line.to_ascii_lowercase();
    if [
        "does not exist",
        "no such file or directory",
        "is not tracked by git",
    ]
    .iter()
    .any(|p| lower.contains(p))
    {
        source_paths(line)
    } else {
        Vec::new()
    }
}

/// The paths below a store path named `...-source` in one line of nix's error text, as written
/// after `-source/` up to a quote or a space. Only a real store path counts (`/nix/store/`
/// and 32 characters), and only a path that stays below its root: no `..`, nothing absolute.
fn source_paths(line: &str) -> Vec<String> {
    const MARK: &str = "-source/";
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = line[from..].find(MARK) {
        let end = from + at;
        from = end + MARK.len();
        let before = &line[..end];
        let hash_ok = before.len() >= 32
            && before.is_char_boundary(before.len() - 32)
            && before[before.len() - 32..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric())
            && before[..before.len() - 32].ends_with("/nix/store/");
        if !hash_ok {
            continue;
        }
        let rest = &line[from..];
        let rel = rest
            .split(|c: char| c == '\'' || c == '"' || c.is_whitespace())
            .next()
            .unwrap_or_default();
        let clean = !rel.is_empty()
            && rel.len() <= 1024
            && Path::new(rel)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
        if clean {
            found.push(rel.to_owned());
        }
    }
    found
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
    let mut collect = Collector::new(&unique, &tx, flake, params);
    let stop = pump(
        &mut sp,
        &mut lines,
        &mut collect,
        &mut err,
        &mut err_pipe,
        Instant::now() + params.timeout,
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
    collect.explain_untracked(cancel).await?;
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
    fn relative_local_scheme_references_are_made_absolute() {
        let base = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(base.path()).unwrap();
        let plain = cwd.join("plain");
        let repo = cwd.join("repo");
        std::fs::create_dir(&plain).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let abs = |p: &Path| format!("path:{}", p.display());
        let new = |s: &str| FlakeRef::new_in(s, &cwd).unwrap();

        // The four forms the review found: with a scheme, and not absolute.
        for form in ["path:./plain", "path:plain"] {
            let f = new(form);
            assert_eq!(f.as_str(), abs(&plain), "{form}");
            assert_eq!(f.local_dir(), Some(plain.clone()), "{form}");
            assert!(!f.has_query(), "{form}");
        }
        for form in [
            "git+file:./repo",
            "git+file:repo",
            "git+file:/abs/not/relative",
        ] {
            let f = new(form);
            if form.ends_with("relative") {
                // Absolute already, with one slash: made a URL, and not a repository.
                assert_eq!(f.as_str(), "git+file:///abs/not/relative", "{form}");
                assert_eq!(f.local_dir(), None, "{form}");
                continue;
            }
            // A git repository: normalised like a plain path inside one.
            assert!(
                f.as_str().starts_with("git+file://"),
                "{form}: {}",
                f.as_str()
            );
            assert_eq!(f.local_dir(), Some(repo.clone()), "{form}");
        }
        // A query is kept, and still a query.
        let q = new("path:./plain?dir=sub");
        assert_eq!(q.as_str(), format!("{}?dir=sub", abs(&plain)));
        assert!(q.has_query() && q.is_local());
        // Already absolute, or not local: untouched.
        assert_eq!(new("path:/x/y").as_str(), "path:/x/y");
        assert_eq!(new("git+file:///x/y").as_str(), "git+file:///x/y");
        assert_eq!(new("git+file://host/x").as_str(), "git+file://host/x");
        assert_eq!(new("github:o/r").as_str(), "github:o/r");
        // And a plain relative path is relative to `cwd`, as it always was.
        assert_eq!(new("plain").as_str(), abs(&plain));
        assert!(FlakeRef::new_in("path:", &cwd).is_err());
    }

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

    /// A repository with `a.nix` tracked and `[a].nix`, `b.nix` untracked, and a `Collector` over
    /// it with those as candidates, run through `explain_untracked` with `git` and `memory`.
    /// Returns the notes.
    fn untracked_notes(git: crate::cache::GitParams<'_>, memory: Option<u64>) -> Vec<String> {
        let repo = tempfile::tempdir().unwrap();
        let top = std::fs::canonicalize(repo.path()).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&top)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(top.join("a.nix"), "{}").unwrap();
        run(&["add", "a.nix"]);
        std::fs::write(top.join("[a].nix"), "{}").unwrap();
        std::fs::write(top.join("b.nix"), "{}").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut collect = Collector {
            requested: BTreeSet::new(),
            delivered: BTreeSet::new(),
            notes: 0,
            tx: &tx,
            top: Some(&top),
            hinted: BTreeSet::new(),
            candidates: ["[a].nix", "a.nix", "b.nix"]
                .map(|p| ("m".to_owned(), p.to_owned()))
                .to_vec(),
            git,
            git_memory: memory,
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(collect.explain_untracked(&CancellationToken::new()))
            .unwrap();
        drop(collect);
        drop(tx);
        let mut notes = Vec::new();
        while let Ok(EvalItem::Note(n)) = rx.try_recv() {
            notes.push(n);
        }
        notes
    }

    /// Review of PR #37, re-review A and mutations Q04 and Q13. The name is data, not a pathspec
    /// (`[a].nix` is not `a.nix`), a tracked file gets no note, and with **no memory limit** (the
    /// platforms without a launcher, macOS) the check still answers: the note is not silently off.
    #[test]
    fn the_untracked_check_reads_names_literally_and_needs_no_launcher() {
        for memory in [None, Some(crate::cache::GIT_MEMORY_LIMIT)] {
            let notes = untracked_notes(crate::cache::GitParams::new(), memory);
            let mentioned = |p: &str| notes.iter().any(|n| n.contains(&format!("{p:?}")));
            assert_eq!(notes.len(), 2, "{memory:?}: {notes:?}");
            assert!(mentioned("[a].nix") && mentioned("b.nix"), "{notes:?}");
            assert!(!mentioned("a.nix"), "a tracked file: {notes:?}");
        }
    }

    /// Mutation Q13: when git cannot answer there is no note, never a guess.
    #[test]
    fn a_git_that_cannot_answer_gives_no_note() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("git");
        std::fs::write(&fake, "#!/bin/sh\necho 'fatal: nope' >&2\nexit 128\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let mut git = crate::cache::GitParams::new();
        git.program = fake.as_os_str();
        assert_eq!(untracked_notes(git, None), Vec::<String>::new());
        let missing = std::ffi::OsString::from("/nonexistent/auspex-git");
        git.program = &missing;
        assert_eq!(untracked_notes(git, None), Vec::<String>::new());
    }

    /// Where a launcher exists the hint is bounded like every other git; elsewhere it is not.
    #[test]
    fn the_hint_is_bounded_where_a_launcher_exists() {
        assert_eq!(
            crate::cache::hint_memory(true),
            Some(crate::cache::GIT_MEMORY_LIMIT)
        );
        assert_eq!(crate::cache::hint_memory(false), None);
    }

    /// Review of PR #37, finding 2 and mutations R14 and R15: which paths nix's error text
    /// names. A path ends at a quote or a space, must be below a real store path, stays below
    /// it, and is at most 1024 bytes; and only a line that says something is missing counts.
    #[test]
    fn source_paths_are_what_nix_names_and_missing_paths_only_when_it_says_so() {
        let store = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source";
        assert_eq!(
            source_paths(&format!("path '{store}/a/b.nix' does not exist")),
            ["a/b.nix"]
        );
        assert_eq!(
            source_paths(&format!("path \"{store}/dq.nix\" does not exist")),
            ["dq.nix"]
        );
        assert_eq!(source_paths(&format!("at {store}/sp.nix :1:1")), ["sp.nix"]);
        assert_eq!(
            source_paths(&format!("{store}/one.nix and {store}/two.nix")),
            ["one.nix", "two.nix"]
        );
        // The length cap: 1024 bytes pass, 1025 do not.
        let long = |n: usize| format!("'{store}/{}'", "x".repeat(n));
        assert_eq!(source_paths(&long(1024)).len(), 1);
        assert_eq!(source_paths(&long(1025)), Vec::<String>::new());
        // Not below a store path, not a hash, escapes, absolute.
        for line in [
            "'/tmp/x-source/a.nix'",
            "'/nix/store/short-source/a.nix'",
            &format!("'{store}/../a.nix'"),
            &format!("'{store}/a/../../b.nix'"),
            &format!("'{store}//a.nix'"),
            &format!("'{store}/'"),
        ] {
            assert!(source_paths(line).is_empty(), "{line}");
        }
        // Only a missing-file line is a candidate.
        for said in [
            format!("error: path '{store}/a.nix' does not exist"),
            format!("error: getting status of '{store}/a.nix': No such file or directory"),
            format!("error: '{store}/a.nix' is not tracked by Git"),
            format!("error: PATH '{store}/a.nix' DOES NOT EXIST"),
        ] {
            assert_eq!(missing_paths(&said), ["a.nix"], "{said}");
        }
        for silent in [
            format!("- In `{store}/a.nix': true"),
            format!("error: The option `x' has conflicting definitions in `{store}/a.nix'"),
            format!("trace: evaluating {store}/a.nix"),
        ] {
            assert!(missing_paths(&silent).is_empty(), "{silent}");
        }
    }

    /// Review item N9, now documented and pinned: an explicit `git+file:` reference without a
    /// query is rewritten like a plain path (nix refuses `git+file:///repo/x/y` itself), and
    /// one with a query is kept as written.
    #[test]
    fn explicit_git_file_references_are_rewritten_unless_they_carry_a_query() {
        let tmp = tempfile::tempdir().unwrap();
        let top = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(top.join(".git")).unwrap();
        std::fs::create_dir_all(top.join("x/y")).unwrap();
        let want = format!("git+file://{}/?dir=x%2Fy", top.display());
        for given in [
            format!("git+file://{}/x/y", top.display()),
            format!("git+file://localhost{}/x/y", top.display()),
            format!("git+file:{}/x/y", top.display()),
        ] {
            let f = FlakeRef::new(&given).unwrap();
            assert_eq!(f.as_str(), want, "{given}");
            assert!(f.is_git() && !f.has_query(), "{given}");
            assert_eq!(f.worktree_dir(), Some(top.join("x/y").as_path()), "{given}");
        }
        let root = FlakeRef::new(&format!("git+file://{}", top.display())).unwrap();
        assert_eq!(root.as_str(), format!("git+file://{}/", top.display()));
        // The operator's own query is theirs: kept, and not cached.
        let queried = format!("git+file://{}/x/y?ref=main", top.display());
        let q = FlakeRef::new(&queried).unwrap();
        assert_eq!(q.as_str(), queried);
        assert!(q.has_query() && !q.is_git());
    }

    /// What nix's flake reference parser takes in a path: RFC 3986 `pchar`s and `/`, or an
    /// escape. The test's own statement of the grammar, not `nix_path`'s code.
    fn nix_takes(path: &str) -> bool {
        let b = path.as_bytes();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'%' => {
                    if !b
                        .get(i + 1..i + 3)
                        .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
                    {
                        return false;
                    }
                    i += 2;
                }
                c if c.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&c) => {}
                _ => return false,
            }
            i += 1;
        }
        true
    }

    /// Review item N3: `[`, `]`, `|` and `^` in a repository path made nix refuse the
    /// reference ("is not an absolute path"). Every awkward name the review tried names its
    /// own directory in a reference nix takes, for the top level, for `?dir=`, and for
    /// `path:`.
    #[test]
    fn awkward_directory_names_make_references_nix_takes() {
        let names = [
            "t[1]",
            "t]b",
            "t|b",
            "t^b",
            "[|]^",
            "a b",
            "a+b",
            "a%20b",
            "a%2Fb",
            "a%",
            "a?dir=x",
            "a%3Fdir=x",
            "a&dir=x",
            "a;dir=x",
            "a=b",
            "é ü",
            "a\\b",
            "a'b",
            "a\"b",
            "a{b}",
            "a`b",
            "a<b>",
            "a!$*@~,()b",
            "a:b",
        ];
        let base = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(base.path()).unwrap();
        for name in names {
            // A repository named `name`, a nested flake directory named `name`, and a
            // directory outside any repository named `name`.
            let top = base.join("repos").join(name);
            let nested = top.join(name);
            let plain = base.join("plain").join(name);
            std::fs::create_dir_all(top.join(".git")).unwrap();
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::create_dir_all(&plain).unwrap();

            let root = FlakeRef::new(top.to_str().unwrap()).unwrap();
            let file = root.as_str().strip_prefix("git+file://").unwrap();
            assert!(nix_takes(file), "{name}: {}", root.as_str());
            let decoded = url::Url::parse(&format!("file://{file}")).unwrap();
            assert_eq!(decoded.to_file_path().unwrap(), top, "{name}");
            assert!(root.is_git(), "{name}");

            let sub = FlakeRef::new(nested.to_str().unwrap()).unwrap();
            let (path, query) = sub
                .as_str()
                .strip_prefix("git+file://")
                .unwrap()
                .split_once("?dir=")
                .unwrap();
            assert!(
                nix_takes(path) && nix_takes(query),
                "{name}: {}",
                sub.as_str()
            );
            assert_eq!(sub.worktree_dir(), Some(nested.as_path()), "{name}");

            let p = FlakeRef::new(plain.to_str().unwrap()).unwrap();
            let rest = p.as_str().strip_prefix("path:").unwrap();
            assert!(nix_takes(rest), "{name}: {}", p.as_str());
            assert_eq!(p.local_dir(), Some(plain), "{name}");
            // The scheme form goes through the same encoding. (`path:a?dir=x` is a query
            // there by definition, so those names are left out.)
            if !name.contains('?') {
                let rel = FlakeRef::new_in(&format!("path:{name}"), &base.join("plain")).unwrap();
                assert_eq!(rel, p, "{name}");
            }
        }
    }

    /// The four characters from the review, spelled out, so the expected text is in the
    /// tests and not only in the encoder.
    #[test]
    fn brackets_pipes_and_carets_are_percent_encoded() {
        let base = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(base.path()).unwrap();
        let top = base.join("fleet|[1]^");
        std::fs::create_dir_all(top.join(".git")).unwrap();
        let f = FlakeRef::new(top.to_str().unwrap()).unwrap();
        assert_eq!(
            f.as_str(),
            format!("git+file://{}/fleet%7C%5B1%5D%5E/", base.display())
        );
        let plain = base.join("p]b");
        std::fs::create_dir(&plain).unwrap();
        let p = FlakeRef::new(plain.to_str().unwrap()).unwrap();
        assert_eq!(p.as_str(), format!("path:{}/p%5Db", base.display()));
    }

    /// N3's other half: what cannot be encoded is refused, never passed on. The `url` crate
    /// encodes everything outside ASCII, but this is the last check before nix, so it does
    /// not rely on that.
    #[test]
    fn a_path_that_cannot_be_encoded_is_refused() {
        assert_eq!(nix_path("/a/b%20c").unwrap(), "/a/b%20c");
        for bad in [
            "/a b", "/a\u{e9}", "/a%", "/a%2", "/a%zz", "/a\"b", "/a\\b", "/a\u{7f}", "/a\n",
        ] {
            let e = nix_path(bad).unwrap_err();
            assert!(e.0.contains("cannot encode the path"), "{bad:?}: {e}");
        }
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
