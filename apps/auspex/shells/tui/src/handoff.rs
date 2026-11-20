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

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
use rustix::termios::{QueueSelector, tcflush};
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

    /// Stops reading and waits until the reader is gone, then forgets every event that was
    /// read and not handled, and every key typed meanwhile: keys typed before ssh was on screen
    /// are not replayed into auspex when it comes back, where one could be an action's key.
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

/// Gives the terminal back to the shell it was started from: what `ratatui::restore` and
/// [`crate::app`]'s teardown do, plus the cursor, which a frame hides and `restore` does not
/// show.
pub fn leave_terminal() {
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

/// Runs `program args` in the foreground with the terminal, and waits for it.
///
/// stdin, stdout and stderr are auspex's own, the environment is auspex's own (so
/// `SSH_AUTH_SOCK` and `SSH_ASKPASS` are the operator's), and nothing is wrapped around it: no
/// shell, no new session, no timeout. See the module documentation for the signals.
pub async fn run(program: &Path, args: &[String], term: &mut Termination) -> Session {
    let not_started = |reason: String| Session {
        end: ShellEnd::NotStarted { reason },
        stopped_by: None,
    };
    // Before ssh exists: a SIGTSTP in the first moments would stop auspex.
    let _tstp = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
        Signal::TSTP.as_raw(),
    ));
    let child = match std::process::Command::new(program).args(args).spawn() {
        Ok(child) => child,
        Err(e) => {
            return not_started(format!("could not run {}: {e}", program.display()));
        }
    };
    let pid = Pid::from_child(&child);
    // `waitpid` is ours, not tokio's: tokio's child reaping never asks for stops, and a stopped
    // ssh must be seen. The `Child` is only a handle here, and is never waited on again.
    let mut waiter = tokio::task::spawn_blocking(move || {
        let end = wait_for(pid);
        drop(child);
        end
    });
    loop {
        tokio::select! {
            ended = &mut waiter => {
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
                let _ = kill_process(pid, Signal::TERM);
                let end = if let Ok(ended) = tokio::time::timeout(KILL_AFTER, &mut waiter).await {
                    ended
                } else {
                    let _ = kill_process(pid, Signal::KILL);
                    (&mut waiter).await
                };
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

/// Waits for `pid` to end, continuing it whenever it stops.
fn wait_for(pid: Pid) -> ShellEnd {
    loop {
        match waitpid(Some(pid), WaitOptions::UNTRACED) {
            Ok(Some((_, status))) => {
                if status.stopped() {
                    let _ = kill_process(pid, Signal::CONT);
                } else if let Some(code) = status.exit_status() {
                    return ShellEnd::Exited { code };
                } else if let Some(signal) = status.terminating_signal() {
                    return ShellEnd::Signaled { signal };
                }
            }
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                return ShellEnd::NotStarted {
                    reason: format!("could not wait for ssh: {e}"),
                };
            }
        }
    }
}
