//! auspex's adapters: the only code in auspex that touches the outside world, and the only
//! crate tokio lives in.
//!
//! **Admission rule.** A function belongs here when it *obtains* something: it runs a
//! process (`ssh`, `nix`, `nix-eval-jobs`, `git`), reads or writes a file, asks a server, or
//! resolves a name. It takes everything it needs as arguments (no globals, no config files),
//! hands what it obtained to `auspex-domain` to be parsed and judged, and returns the
//! domain's value. Parsing, status rules and anything else that is a function of values
//! belongs in `auspex-domain`; deciding *what* to ask, and when, belongs in the Crux core.
//! This crate never depends on the core: the shell translates a core `Effect` into a call
//! here.
//!
//! Modules, one per adapter:
//!
//! - [`ssh`]: the probe, and the control-socket directory it needs.
//! - [`nix`]: the machine list (`nix eval`) and the evaluation (`nix-eval-jobs`).
//! - [`cache`]: the evaluation cache under `$XDG_CACHE_HOME/auspex/`, and
//!   [`cache::evaluate_cached`], which puts it in front of [`nix::evaluate`].
//! - [`prometheus`]: the fleet-wide health query.
//! - [`narinfo`]: the "already cached" badge.
//! - [`dns`]: hostname resolution for the shared-IP lint.
//! - [`http`]: the one HTTP client the two HTTP adapters share.
//!
//! # Timeouts and cancellation, everywhere the same
//!
//! Every function is async, has a timeout among its arguments, and takes a
//! [`CancellationToken`]. Cancelling the token (or the timeout passing) stops the work, and
//! for a function that spawned a process, **kills its whole process group and reaps it
//! before returning**: dropping a future would kill only the direct child (`kill_on_drop`),
//! and `nix-eval-jobs` forks workers. A cancelled call returns `Err(Cancelled)`, and
//! the shell drops the core's request without answering it. A timeout is not a
//! cancellation: it is answered, in the domain's terms (`ProbeExit::TimedOut`, an `Err`
//! the operator sees, `NarCache::Failed`).
//!
//! Dropping a future instead of cancelling is still safe: the process group is killed from a
//! drop guard, and tokio reaps the child in the background.

// `CommandExt::setsid` (nightly `process_setsid`, rust-lang/rust#105376): spawning in a new
// session is the only way to give a child no controlling terminal, and the stable route,
// `pre_exec` calling `setsid(2)`, is `unsafe`, which the workspace forbids. The toolchain
// is pinned nightly (versions.toml) and the gate is std's own code: if it changes, this
// stops compiling rather than silently losing the guarantee. See `process::spawn`.
#![feature(process_setsid)]

pub mod cache;
pub mod dns;
pub mod http;
pub mod narinfo;
pub mod nix;
pub mod prometheus;
pub mod ssh;

mod process;
mod text;

pub use tokio_util::sync::CancellationToken;

/// The call was cancelled.
///
/// Through its [`CancellationToken`], or, for an evaluation, by dropping its receiver. Any
/// process it started has been killed and reaped. There is no answer to give: the shell
/// drops the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}
