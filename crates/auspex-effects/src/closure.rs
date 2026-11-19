//! The two runtime-closure queries behind the package preview.
//!
//! What a machine runs, and what it would run after an update. Both print one store path per
//! line, which the domain reads ([`parse_closure`]) and compares ([`auspex_domain::diff::diff_closures`]); this
//! module only obtains the text.
//!
//! - [`deployed`]: `nix-store --query --requisites /run/current-system` **on the machine**,
//!   over the control master the probe opened. Same ssh argv as the probe ([`probe_argv`]),
//!   same approval phase when no master is up, so a warm master costs no new prompt. The
//!   query itself is the domain's [`DEPLOYED_QUERY_SCRIPT`] on stdin to `/bin/sh -s`: the
//!   probe's mechanism, and nothing from a target, a name or the config reaches the remote
//!   side.
//! - [`pending`]: locally, through the process supervisor. When the pending toplevel is
//!   valid in the local store, `nix-store --query --requisites <path>` (no network, one
//!   process); otherwise `nix path-info --recursive --store <cache> <path>`, which reads the
//!   cache's narinfos and writes nothing to the local store.
//!
//! Every answer is a value: a closure, or one line of why not. The output of each query is
//! capped ([`STDOUT_CAP`]) and so is its time; a timeout is an error here, not a
//! cancellation (the shell cancels by token, and a cancelled call returns `Err(Cancelled)`
//! after its process group has been killed and reaped).

use std::ffi::{OsStr, OsString};
use std::time::Duration;

use auspex_domain::closure::{ClosurePaths, DEPLOYED_QUERY_SCRIPT, parse_closure};
use auspex_domain::fleet::Target;
use auspex_domain::observe::{ProbeOutcome, StorePath};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::http::base_url;
use crate::process::{End, Limits, OnExit, Ran, command, describe_status, run, spawn_error};
use crate::ssh::{ProbeParams, connect, probe_argv};
use crate::text::{lossy, reason};

/// How much stdout a closure query keeps: 4 MiB.
///
/// A NixOS closure is about 3,500 paths and 350 KB, so this is ten times what a real one
/// needs; past it the query is killed and reported as too large rather than compared from a
/// prefix.
pub const STDOUT_CAP: usize = 4 * 1024 * 1024;

/// How much of stderr's tail a closure query keeps.
pub const STDERR_CAP: usize = 16 * 1024;

/// A closure, or why it could not be had (one line, sanitised by the domain when it is shown).
pub type ClosureAnswer = Result<ClosurePaths, String>;

/// Everything a pending-closure query needs besides the path.
#[derive(Debug, Clone)]
pub struct PendingParams<'a> {
    /// `nix`, for `path-info`.
    pub nix: &'a OsStr,
    /// `nix-store`, for the local query.
    pub nix_store: &'a OsStr,
    /// The binary cache's URL, or `None` when none is configured.
    pub cache_url: Option<&'a str>,
    /// One budget for the local attempt and the cache query together.
    pub timeout: Duration,
}

/// The exact argv (after the program) of the local query.
///
/// `path` is a validated [`StorePath`], so it starts with `/nix/store/` and cannot be read
/// as an option.
#[must_use]
pub fn local_argv(path: &StorePath) -> Vec<String> {
    vec![
        "--query".to_owned(),
        "--requisites".to_owned(),
        path.as_str().to_owned(),
    ]
}

/// The exact argv (after the program) of the cache query, for the already validated
/// `cache_url` ([`base_url`]'s output).
#[must_use]
pub fn cache_argv(cache_url: &str, path: &StorePath) -> Vec<String> {
    vec![
        "path-info".to_owned(),
        "--recursive".to_owned(),
        // Nix remembers a narinfo it did not find for an hour (a negative entry in the user's
        // `~/.cache/nix`). A toplevel whose references are still being uploaded would then be
        // skipped for that user's later operations against this cache; 0 turns that off.
        "--option".to_owned(),
        "narinfo-cache-negative-ttl".to_owned(),
        "0".to_owned(),
        "--store".to_owned(),
        cache_url.to_owned(),
        "--".to_owned(),
        path.as_str().to_owned(),
    ]
}

