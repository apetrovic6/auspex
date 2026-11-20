//! `s`: lending the terminal to ssh and taking it back.
//!
//! This is the one place auspex gives a child a terminal on purpose. Everything else it starts
//! runs in a session of its own with no controlling terminal and stdin, stdout and stderr
//! piped, so nothing can prompt on the screen the TUI is drawing on. Here the operator is the
//! one typing, so the TUI steps aside completely: [`Shell::open_shell`] sends a [`Handoff`] to
//! the TUI loop, and the loop calls, in this order,
//!
//! 1. [`TerminalEvents::stop`]: the loop's reader thread is stopped and joined, because a reader
//!    still alive when ssh starts would take keys that are ssh's;
//! 2. [`leave_terminal`]: bracketed paste off, raw mode off, the alternate screen left, the
//!    cursor shown, so ssh starts on the screen the operator started auspex from;
//! 3. [`run`]: ssh in the foreground, until it exits or auspex is told to terminate;
//! 4. unless that was a termination, [`enter_terminal`] (what `ratatui::init` did, again), a
//!    new [`TerminalEvents`], and a redraw. The core is then told how ssh ended
//!    ([`ShellEnd`]), and the status bar says it.
//!
//! While ssh runs the loop is inside [`run`]: no frame is drawn and no key is read, but the
//! runtime goes on, so probes, evaluations and an update's output carry on (and the Log tab has
//! the update's output when auspex is back).
//!
//! # Signals
//!
//! ssh stays in auspex's process group, which is the terminal's foreground group, so the
//! terminal's signals (Ctrl-C's `SIGINT`, Ctrl-\'s `SIGQUIT`, Ctrl-Z's `SIGTSTP`) reach ssh,
//! as they would from any shell. They reach auspex too, which is why it must not act on them:
//!
//! - `SIGINT` and `SIGQUIT` are taken by [`Termination`] already; while ssh runs they are read
//!   and dropped, instead of quitting auspex from under the operator's ssh session.
//! - `SIGTSTP` would stop auspex (the terminal is cooked now, so Ctrl-Z is a signal again), so
//!   it is caught for the duration, and when ssh stops itself (it can, at a password prompt) it
//!   is continued at once: auspex has no job control to bring a stopped ssh back, and a terminal
//!   nobody can type into is worse than a Ctrl-Z that does nothing. The handler stays
//!   installed for the rest of the run (a handler cannot be removed), which only matters for a
//!   `kill -TSTP` aimed at auspex itself.
//! - `SIGTERM` and `SIGHUP` are auspex's own to obey: ssh is sent `SIGTERM` (then `SIGKILL`
//!   after [`KILL_AFTER`]), the terminal is left as it already is (cooked, on the main screen,
//!   cursor shown) and the loop exits with 128 + the signal, as it does everywhere else.
//!
//! Because the process group is shared there is no `tcsetpgrp` and so no `SIGTTOU` to defend
//! against when the terminal is taken back: auspex never stops being in the foreground.
//!
//! # What ssh leaves behind
//!
//! - **The terminal's modes.** ssh puts the tty in raw mode and restores it only on a clean exit.
//!   [`run`] saves them before ssh starts and puts them back the moment it has ended ([`Modes`]),
//!   on every path, before the TUI takes the terminal again, so what crossterm later saves as
//!   "before raw mode" is the operator's, not ssh's.
//! - **A pid that is no longer ssh's.** Signals go through [`signal_ssh`], which refuses once the
//!   waiter has marked ssh reaped, and the waiter marks it before it reaps.
//! - **A panic while suspended** must not write escape sequences into ssh's session, so the TUI's
//!   panic hooks are set aside until the terminal is taken back ([`suspend_panic_hook`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use auspex_core::ShellEnd;
use auspex_effects::Termination;
use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::{DefaultTerminal, Terminal};
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, WaitOptions, kill_process, waitid, waitpid,
};
use rustix::termios::{OptionalActions, QueueSelector, Termios, tcflush, tcgetattr, tcsetattr};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::oneshot;

/// How long after ssh ends auspex waits for the terminal's signals to reach it, before it forgets
/// the ones meant for ssh.
pub const SIGNAL_SETTLE: Duration = Duration::from_millis(100);

/// How long ssh gets to exit after `SIGTERM` before it is killed.
pub const KILL_AFTER: Duration = Duration::from_secs(3);

/// A request for the terminal, from the shell to the TUI loop.
#[derive(Debug)]
pub struct Handoff {
    /// ssh, as an absolute path.
    pub program: PathBuf,
    /// Its argv, from the domain ([`auspex_domain::fleet::SshOperation::Interactive`]).
    pub args: Vec<String>,
    /// Where the loop says how ssh ended.
    pub reply: oneshot::Sender<ShellEnd>,
}

