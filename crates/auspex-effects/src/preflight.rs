//! What the checks before an update have to look up: the state of the fleet repository, and
//! the size of the closure the update would copy.
//!
//! Both are read-only and both go through the process supervisor like every other child here
//! (its own session, no terminal, stdin on `/dev/null`, capped output, killed and reaped on
//! cancellation or timeout).
//!
//! - [`tree_state`]: one `git status` ([`auspex_domain::preflight::STATUS_ARGS`]) in the fleet
//!   directory, run exactly as the cache key's git commands are ([`crate::cache`]: the same
//!   address-space limit, `core.fsmonitor=false`, `GIT_OPTIONAL_LOCKS=0`, and the variables
//!   that would point git at another repository removed), plus the hardening that command
//!   needs because, unlike those, it can read file contents. A fleet that is not a local
//!   directory, or not a git repository, is an answer ("cannot be checked"), not a failure.
//! - [`closure_size`]: the sum of the NAR sizes of the pending toplevel's closure, from
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
use auspex_domain::preflight::{STATUS_ARGS, TreeState, parse_closure_sizes, parse_tree_status};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::cache::{GitParams, git};
use crate::closure::{Failure, PendingParams, limits, text_of};
use crate::http::base_url;
use crate::nix::FlakeRef;
use crate::process::{End, OnExit, command, run, spawn_error};

/// The most `git status` output kept: 4 MiB, about 40,000 changed files. Past it the check is
/// "could not be done", not a count from a prefix.
const STATUS_CAP: usize = 4 * 1024 * 1024;

/// The fleet repository's state, read from `dir`.
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
    let out = match git(params, dir, &STATUS_ARGS, STATUS_CAP, cancel).await? {
        Ok(out) => out,
        Err(why) => return Ok(Err(why)),
    };
    Ok(parse_tree_status(&String::from_utf8_lossy(&out)))
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
) -> Result<Result<u64, String>, Cancelled> {
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

/// The NAR size, in bytes, of the closure of `path`, the pending toplevel.
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
) -> Result<Result<u64, String>, Cancelled> {
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
