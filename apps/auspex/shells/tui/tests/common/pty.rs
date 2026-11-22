//! A pseudo-terminal for the built binary: what the operator's terminal does, with nothing
//! but safe code.
//!
//! The binary is started under util-linux `setsid --ctty` with the pty's slave as stdin, stdout
//! and stderr, so it has the pty as its **controlling terminal**: the line discipline then
//! turns a Ctrl-C written to the master into a `SIGINT` for the foreground process group, as a
//! real terminal does. That is the one thing a pipe or an unattached pty cannot show, and the
//! reason this is not `Stdio::piped()`. (`setsid` is the only way to give a child a
//! controlling terminal without `unsafe`: `Command::pre_exec` is unsafe and the workspace
//! forbids it.) Its output is collected from the master by a thread, raw, escape sequences
//! included, because the escape sequences are what these tests are about: which screen the
//! operator is looking at, whether the cursor is shown, whether bracketed paste is on.
//!
//! The termios of the slave is read from the master (`tcgetattr` on Linux returns the
//! slave's), which is how "raw mode is off while ssh runs" is observed rather than assumed.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, open};
use rustix::process::{Pid, Signal, kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};

pub const ENTER_ALT: &[u8] = b"\x1b[?1049h";
pub const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
pub const PASTE_ON: &[u8] = b"\x1b[?2004h";
pub const PASTE_OFF: &[u8] = b"\x1b[?2004l";
pub const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
pub const CURSOR_HIDE: &[u8] = b"\x1b[?25l";

/// How long a wait for the terminal, or for the process to exit, lasts before the test fails:
/// [`super::PATIENT`]. None of these waits is something a test is measuring, and each ends the
/// moment its condition holds, so a long one costs nothing until something is wrong. (It was
/// 30 s, which a core shared with twelve busy loops did not give the shell, AUS-47.)
pub const WAIT: Duration = super::PATIENT;

pub struct Pty {
    child: Child,
    master: File,
    out: Arc<Mutex<Vec<u8>>>,
    pub pid: i32,
}

/// Starts `cmd` (its program, arguments and environment) on a new pty, as a session leader
/// whose controlling terminal it is. `setsid` is the absolute path of util-linux's.
pub fn spawn(cmd: &Command, setsid: &Path) -> Pty {
    let master: OwnedFd = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let name = ptsname(&master, Vec::new()).unwrap();
    let slave: OwnedFd = open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .unwrap();
    tcsetwinsize(
        &master,
        Winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();

    let mut wrapped = Command::new(setsid);
    wrapped
        .arg("--ctty")
        .arg(cmd.get_program())
        .args(cmd.get_args());
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => wrapped.env(k, v),
            None => wrapped.env_remove(k),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        wrapped.current_dir(dir);
    }
    wrapped
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let child = wrapped.spawn().unwrap();
    // `wrapped` holds the last copies of the slave in this process: it must go, or the master
    // never reads end-of-file when the child is gone.
    drop(wrapped);

    let master = File::from(master);
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut reader = master.try_clone().unwrap();
    let sink = Arc::clone(&out);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // `EIO` at the end: every slave descriptor is closed.
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let pid = i32::try_from(child.id()).unwrap();
    Pty {
        child,
        master,
        out,
        pid,
    }
}

impl Pty {
    /// Everything written to the terminal so far, raw.
    pub fn raw(&self) -> Vec<u8> {
        self.out.lock().unwrap().clone()
    }

    /// What the terminal is showing, as text (see [`screen`]).
    pub fn text(&self) -> String {
        screen(&self.raw())
    }