/// How long the reader thread waits for a key before it looks at its stop flag again. It is
/// also the longest [`TerminalEvents::stop`] can take; a key wakes it at once.
const READER_TICK: Duration = Duration::from_millis(50);

/// The terminal's events (keys, pastes, resizes), read on a thread of our own that can be
/// **stopped**: the one thing `crossterm::event::EventStream` cannot do safely.
///
/// `EventStream` reads on a helper thread that sits in `poll(None)` holding crossterm's global
/// reader lock, and the only way to end it is to drop the stream, which wakes it through a
/// one-shot token. Dropped at the wrong moment (the helper not yet in `poll`), that token is
/// consumed by whoever polls next, the helper then blocks for good, and the lock is never
/// released: the next `poll` from the loop never returns and the TUI freezes after ssh. This was
/// measured, not guessed: it hung about one `s` in two under load. A thread that polls with a
/// timeout and checks a flag has no token to lose, and [`TerminalEvents::stop`] can *join* it,
/// so when it returns nothing of auspex's is reading the terminal.
pub struct TerminalEvents {
    events: UnboundedReceiver<std::io::Result<crossterm::event::Event>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl TerminalEvents {
    /// Starts reading.
    ///
    /// # Panics
    ///
    /// When the operating system cannot start a thread.
    #[must_use]
    pub fn start() -> Self {
        let (tx, events) = unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let reader = std::thread::Builder::new()
            .name("auspex-keys".to_owned())
            .spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    // An error ends the thread after it is reported: the loop treats it as
                    // fatal, as it did the stream's.
                    match crossterm::event::poll(READER_TICK) {
                        Ok(false) => {}
                        Ok(true) => match crossterm::event::read() {
                            Ok(event) => {
                                if tx.send(Ok(event)).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Err(e));
                                break;
                            }
                        },
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })
            .expect("a thread to read the terminal on");
        Self {
            events,
            stop,
            reader: Some(reader),
        }
    }

    /// The next event; `None` once the reader has stopped (after an error it reported). Safe to
    /// drop mid-wait, as a `select!` branch does every frame.
    pub async fn next(&mut self) -> Option<std::io::Result<crossterm::event::Event>> {
        self.events.recv().await
    }

    /// Stops reading and waits until the reader is gone, then forgets every event that was read
    /// and not handled: the ones in the channel (dropped with `self`) and the ones crossterm
    /// has parsed and not yet returned (drained here), so keys typed together with the `s`, or
    /// in a burst behind it, are not replayed into auspex when it comes back, where one could
    /// be an action's key. Bytes the reader had not yet read stay in the terminal's input queue,
    /// where a shell's typeahead for the program it is about to start stays: they are ssh's, and
    /// what ssh leaves unread is discarded when auspex takes the terminal back
    /// ([`enter_terminal`]).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // Nothing else reads the terminal now, so this cannot wait on anyone.
        while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
            if crossterm::event::read().is_err() {
                break;
            }
        }
    }
}

impl Drop for TerminalEvents {
    /// The loop ends: the reader notices within [`READER_TICK`] and exits with the process.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// A panic hook, as `std::panic::set_hook` takes it.
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// The hooks that were installed when the terminal was lent away, until it is taken back.
static HOOKS: Mutex<Option<PanicHook>> = Mutex::new(None);

/// While ssh has the terminal, a panic must not write into the operator's session. The hooks
/// installed for the TUI (ratatui's, ours) restore the terminal by writing `?1049l` and
/// `?2004l`, which would knock a remote full-screen program off its alternate screen and print
/// into whatever ssh is showing. They are set aside for the duration and replaced by one that
/// only says, in plain text, that auspex panicked; [`resume_panic_hook`] puts them back.
fn suspend_panic_hook() {
    let mut set_aside = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if set_aside.is_some() {
        return;
    }
    *set_aside = Some(std::panic::take_hook());
    drop(set_aside);
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(
            std::io::stderr(),
            "auspex panicked while ssh has the terminal: {info}"
        );
    }));
}

/// Puts back the hooks [`suspend_panic_hook`] set aside, if it did.
fn resume_panic_hook() {
    let mut set_aside = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hooks) = set_aside.take() {
        std::panic::set_hook(hooks);
    }
}

/// Gives the terminal back to the shell it was started from.
///
/// What `ratatui::restore` and
/// [`crate::app`]'s teardown do, plus the cursor, which a frame hides and `restore` does not
/// show. From here until [`enter_terminal`] a panic no longer writes into the terminal
/// (see [`suspend_panic_hook`]).
pub fn leave_terminal() {
    suspend_panic_hook();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    let _ = execute!(std::io::stdout(), Show);
    let _ = std::io::stdout().flush();
}

