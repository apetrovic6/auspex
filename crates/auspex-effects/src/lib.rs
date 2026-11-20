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
//! - [`act`]: what auspex does to a machine: `clan machines update` and a local `nix build`, both
//!   streamed, with the argv built here and the machine name checked here.
//! - [`clan`]: which `clan` binary the fleet pins (built from its lock), and running it.
//! - [`nix`]: the machine list (`nix eval`) and the evaluation (`nix-eval-jobs`).
//! - [`cache`]: the evaluation cache under `$XDG_CACHE_HOME/auspex/`, and
//!   [`cache::evaluate_cached`], which puts it in front of [`nix::evaluate`].
//! - [`prometheus`]: the fleet-wide health query.
//! - [`narinfo`]: the "already cached" badge.
//! - [`closure`]: the two closure queries behind the package preview: a machine's
//!   `/run/current-system` over ssh, and the pending toplevel from the local store or the cache.
//! - [`preflight`]: what the checks before an update look up: `git status` of the fleet repository
//!   and the pending closure's size.
//! - [`deploylog`]: the local SQLite log of every action (deploy, build, reboot, …), a documented
//!   schema the weekly digest reads, under `$XDG_STATE_HOME/auspex/log.sqlite`.
//! - [`freeze`]: which machines the operator froze with `f`, one small TOML file under
//!   `$XDG_STATE_HOME/auspex/state.toml` (not `config.toml`, which home-manager makes read-only),
//!   keyed by the fleet, written atomically.
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
//! shell must handle `SIGHUP`, `SIGTERM`, `SIGINT` and `SIGQUIT`, with a [`Termination`]:
//!
//! 1. **create it once, before the first child is spawned** (from then on those four
//!    signals no longer kill the process by default: the `Termination` is what notices
//!    them);
//! 2. **keep it for the whole run**, and **never recreate it in a loop**: it is the value
//!    that remembers a signal arriving while nobody is waiting, so `recv` can be called
//!    again and again from a `select!` without losing one (review finding N4);
//! 3. **when it fires, cancel every token, drop the runtime, and exit.**
//!
//! ```no_run
//! # async fn shell(all: auspex_effects::CancellationToken) -> std::io::Result<()> {
//! let mut term = auspex_effects::Termination::new()?; // before the first spawn
//! loop {
//!     tokio::select! {
//!         signal = term.recv() => {
//!             all.cancel(); // every call returns `Err(Cancelled)` after killing its group
//!             # let _ = signal;
//!             break; // then drop the runtime and exit
//!         }
//!         // … keys, frame timer, effects …
//!     }
//! }
//! # Ok(()) }
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

pub mod act;
pub mod cache;
pub mod clan;
pub mod closure;
pub mod deploylog;
pub mod dns;
pub mod freeze;
pub mod http;
pub mod narinfo;
pub mod nix;
pub mod preflight;
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

/// The process's termination signals (`SIGHUP`, `SIGTERM`, `SIGINT`, `SIGQUIT`), caught.
///
/// Created once, before the first child is spawned, and kept for the whole run (see the
/// crate documentation for the full duty). All four handlers are registered by
/// [`Termination::new`], and each remembers a signal that arrives while nobody is in
/// [`Termination::recv`], so a signal between two `recv` calls is not lost. Creating one
/// replaces the default action (dying) of those four signals for the whole process for
/// good, which is why it must not be created and dropped casually.
#[derive(Debug)]
pub struct Termination {
    hup: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
}

impl Termination {
    /// Registers all four handlers now. Must be called inside a tokio runtime.
    ///
    /// # Errors
    ///
    /// When a signal handler cannot be registered with the operating system.
    ///
    /// # Panics
    ///
    /// When called outside a tokio runtime or without its signal driver enabled.
    pub fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            hup: signal(SignalKind::hangup())?,
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            quit: signal(SignalKind::quit())?,
        })
    }

    /// Forgets every `SIGINT` and `SIGQUIT` that has arrived and not been read.
    ///
    /// For when the terminal's own Ctrl-C or Ctrl-\ reached auspex and a child at once and the
    /// child is the one it was meant for (`s`): ssh can be gone before the runtime has even
    /// noticed auspex's copy of the signal, which would then quit auspex from the next `recv`.
    /// `SIGTERM` and `SIGHUP` are kept: nobody aims those at a child.
    pub async fn forget_interrupts(&mut self) {
        use std::time::Duration;

        use tokio::time::timeout;
        while timeout(Duration::ZERO, self.int.recv()).await.is_ok() {}
        while timeout(Duration::ZERO, self.quit.recv()).await.is_ok() {}
    }

    /// Waits for the next of the four (or returns at once if one arrived since the last
    /// call) and returns its name. Cancel-safe: use it in a `select!` loop.
    pub async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.hup.recv() => "SIGHUP",
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
            _ = self.quit.recv() => "SIGQUIT",
        }
    }
}