    /// Types `bytes`, as the operator's keyboard would.
    pub fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    /// Waits until `ready` holds of the raw output, or fails the test with the output.
    pub fn wait_until(&self, what: &str, ready: impl Fn(&[u8]) -> bool) {
        let start = Instant::now();
        loop {
            let raw = self.raw();
            if ready(&raw) {
                return;
            }
            assert!(
                start.elapsed() < WAIT,
                "timed out waiting for {what}; the terminal shows:\n{}\n--- the last bytes written:\n{:?}",
                screen(&raw),
                format!(
                    "{}\n--- threads:\n{}",
                    String::from_utf8_lossy(&raw[raw.len().saturating_sub(700)..]),
                    threads(self.pid)
                )
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until `needle` is on the terminal as text.
    pub fn wait_for_text(&self, needle: &str) {
        self.wait_until(&format!("{needle:?}"), |raw| screen(raw).contains(needle));
    }

    /// Waits until the process has exited.
    pub fn wait_exit(&mut self) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                start.elapsed() < WAIT,
                "the process did not exit; the terminal shows:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether the process is still running (not exited, not a zombie).
    pub fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    pub fn signal(&self, signal: Signal) {
        kill_process(Pid::from_raw(self.pid).unwrap(), signal).unwrap();
    }

    /// The terminal is in cooked mode: line editing, echo and signal keys, as a shell leaves it.
    pub fn cooked(&self) -> bool {
        let modes = tcgetattr(&self.master).unwrap().local_modes;
        modes.contains(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG)
    }

    /// The terminal is raw, as the TUI sets it: none of line editing, echo and signal keys.
    pub fn raw_mode(&self) -> bool {
        let modes = tcgetattr(&self.master).unwrap().local_modes;
        !modes.intersects(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // A test that failed must not leave auspex (or the fake ssh under it) running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where `needle` last occurs in `haystack`.
pub fn last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// How many times `needle` occurs in `haystack`.
pub fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// Whether the mode `on`/`off` toggles is on at the end of `output`: the later of the two
/// sequences wins. `None` if neither was ever written.
pub fn state(output: &[u8], on: &[u8], off: &[u8]) -> Option<bool> {
    match (last(output, on), last(output, off)) {
        (None, None) => None,
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (Some(a), Some(b)) => Some(a > b),
    }
}

/// What the terminal would be showing after `output`: the screen as text, one line per row.
///
/// A replay of the cursor movements, clears and characters in `output` over a 120 by 40 grid,
/// which is all ratatui's crossterm backend writes. It is needed because ratatui writes only the
/// cells that changed since the last frame, so a message is rarely one contiguous run in the
/// byte stream: stripping escape sequences from the stream gives "eval ca off" for "eval
/// cache off", and a test that waits for text that way passes or fails by timing. Entering the
/// alternate screen clears the grid, as it does a terminal. Styles are ignored.
pub fn screen(output: &[u8]) -> String {
    const ROWS: usize = 40;
    const COLS: usize = 120;
    let text = String::from_utf8_lossy(output);
    let mut grid = vec![vec![' '; COLS]; ROWS];
    let (mut row, mut col) = (0usize, 0usize);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut last = ' ';
                    for p in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&p) {
                            last = p;
                            break;
                        }
                        params.push(p);
                    }
                    let private = params.starts_with('?');
                    let numbers: Vec<usize> = params
                        .trim_start_matches('?')
                        .split(';')
                        .map(|n| n.parse().unwrap_or(0))
                        .collect();
                    let n = |i: usize, default: usize| match numbers.get(i) {
                        Some(&0) | None => default,
                        Some(&v) => v,
                    };
                    match last {
                        'H' | 'f' => {
                            row = (n(0, 1) - 1).min(ROWS - 1);
                            col = (n(1, 1) - 1).min(COLS - 1);
                        }
                        'A' => row = row.saturating_sub(n(0, 1)),
                        'B' => row = (row + n(0, 1)).min(ROWS - 1),
                        'C' => col = (col + n(0, 1)).min(COLS - 1),
                        'D' => col = col.saturating_sub(n(0, 1)),
                        'G' => col = (n(0, 1) - 1).min(COLS - 1),
                        'd' => row = (n(0, 1) - 1).min(ROWS - 1),
                        'J' if numbers.first().copied().unwrap_or(0) >= 2 => {
                            grid = vec![vec![' '; COLS]; ROWS];
                        }
                        'J' => {
                            grid[row][col..].fill(' ');
                            for r in &mut grid[row + 1..] {
                                r.fill(' ');
                            }
                        }
                        'K' => match numbers.first().copied().unwrap_or(0) {
                            0 => grid[row][col..].fill(' '),
                            1 => grid[row][..=col].fill(' '),
                            _ => grid[row].fill(' '),
                        },
                        'h' if private && params == "?1049" => {
                            grid = vec![vec![' '; COLS]; ROWS];
                            (row, col) = (0, 0);
                        }
                        _ => {}
                    }
                }
                Some(']') => {
                    // An OSC string ends at BEL or ESC \.
                    for p in chars.by_ref() {
                        if p == '\x07' || p == '\\' {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => col = 0,
            '\n' => row = (row + 1).min(ROWS - 1),
            c if c.is_control() => {}
            c => {
                if col >= COLS {
                    col = 0;
                    row = (row + 1).min(ROWS - 1);
                }
                grid[row][col] = c;
                col += 1;
            }
        }
    }
    grid.iter()
        .map(|r| r.iter().collect::<String>().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Each thread of `pid`: name, state and what it is blocked in. For a test that timed out, to
/// tell a process that is gone from one that is stuck, and where.
pub fn threads(pid: i32) -> String {
    let mut out = String::new();
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return "the process is gone".to_owned();
    };
    for entry in dir.flatten() {
        let p = entry.path();
        let read = |f: &str| {
            std::fs::read_to_string(p.join(f))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let stat = read("stat");
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, r)| r.split_whitespace().next())
            .unwrap_or("?")
            .to_owned();
        let _ = writeln!(out, "{} {state} {}", read("comm"), read("wchan"));
    }
    out
}
