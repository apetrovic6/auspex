//! What the checks before an update have to look up: the state of the fleet repository, and
//! the size of the closure the update would copy.
//!
//! Both are read-only and both go through the process supervisor like every other child here
//! (its own session, no terminal, stdin on `/dev/null`, capped output, killed and reaped on
//! cancellation or timeout).
//!
//! - [`tree_state`]: one `git status` ([`auspex_domain::preflight::status_args`]) in the fleet
//!   directory, run exactly as the cache key's git commands are ([`crate::cache`]: the same
//!   address-space limit, `core.fsmonitor=false`, `GIT_OPTIONAL_LOCKS=0`, and the variables
//!   that would point git at another repository removed), plus the hardening that command
//!   needs because, unlike those, it can read file contents. A fleet that is not a local
//!   directory, or not a git repository, is an answer ("cannot be checked"), not a failure.
//!   The state also names `HEAD`'s commit (`rev-parse --verify HEAD`, none for a branch with no
//!   commit yet) and, for a tree with changes to tracked files, carries a fingerprint of what nix
//!   would evaluate ([`worktree_fingerprint`]: the cache key's own bounded worktree digest,
//!   which reads no file through git and runs no filter), so a machine's turn can tell a tree
//!   that was pulled, or edited again, from the one the operator confirmed. A fingerprint that
//!   cannot be had is [`Edits::Unavailable`] with the reason, not a failed check.
//! - [`closure_size`]: the NAR size of every path of the pending toplevel's closure, from
//!   `nix path-info --recursive --size`: against the local store first (valid there when this
//!   machine built it), then against the binary cache. The package preview's pending-closure
//!   query asks the same two places in the same order ([`crate::closure::pending`]); this one
//!   asks for sizes, which that query's output does not carry.
//!
//! Every answer is a value: the result, or one line on why not. A cancelled call returns
//! `Err(Cancelled)` after its process group has been killed and reaped.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use auspex_domain::observe::StorePath;
use auspex_domain::preflight::{
    Edits, LIST_CONFIG_ARGS, TreeState, branch_is_unborn, parse_closure_sizes, parse_commit_id,
    parse_filter_names, parse_tree_status, status_args,
};
use auspex_domain::text::sanitize_line;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::cache::{GitParams, git, worktree_fingerprint};
use crate::closure::{Failure, PendingParams, limits, text_of};
use crate::http::base_url;
use crate::nix::FlakeRef;
use crate::process::{End, OnExit, command, run, spawn_error};

/// The most `git status` output kept: 4 MiB, about 40,000 changed files. Past it the check is
/// "could not be done", not a count from a prefix.
const STATUS_CAP: usize = 4 * 1024 * 1024;

/// The most config-key listing kept: 1 MiB. Past it the check is "could not be done", not a
/// list of filters with some missing.
const CONFIG_CAP: usize = 1024 * 1024;

/// The most `rev-parse HEAD` output kept: a commit id is 65 bytes; anything past this is not one.
const HEAD_CAP: usize = 4096;