/// Takes the terminal for the TUI again: raw mode, the alternate screen, bracketed paste, and a
/// full redraw on the next frame (ssh wrote over everything, and the size may have changed).
///
/// The terminal is **replaced** with a new one rather than cleared. `Terminal::clear` first asks
/// the terminal where its cursor is (to put it back), and a terminal that does not answer, or
/// answers late, costs two seconds and then an error; a new `Terminal` has no such question, and
/// its empty buffers are what make the next frame draw everything. The screen itself is cleared
/// explicitly, because a diff against an empty buffer does not write the blank cells.
///
/// # Errors
///
/// The terminal cannot be put back. The caller says so and exits.
pub fn enter_terminal(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    resume_panic_hook();
    enable_raw_mode()?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        Clear(ClearType::All)
    )?;
    // Whatever was typed while ssh had the terminal and ssh did not read is not for auspex.
    let _ = tcflush(rustix::stdio::stdin(), QueueSelector::IFlush);
    *terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    Ok(())
}

/// How a session ended, and whether auspex must now exit.
#[derive(Debug, PartialEq, Eq)]
pub struct Session {
    /// For the core.
    pub end: ShellEnd,
    /// The signal that ended auspex's wait (`SIGTERM` or `SIGHUP`): ssh has been stopped and the
    /// caller must exit with it, without taking the terminal back.
    pub stopped_by: Option<&'static str>,
}

/// The terminal's modes (`termios`) as they were when ssh was about to start, put back as soon
/// as it has ended, on every path.
///
/// A real ssh puts the tty in raw mode and restores it only when it exits cleanly; killed (by
/// `kill -9`, by the OOM killer, by [`KILL_AFTER`]) it cannot. Without this, [`enter_terminal`]
/// would take ssh's raw mode for "the mode before auspex" (crossterm saves what it finds), and
/// the operator's shell would get it back when auspex quit: no echo, no line editing, no
/// Ctrl-C, staircase output, until `reset`. A job-control shell does the same thing for the
/// same reason.
struct Modes(Option<Termios>);

impl Modes {
    /// What the terminal is now. `None` when stdin is not a terminal, where there is nothing to
    /// put back.
    fn save() -> Self {
        Self(tcgetattr(rustix::stdio::stdin()).ok())
    }

    /// Puts them back, once.
    fn restore(&mut self) {
        if let Some(modes) = self.0.take() {
            let _ = tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &modes);
        }
    }
}

impl Drop for Modes {
    /// Also when the wait is abandoned (the future is dropped).
    fn drop(&mut self) {
        self.restore();
    }
}

/// Whether ssh has been reaped: from then on its pid may belong to someone else, so nothing may
/// signal it. Held while signalling and while setting it, so one decision is never half made.
type Reaped = Arc<Mutex<bool>>;

/// Signals `pid` unless it has been reaped. The waiter marks it reaped *before* it reaps (it
/// waits with `WNOWAIT` first), so a pid that is not marked is still ssh, at worst a zombie, and
/// cannot have been handed to another process.
fn signal_ssh(pid: Pid, reaped: &Reaped, signal: Signal) {
    let reaped = reaped
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !*reaped {
        let _ = kill_process(pid, signal);
    }
}

