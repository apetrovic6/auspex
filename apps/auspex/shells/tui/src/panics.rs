//! What a panic does, by where it happens.
//!
//! The runtime is multi-threaded: the TUI loop runs on one thread (the one that calls
//! [`crate::app`]'s `tui`), adapters run as tasks on the others. A panic hook runs on the thread
//! that panicked, so it can tell the two apart, and they must be told apart: restoring the terminal
//! under a TUI that keeps running leaves the operator in cooked mode with bracketed paste off,
//! where a pasted line ends in a newline that is delivered as keys.
//!
//! - **On the loop thread** the panic ends auspex. The hook gives the terminal back (bracketed
//!   paste off, raw mode off, the main screen), lets the default hook print the message, and runs
//!   the cordon last resort ([`crate::cordon::Cordons::last_resort`]): every node cordoned for an
//!   update that has not started is uncordoned, within a bound, and the ones that are not are
//!   named with the command that gives them back.
//! - **On any other thread** (a task, a helper) the hook touches neither the terminal nor the
//!   cordons. It records the message, sanitised and bounded, and wakes the loop
//!   ([`Hooks::task_panicked`]). The loop ends the run **in order**, as it does for a signal
//!   ([`crate::shell::Shell::terminate_with`]): the roll is stopped, a node is given back or
//!   named, the terminal is restored by the loop, and the exit says a panic in a task ended the
//!   run, with its message, and exits non-zero. The default hook's own print is skipped, because
//!   stderr is the TUI's screen: the message is in the exit report instead.
//! - **While `s` has the terminal** it is ssh's ([`Hooks::lend`]): the hook writes no escape
//!   sequence into the operator's session, on any thread. A loop-thread panic still runs the
//!   last resort, and the message is printed as plain text.
//!
//! The hook is installed once ([`Hooks::install`]) and the terminal is touched only through
//! [`Terminal`], so a test can count the calls: there is no TTY under `cargo test`.

use std::panic::PanicHookInfo;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use tokio::sync::Notify;

use crate::cordon::Cordons;
use crate::log::say;

/// How long one `kubectl uncordon` of the loop-thread last resort may take.
const HOOK_BOUND: Duration = Duration::from_secs(10);

/// The longest a panic's message is kept: it is shown in a status line and in the exit report.
const MESSAGE_BOUND: usize = 240;

/// What a panic hook may do to the terminal.
pub trait Terminal: Send + Sync + 'static {
    /// Raw mode off, the alternate screen left (what `ratatui::restore` does).
    fn restore(&self);
    /// Bracketed paste off.
    fn paste_off(&self);
}

/// The process's own terminal.
#[derive(Debug, Clone, Copy)]
pub struct Real;

impl Terminal for Real {
    fn restore(&self) {
        ratatui::restore();
    }

    fn paste_off(&self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    }
}

/// The panic hook's state, shared with the loop.
pub struct Hooks {
    loop_thread: ThreadId,
    cordons: Arc<Cordons>,
    terminal: Arc<dyn Terminal>,
    /// `s` has the terminal: nothing is written to it.
    lent: AtomicBool,
    /// The first panic outside the loop thread.
    task_panic: Mutex<Option<String>>,
    wake: Notify,
    /// The hook this one was put on top of, for [`Hooks::uninstall`].
    previous: Mutex<Option<Previous>>,
}

/// A panic hook, as `std::panic::set_hook` takes it, shared.
type Previous = Arc<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