const fn limits() -> Limits {
    Limits {
        stdout: STDOUT_CAP,
        stderr: STDERR_CAP,
        memory: None,
    }
}

/// Turns a finished run into the closure it printed, or one line saying why not. `what` names
/// the query ("deployed closure query"); `timeout` is the budget it ran under.
///
/// A run that exited 0 but printed no store path at all is an error too: a closure always
/// contains its own root, so an empty one is a broken query, and comparing against it would
/// show every package as added.
fn closure_of(ran: &Ran, what: &str, timeout: Duration) -> Result<ClosurePaths, Failure> {
    let stderr = lossy(&ran.stderr);
    let with_stderr = |text: String| match reason(&stderr) {
        Some(r) => format!("{text}: {r}"),
        None => text,
    };
    match &ran.end {
        End::Cancelled => Err(Failure::Cancelled),
        End::TimedOut => Err(Failure::Why(with_stderr(format!(
            "{what} timed out after {timeout:?}"
        )))),
        End::Overflow => Err(Failure::Why(format!(
            "{what} printed more than {} MiB; not compared",
            STDOUT_CAP / (1024 * 1024)
        ))),
        End::Exited(status) if !status.success() => Err(Failure::Why(with_stderr(format!(
            "{what} {}",
            describe_status(Some(*status))
        )))),
        End::Exited(_) => match ran.whole_stdout() {
            Ok(out) => {
                let closure = parse_closure(&lossy(out));
                if closure.paths.is_empty() {
                    Err(Failure::Why(with_stderr(format!(
                        "{what} printed no store paths ({} lines dropped)",
                        closure.dropped
                    ))))
                } else {
                    Ok(closure)
                }
            }
            Err(true) => Err(Failure::Why(format!(
                "{what} printed more than {} MiB; not compared",
                STDOUT_CAP / (1024 * 1024)
            ))),
            Err(false) => Err(Failure::Why(format!(
                "{what} exited, but its output was not read to the end \
                 (something still held it open)"
            ))),
        },
    }
}

enum Failure {
    Cancelled,
    Why(String),
}

impl Failure {
    /// `Err(Cancelled)` for a cancellation, the line otherwise.
    fn answer(self) -> Result<ClosureAnswer, Cancelled> {
        match self {
            Self::Cancelled => Err(Cancelled),
            Self::Why(line) => Ok(Err(line)),
        }
    }
}

/// What a machine runs: its `/run/current-system` closure. Serves the core's
/// `Effect::DeployedClosure`.
///
/// Authenticates like a probe ([`ProbeParams::approval_deadline`], [`ProbeParams::on_approval`]):
/// with a master up, `ssh -O check` succeeds and nothing prompts; with none, the same
/// separately bounded `true` runs first, and concurrent calls for one destination share it.
/// Then `ssh <probe argv>` runs the domain's [`DEPLOYED_QUERY_SCRIPT`] on stdin, for at most
/// [`ProbeParams::deadline`], with stdout capped at [`STDOUT_CAP`].
///
/// Lines that are not store paths are dropped and counted ([`ClosurePaths::dropped`]).
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; ssh's process group has been killed and reaped.
pub async fn deployed(
    target: &Target,
    params: &ProbeParams<'_>,
    cancel: &CancellationToken,
) -> Result<ClosureAnswer, Cancelled> {
    const WHAT: &str = "deployed closure query";
    if let Some(outcome) = connect(target, params, cancel).await? {
        return Ok(Err(match outcome {
            ProbeOutcome::Unreachable { reason } | ProbeOutcome::Failed { reason } => {
                format!("deployed closure: {reason}")
            }
            ProbeOutcome::Observed(_) => "deployed closure: connection check failed".to_owned(),
        }));
    }
    let deadline = Instant::now() + params.deadline;
    let args = match probe_argv(target, params) {
        Ok(a) => a,
        Err(reason) => return Ok(Err(format!("deployed closure: {reason}"))),
    };
    let mut cmd = match command(params.ssh_program) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(spawn_error(
                "cannot query the deployed closure",
                params.ssh_program,
                &e,
            )));
        }
    };
    cmd.args(args.iter().map(OsString::from));
    // As for the probe: no prompt, anywhere.
    cmd.env_remove("SSH_ASKPASS")
        .env("SSH_ASKPASS_REQUIRE", "never");
    // `LeaveGroup`, as for the probe: see `ssh::probe` for why ssh is the one program whose
    // group is not swept when it exits on its own. A cancel, a timeout or a drop still kills
    // the whole group.
    let ran = match run(
        cmd,
        Some(DEPLOYED_QUERY_SCRIPT.as_bytes().to_vec()),
        OnExit::LeaveGroup,
        limits(),
        deadline,
        cancel,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(Err(spawn_error(
                "cannot query the deployed closure",
                params.ssh_program,
                &e,
            )));
        }
    };
    match closure_of(&ran, WHAT, params.deadline) {
        Ok(closure) => Ok(Ok(closure)),
        Err(f) => f.answer(),
    }
}

