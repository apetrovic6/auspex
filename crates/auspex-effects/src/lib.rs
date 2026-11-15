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
//! drop guard, and tokio reaps the child in the background. A process that exits on its own
//! has the rest of its group killed too (except ssh; see `ssh::probe`).
//!
//! # What the shell must do: handle termination signals
//!
//! Every child runs in a session of its own, so the terminal's `SIGHUP` and `SIGINT` never
//! reach it, and nothing here runs if auspex itself is killed by a signal: no destructor,
//! no group guard (review finding F4). Killing children on the parent's death needs
//! `PR_SET_PDEATHSIG` in the child, which needs `pre_exec`, which is `unsafe`. So the
//! shell must handle `SIGHUP`, `SIGTERM`, `SIGINT` and `SIGQUIT` by cancelling every
//! token and dropping the runtime before it exits. [`termination`] is that wait:
//!
//! ```no_run
//! # async fn shell(all: auspex_effects::CancellationToken) -> std::io::Result<()> {
//! let signal = auspex_effects::termination().await?;
//! all.cancel(); // every call returns `Err(Cancelled)` after killing its group
//! # let _ = signal; Ok(()) }
//! ```
//!
//! `SIGKILL` cannot be handled; after one, children are left to finish on their own.
//!
//! # Platform
//!
//! Linux is the target (auspex runs on the operator's desktop). On macOS the default ssh
//! control directory is too long for a socket path (review finding F9, not addressed).

// `CommandExt::setsid` (nightly `process_setsid`, rust-lang/rust#105376).
//
// Why: a new session is the only way to give a child no controlling terminal, which is
// what makes "ssh, or a ProxyJump hop, can never prompt" true. The stable route, `pre_exec`
// calling `setsid(2)`, is `unsafe`, and the workspace forbids `unsafe` outright
// (`#[allow]` cannot lift `forbid`). The toolchain is pinned nightly for the workspace
// already; this is the only `#![feature]` in auspex, so auspex itself now needs nightly.
//
// When it is stabilised: the `stable_features` lint fires and CI's `-D warnings` fails the
// build. Delete this line; nothing else changes.
//
// Alternatives, in order of cost: `pre_exec` with `rustix::process::setsid` (one `unsafe`
// block, needs a scoped exception to the workspace lint); a tiny helper binary that calls
// the safe `rustix::process::setsid` and execs (stable, but must be installed beside
// auspex); util-linux `setsid` (Linux only, one more program to find). `process_group(0)`
// alone is not enough: the child keeps the terminal.
#![feature(process_setsid)]

pub mod cache;
pub mod dns;
pub mod http;
pub mod narinfo;
pub mod nix;
pub mod prometheus;
pub mod ssh;

mod process;
mod safe_fs;
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

/// Waits for `SIGHUP`, `SIGTERM`, `SIGINT` or `SIGQUIT` and returns its name.
///
/// The shell awaits this beside its main loop, then cancels every token and drops the
/// runtime, so children are killed even when the terminal closes or the operator presses
/// Ctrl-C in `--plain`. Registering replaces the default action (dying) for those four
/// signals for the whole process, which is the point.
///
/// # Errors
///
/// When a handler cannot be registered (no tokio signal driver).
pub async fn termination() -> std::io::Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut quit = signal(SignalKind::quit())?;
    Ok(tokio::select! {
        _ = hup.recv() => "SIGHUP",
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
        _ = quit.recv() => "SIGQUIT",
    })
}