impl Hooks {
    /// Installs the hook on top of the one that is set now (the default one, which prints the
    /// message), and returns the state the loop reads.
    ///
    /// `loop_thread` is the thread whose panic ends auspex. Call it after the terminal is taken.
    /// ratatui's own hook must not be the one in place: it restores the terminal on any thread.
    pub fn install(
        loop_thread: ThreadId,
        cordons: Arc<Cordons>,
        terminal: Arc<dyn Terminal>,
    ) -> Arc<Self> {
        let hooks = Arc::new(Self {
            loop_thread,
            cordons,
            terminal,
            lent: AtomicBool::new(false),
            task_panic: Mutex::new(None),
            wake: Notify::new(),
            previous: Mutex::new(None),
        });
        let previous: Previous = Arc::from(std::panic::take_hook());
        let this = Arc::clone(&hooks);
        let chained = Arc::clone(&previous);
        std::panic::set_hook(Box::new(move |info| this.on_panic(info, &*chained)));
        *hooks
            .previous
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(previous);
        hooks
    }

    /// Puts back the hook that was in place before [`Hooks::install`]. For tests, which install
    /// the hook more than once in a process; a run installs it once and keeps it.
    pub fn uninstall(&self) {
        let previous = self
            .previous
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(previous) = previous {
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }

    fn on_panic(&self, info: &PanicHookInfo<'_>, previous: &(dyn Fn(&PanicHookInfo<'_>) + Sync)) {
        let lent = self.lent.load(Ordering::SeqCst);
        if std::thread::current().id() == self.loop_thread {
            if !lent {
                self.terminal.paste_off();
                self.terminal.restore();
            }
            previous(info);
            for line in self.cordons.last_resort(HOOK_BOUND) {
                say(&line);
            }
            return;
        }
        // Recorded first: whatever the print below does, the loop is told.
        {
            let mut first = self
                .task_panic
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            first.get_or_insert_with(|| describe(info));
        }
        self.wake.notify_one();
        if lent {
            // Text only, to the terminal ssh is using; the loop acts when ssh ends.
            previous(info);
        }
    }

    /// The terminal the hook acts on, for the loop's own restore on the way out.
    #[must_use]
    pub fn terminal(&self) -> &dyn Terminal {
        &*self.terminal
    }

    /// `s` has the terminal until [`Hooks::take_back`]: a panic writes no escape sequence.
    pub fn lend(&self) {
        self.lent.store(true, Ordering::SeqCst);
    }

    /// The terminal is the TUI's again.
    pub fn take_back(&self) {
        self.lent.store(false, Ordering::SeqCst);
    }

    /// Completes when a panic happened outside the loop thread, with its message. Cancel-safe.
    pub async fn task_panicked(&self) -> String {
        loop {
            if let Some(message) = self.task_panic() {
                return message;
            }
            self.wake.notified().await;
        }
    }

    /// The first panic outside the loop thread, if there was one: what and where, one line.
    #[must_use]
    pub fn task_panic(&self) -> Option<String> {
        self.task_panic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The message and the place of a panic: one line, sanitised, at most [`MESSAGE_BOUND`] characters.
fn describe(info: &PanicHookInfo<'_>) -> String {
    let message = payload_text(info.payload());
    let at = info
        .location()
        .map_or_else(String::new, |l| format!(" at {}:{}", l.file(), l.line()));
    bounded(&format!("{message}{at}"))
}

/// A panic payload as text: what `panic!` was given, or a plain fallback.
#[must_use]
pub fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// `text` on one line, sanitised, cut at [`MESSAGE_BOUND`] characters.
#[must_use]
pub fn bounded(text: &str) -> String {
    let one_line = text
        .lines()
        .map(auspex_domain::text::sanitize_line)
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= MESSAGE_BOUND {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(MESSAGE_BOUND).collect();
    cut.push('…');
    cut
}

/// A panic inside a future that [`guarded`] was polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panicked(pub String);

/// Polls `work`, and returns a panic in it as `Err(Panicked)` for the task that owns the request.
///
/// The panic hook has run by then (it runs before unwinding), so the run is already ending; this
/// is for the request that would otherwise stay unanswered until it does.
///
/// # Errors
///
/// [`Panicked`] with the sanitised, bounded message when `work` panicked.
pub async fn guarded<F: Future>(work: F) -> Result<F::Output, Panicked> {
    let mut work = Box::pin(work);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => {
                std::task::Poll::Ready(Err(Panicked(bounded(&payload_text(&*payload)))))
            }
        }
    })
    .await
}