/// What a machine would run after the update: the closure of `path`, the pending toplevel.
/// Serves the core's `Effect::PendingClosure`.
///
/// 1. **Local store first.** `nix-store --query --requisites <path>` succeeds only when the
///    path is valid locally (a build host that is this machine), costs one process and no
///    network. Any failure of it, including "path is not valid", falls through; it is not
///    an error by itself.
/// 2. **Then the cache.** `nix path-info --recursive --store <cache> <path>`, only when the
///    core said the path is cached and a cache URL is configured. It reads narinfos and
///    writes nothing to the local store.
///
/// Both run under the one `params.timeout` and the one cancellation token. If both fail the
/// answer names both.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; the process group has been killed and reaped.
pub async fn pending(
    path: &StorePath,
    params: &PendingParams<'_>,
    cancel: &CancellationToken,
) -> Result<ClosureAnswer, Cancelled> {
    const WHAT: &str = "pending closure query";
    let deadline = Instant::now() + params.timeout;

    let local = match command(params.nix_store) {
        Ok(mut cmd) => {
            cmd.args(local_argv(path));
            match run(cmd, None, OnExit::KillGroup, limits(), deadline, cancel).await {
                Ok(ran) => match closure_of(&ran, "local store query", params.timeout) {
                    Ok(closure) => return Ok(Ok(closure)),
                    Err(Failure::Cancelled) => return Err(Cancelled),
                    // The one budget is spent: nothing is left for the cache.
                    Err(Failure::Why(why)) if matches!(ran.end, End::TimedOut) => {
                        return Ok(Err(format!("{WHAT}: {why}")));
                    }
                    Err(Failure::Why(why)) if matches!(ran.end, End::Overflow) => {
                        return Ok(Err(format!("{WHAT}: {why}")));
                    }
                    Err(Failure::Why(why)) => why,
                },
                Err(e) => spawn_error("cannot query the local store", params.nix_store, &e),
            }
        }
        Err(e) => spawn_error("cannot query the local store", params.nix_store, &e),
    };

    let Some(cache_url) = params.cache_url else {
        return Ok(Err(format!(
            "{WHAT}: not in the local store ({local}) and no binary cache is configured"
        )));
    };
    let url = match base_url(cache_url) {
        Ok(url) => url,
        Err(e) => return Ok(Err(format!("{WHAT}: the cache URL is unusable: {e}"))),
    };
    let mut cmd = match command(params.nix) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(spawn_error("cannot query the cache", params.nix, &e)));
        }
    };
    cmd.args(cache_argv(url.as_str(), path));
    let ran = match run(cmd, None, OnExit::KillGroup, limits(), deadline, cancel).await {
        Ok(r) => r,
        Err(e) => return Ok(Err(spawn_error("cannot query the cache", params.nix, &e))),
    };
    match closure_of(&ran, "cache query", params.timeout) {
        Ok(closure) => Ok(Ok(closure)),
        Err(Failure::Cancelled) => Err(Cancelled),
        Err(Failure::Why(why)) => Ok(Err(format!(
            "{WHAT}: not in the local store ({local}); {why}"
        ))),
    }
}