/// Runs `program args` in the foreground with the terminal, and waits for it.
///
/// stdin, stdout and stderr are auspex's own, the environment is auspex's own (so
/// `SSH_AUTH_SOCK` and `SSH_ASKPASS` are the operator's), and nothing is wrapped around it: no
/// shell, no new session, no timeout. The terminal's modes are saved before ssh starts and put
/// back the moment it has ended ([`Modes`]), whichever way. See the module documentation for the
/// signals.
pub async fn run(program: &Path, args: &[String], term: &mut Termination) -> Session {
    let not_started = |reason: String| Session {
        end: ShellEnd::NotStarted { reason },
        stopped_by: None,
    };
    // Before ssh exists: a SIGTSTP in the first moments would stop auspex.
    let _tstp = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
        Signal::TSTP.as_raw(),
    ));
    let mut modes = Modes::save();
    let child = match std::process::Command::new(program).args(args).spawn() {
        Ok(child) => child,
        Err(e) => {
            return not_started(format!("could not run {}: {e}", program.display()));
        }
    };
    let pid = Pid::from_child(&child);
    let reaped: Reaped = Arc::new(Mutex::new(false));
    // The waiting is ours, not tokio's: tokio's child reaping never asks for stops, and a stopped
    // ssh must be seen. The `Child` is only a handle here, and is never waited on again.
    let mut waiter = tokio::task::spawn_blocking({
        let reaped = Arc::clone(&reaped);
        move || {
            let end = wait_for(pid, &reaped);
            drop(child);
            end
        }
    });
    loop {
        tokio::select! {
            ended = &mut waiter => {
                // Before anything else: the terminal is the operator's again, as it was.
                modes.restore();
                // Ctrl-C reached auspex as well as ssh, and ssh may be gone before the runtime
                // has noticed auspex's copy: give it a moment to, and forget it, or the loop
                // would read it next and quit (seen: one run in twenty under load).
                tokio::time::sleep(SIGNAL_SETTLE).await;
                term.forget_interrupts().await;
                return Session {
                    end: ended.unwrap_or_else(|e| ShellEnd::NotStarted {
                        reason: format!("waiting for ssh failed: {e}"),
                    }),
                    stopped_by: None,
                };
            }
            signal = term.recv() => {
                // The terminal's own: for ssh, which got them too.
                if matches!(signal, "SIGINT" | "SIGQUIT") {
                    continue;
                }
                signal_ssh(pid, &reaped, Signal::TERM);
                let end = if let Ok(ended) = tokio::time::timeout(KILL_AFTER, &mut waiter).await {
                    ended
                } else {
                    signal_ssh(pid, &reaped, Signal::KILL);
                    (&mut waiter).await
                };
                // ssh is gone, killed or not: its modes are not ours to keep.
                modes.restore();
                return Session {
                    end: end.unwrap_or_else(|e| ShellEnd::NotStarted {
                        reason: format!("waiting for ssh failed: {e}"),
                    }),
                    stopped_by: Some(signal),
                };
            }
        }
    }
}

/// Waits for `pid` to end, continuing it whenever it stops, and marks it reaped just before it
/// reaps it.
///
/// `waitid` with `WNOWAIT` returns when ssh has ended and leaves it unreaped, so the flag is set
/// while the pid is still certainly ssh's; only then is it reaped with `waitpid`.
fn wait_for(pid: Pid, reaped: &Reaped) -> ShellEnd {
    loop {
        let options = WaitIdOptions::EXITED | WaitIdOptions::STOPPED | WaitIdOptions::NOWAIT;
        match waitid(WaitId::Pid(pid), options) {
            Ok(Some(status)) if status.stopped() => {
                let _ = kill_process(pid, Signal::CONT);
            }
            Ok(Some(_)) => {
                *reaped
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                return reap(pid);
            }
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                *reaped
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                return ShellEnd::NotStarted {
                    reason: format!("could not wait for ssh: {e}"),
                };
            }
        }
    }
}

/// Reaps `pid`, which has ended.
fn reap(pid: Pid) -> ShellEnd {
    loop {
        match waitpid(Some(pid), WaitOptions::empty()) {
            Ok(Some((_, status))) => {
                if let Some(code) = status.exit_status() {
                    return ShellEnd::Exited { code };
                }
                if let Some(signal) = status.terminating_signal() {
                    return ShellEnd::Signaled { signal };
                }
            }
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                return ShellEnd::NotStarted {
                    reason: format!("could not reap ssh: {e}"),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Nothing is signalled once ssh is marked reaped (its pid may be someone else's by then),
    /// and it is while it is not. The race itself cannot be staged; the rule that closes it can.
    #[test]
    fn a_reaped_ssh_is_not_signalled() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = Pid::from_child(&child);
        let reaped: Reaped = Arc::new(Mutex::new(true));
        signal_ssh(pid, &reaped, Signal::KILL);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            child.try_wait().unwrap().is_none(),
            "a pid marked reaped was signalled"
        );
        *reaped.lock().unwrap() = false;
        signal_ssh(pid, &reaped, Signal::KILL);
        assert!(!child.wait().unwrap().success(), "an unreaped one was not");
    }

    /// A panic while the terminal is lent to ssh runs none of the TUI's hooks (they write escape
    /// sequences into the operator's session); one after it is taken back runs them again.
    #[test]
    fn a_panic_while_suspended_does_not_run_the_tuis_hooks() {
        static RAN: AtomicUsize = AtomicUsize::new(0);
        let before = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {
            RAN.fetch_add(1, Ordering::SeqCst);
        }));
        suspend_panic_hook();
        // Twice: the second must not set aside the minimal hook in the first one's place.
        suspend_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("while suspended"));
        assert_eq!(
            RAN.load(Ordering::SeqCst),
            0,
            "the TUI's hook ran in ssh's session"
        );
        resume_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("after"));
        assert_eq!(RAN.load(Ordering::SeqCst), 1, "the TUI's hook is back");
        std::panic::set_hook(before);
    }
}