/// An answer that says the shell could not produce one: what a panicked adapter call is answered
/// with, so the core can stop in order instead of waiting for a reply that never comes.
pub trait Internal {
    /// The failure answer, saying `why`.
    fn internal(why: &str) -> Self;
}

/// "internal error: …", the words every failure answer starts with.
#[must_use]
pub fn internal_error(why: &str) -> String {
    format!("internal error: {why}")
}

impl<T> Internal for Result<T, String> {
    fn internal(why: &str) -> Self {
        Err(internal_error(why))
    }
}

impl Internal for auspex_domain::observe::ProbeOutcome {
    fn internal(why: &str) -> Self {
        Self::Failed {
            reason: internal_error(why),
        }
    }
}

impl Internal for auspex_domain::status::NarCache {
    fn internal(_: &str) -> Self {
        Self::Failed
    }
}

impl Internal for std::collections::BTreeMap<String, Vec<std::net::IpAddr>> {
    /// Nothing resolved: not an error to the core, only no collision to find.
    fn internal(_: &str) -> Self {
        Self::new()
    }
}

impl Internal for auspex_core::effects::PreflightAnswer {
    fn internal(why: &str) -> Self {
        Self {
            tree: Err(internal_error(why)),
            closure: Err(internal_error(why)),
        }
    }
}

impl Internal for auspex_core::ActionEnd {
    fn internal(why: &str) -> Self {
        Self {
            result: auspex_core::ActionResult::Failed,
            exit_code: None,
            summary: vec![internal_error(why)],
            explanation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message is one line, with no escape sequence, and never longer than the bound.
    #[test]
    fn a_message_is_one_clean_bounded_line() {
        assert_eq!(bounded("a\n\u{1b}[31mb\u{1b}[0m"), "a b");
        let long = bounded(&"y".repeat(MESSAGE_BOUND + 50));
        assert_eq!(long.chars().count(), MESSAGE_BOUND + 1);
        assert!(long.ends_with('…'));
        assert_eq!(
            bounded(&"y".repeat(MESSAGE_BOUND)).chars().count(),
            MESSAGE_BOUND
        );
    }

    /// A panic inside the polled future is returned, not propagated; what it held is dropped.
    #[tokio::test]
    async fn a_panic_in_the_future_is_an_error_for_the_task_that_owns_the_request() {
        let ok = guarded(async { 7 }).await;
        assert_eq!(ok, Ok(7));
        let panicked = guarded(async {
            tokio::task::yield_now().await;
            panic!("an adapter bug");
            #[expect(unreachable_code, reason = "the future needs a type")]
            0
        })
        .await;
        assert!(
            panicked.is_err_and(|Panicked(why)| why == "an adapter bug"),
            "the message comes back"
        );
    }

    /// The answers a panicked call is given are failures, in each shape the core has.
    #[test]
    fn the_failure_answers_say_internal_error() {
        let r: Result<u8, String> = Internal::internal("x");
        assert_eq!(r, Err("internal error: x".to_owned()));
        assert_eq!(
            auspex_domain::observe::ProbeOutcome::internal("x"),
            auspex_domain::observe::ProbeOutcome::Failed {
                reason: "internal error: x".to_owned()
            }
        );
        assert_eq!(
            auspex_domain::status::NarCache::internal("x"),
            auspex_domain::status::NarCache::Failed
        );
        let end = auspex_core::ActionEnd::internal("x");
        assert_eq!(end.result, auspex_core::ActionResult::Failed);
        assert_eq!(end.summary, ["internal error: x"]);
        let pre = auspex_core::effects::PreflightAnswer::internal("x");
        assert!(pre.tree.is_err() && pre.closure.is_err());
    }
}