/// The fleet repository's state, read from `dir`, in at most `params.timeout` in all.
///
/// That is `timeouts.diff` for the shell: the one budget covers the config listing, the status,
/// `HEAD`'s id and the fingerprint together, and each git command alone is also held to it.
///
/// A look that runs out of time before it has the status and `HEAD` is the inner `Err`; one that
/// runs out of time in the fingerprint keeps what it has and says why the changes are
/// [`Edits::Unavailable`].
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired. Anything else (no git, not a repository, a timeout,
/// output that is not `git status`'s) is the inner `Err` line.
pub async fn tree_state(
    params: &GitParams<'_>,
    dir: &Path,
    cancel: &CancellationToken,
) -> Result<Result<TreeState, String>, Cancelled> {
    let started = Instant::now();
    let budget = params.timeout;
    let child = cancel.child_token();
    let found = within(budget, &child, status_and_head(params, dir, &child), cancel).await?;
    let mut state = match found {
        None => {
            return Ok(Err(format!(
                "looking at the fleet repository timed out after {budget:?}"
            )));
        }
        Some(Err(why)) => return Ok(Err(why)),
        Some(Ok(state)) => state,
    };
    // What a deploy would evaluate, for a tree that has changes: nothing to fingerprint when
    // it has none. The fingerprint is bounded as the cache key's worktree read is, and by what
    // is left of the look's budget; when it cannot be had the state says why instead of failing
    // the whole check, and the turn compares commits only.
    if state.changed_files > 0 {
        let child = cancel.child_token();
        let left = budget.saturating_sub(started.elapsed());
        state.edits = match within(
            left,
            &child,
            worktree_fingerprint(dir, params, &child),
            cancel,
        )
        .await?
        {
            Some(Ok(digest)) => Edits::Digest(digest),
            Some(Err(why)) => Edits::Unavailable(sanitize_line(&why)),
            None => Edits::Unavailable(format!(
                "reading the worktree timed out (the whole look is held to {budget:?})"
            )),
        };
    }
    Ok(Ok(state))
}

/// Runs `work` for at most `budget`: `None` when the time ran out first (after `child`, the token
/// `work` was built with, was cancelled and `work` had killed and reaped its processes).
/// `outer` is the caller's token: when it fires `work` ends with [`Cancelled`], which is
/// returned as such.
async fn within<T>(
    budget: Duration,
    child: &CancellationToken,
    work: impl std::future::Future<Output = Result<T, Cancelled>>,
    outer: &CancellationToken,
) -> Result<Option<T>, Cancelled> {
    tokio::pin!(work);
    tokio::select! {
        r = &mut work => r.map(Some),
        () = tokio::time::sleep(budget) => {
            child.cancel();
            match work.await {
                Ok(done) => Ok(Some(done)),
                Err(Cancelled) if outer.is_cancelled() => Err(Cancelled),
                Err(Cancelled) => Ok(None),
            }
        }
    }
}

/// The config listing, the status and `HEAD`'s id: what the look needs before anything else.
async fn status_and_head(
    params: &GitParams<'_>,
    dir: &Path,
    cancel: &CancellationToken,
) -> Result<Result<TreeState, String>, Cancelled> {
    // First, which filter drivers the repository defines (a read-only listing of config keys,
    // which runs nothing): `status` then runs with each of them emptied, so no filter an
    // attribute names can run on a file it has to read.
    let listing = match git(params, dir, &LIST_CONFIG_ARGS, CONFIG_CAP, cancel).await? {
        Ok(out) => out,
        Err(why) => return Ok(Err(why)),
    };
    // As bytes: a driver name that is not UTF-8 must not be decoded into another name.
    let filters = match parse_filter_names(&listing) {
        Ok(f) => f,
        Err(why) => return Ok(Err(why)),
    };
    let args = status_args(&filters);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = match git(params, dir, &args, STATUS_CAP, cancel).await? {
        Ok(out) => out,
        Err(why) => return Ok(Err(why)),
    };
    let text = String::from_utf8_lossy(&out);
    let mut state = match parse_tree_status(&text) {
        Ok(s) => s,
        Err(why) => return Ok(Err(why)),
    };
    // HEAD's commit id: `status` does not print it. A branch with no commit yet has none, and
    // asking would fail; every other failure is the check not running.
    if !branch_is_unborn(&text) {
        let args = ["rev-parse", "--verify", "HEAD"];
        let id = match git(params, dir, &args, HEAD_CAP, cancel).await? {
            Ok(out) => out,
            Err(why) => return Ok(Err(why)),
        };
        state.head = match parse_commit_id(&id) {
            Ok(id) => Some(id),
            Err(why) => return Ok(Err(why)),
        };
    }
    Ok(Ok(state))
}

/// The exact argv (after the program) of the local size query. `path` is a validated
/// [`StorePath`], so it starts with `/nix/store/` and cannot be read as an option.
#[must_use]
pub fn local_size_argv(path: &StorePath) -> Vec<String> {
    vec![
        "path-info".to_owned(),
        "--recursive".to_owned(),
        "--size".to_owned(),
        "--".to_owned(),
        path.as_str().to_owned(),
    ]
}

/// The exact argv (after the program) of the cache size query, for the already validated
/// `cache_url` ([`base_url`]'s output).
#[must_use]
pub fn cache_size_argv(cache_url: &str, path: &StorePath) -> Vec<String> {
    vec![
        "path-info".to_owned(),
        "--recursive".to_owned(),
        "--size".to_owned(),
        // As for the preview's query: a narinfo nix did not find is remembered for an hour
        // otherwise, which would hide a toplevel whose references are still being uploaded.
        "--option".to_owned(),
        "narinfo-cache-negative-ttl".to_owned(),
        "0".to_owned(),
        "--store".to_owned(),
        cache_url.to_owned(),
        "--".to_owned(),
        path.as_str().to_owned(),
    ]
}

async fn size_with(
    nix: &OsStr,
    argv: Vec<String>,
    what: &str,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<Vec<(StorePath, u64)>, String>, Cancelled> {
    let mut cmd = match command(nix) {
        Ok(c) => c,
        Err(e) => return Ok(Err(spawn_error(&format!("cannot run {what}"), nix, &e))),
    };
    cmd.args(argv);
    let ran = match run(cmd, None, OnExit::KillGroup, limits(), deadline, cancel).await {
        Ok(r) => r,
        Err(e) => return Ok(Err(spawn_error(&format!("cannot run {what}"), nix, &e))),
    };
    let timed_out = matches!(ran.end, End::TimedOut);
    match text_of(&ran, what, timeout) {
        Err(Failure::Cancelled) => Err(Cancelled),
        Err(Failure::Why(why)) => Ok(Err(if timed_out {
            // The one budget is spent: tell the caller not to try another place.
            format!("{why} (out of time)")
        } else {
            why
        })),
        Ok(text) => Ok(parse_closure_sizes(&text).map_err(|e| format!("{what}: {e}"))),
    }
}

/// The NAR size of every path of the closure of `path`, the pending toplevel (the caller sums
/// them, or leaves out the ones a machine has already: [`auspex_domain::preflight::estimate`]).
///
/// 1. `nix path-info --recursive --size -- <path>` against the local store. It succeeds only
///    when the path is valid here, writes nothing, and a failure of any kind falls through.
/// 2. `nix path-info --recursive --size --store <cache> -- <path>`, when a cache is configured.
///
/// Both run under the one `params.timeout` and the one cancellation token. If both fail the
/// answer names both. `params.nix_store` is not used: sizes come from `nix`.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; the process group has been killed and reaped.
pub async fn closure_size(
    path: &StorePath,
    params: &PendingParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<Vec<(StorePath, u64)>, String>, Cancelled> {
    const WHAT: &str = "closure size query";
    let deadline = Instant::now() + params.timeout;
    let local = match size_with(
        params.nix,
        local_size_argv(path),
        "local store size query",
        deadline,
        params.timeout,
        cancel,
    )
    .await?
    {
        Ok(bytes) => return Ok(Ok(bytes)),
        Err(why) if why.ends_with("(out of time)") => return Ok(Err(format!("{WHAT}: {why}"))),
        Err(why) => why,
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
    Ok(
        match size_with(
            params.nix,
            cache_size_argv(url.as_str(), path),
            "cache size query",
            deadline,
            params.timeout,
            cancel,
        )
        .await?
        {
            Ok(bytes) => Ok(bytes),
            Err(why) => Err(format!("{WHAT}: not in the local store ({local}); {why}")),
        },
    )
}

/// [`tree_state`] for the fleet flake: in its directory when it is a local one.
///
/// A fleet that is not a local directory (a remote reference) has no working tree to be dirty,
/// and that is said rather than guessed.
///
/// # Errors
///
/// [`Cancelled`] as [`tree_state`].
pub async fn fleet_tree(
    params: &GitParams<'_>,
    flake: &FlakeRef,
    cancel: &CancellationToken,
) -> Result<Result<TreeState, String>, Cancelled> {
    let Some(dir) = flake.local_dir() else {
        return Ok(Err(
            "the fleet is not a local directory, so there is no working tree to check".to_owned(),
        ));
    };
    tree_state(params, &dir, cancel).await
}
