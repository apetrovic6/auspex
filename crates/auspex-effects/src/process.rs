//! Child processes that die when auspex stops caring about them.
//!
//! Every process here is spawned through [`spawn`], which puts it in a **session of its
//! own** (`setsid()`, hence also a process group of its own, and no controlling terminal)
//! and sets `kill_on_drop`. That gives three ways for it to be stopped, all of which reach
//! its children too:
//!
//! - on a timeout or a cancellation, [`kill_and_reap`] sends `SIGKILL` to the whole group,
//!   then waits for the leader, so the call returns only once it is gone;
//! - when the future is dropped mid-run, the [`Group`] guard's `Drop` kills the group, and
//!   tokio reaps the leader in the background (`kill_on_drop`);
//! - when it exits on its own, nothing is killed: the guard is disarmed as soon as the
//!   leader is reaped. Killing the group then could hit an unrelated group that has reused
//!   the id, and for ssh would kill a `ControlPersist` master that stayed in the group.
//!
//! The group is killed *before* the leader is reaped. While the leader is alive or a zombie
//! its pid, and so the group id, cannot be reused, so the kill cannot reach anyone else.
//!
//! Output is read through capped readers whose state lives in a struct, not in a future,
//! so a `select!` that drops a read keeps everything already read. Each read is one
//! `read`/`fill_buf` call, which tokio documents as cancel-safe.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::time::{Instant, sleep, sleep_until};
use tokio_util::sync::CancellationToken;

/// How long the pipe readers get after the process has exited (or been killed). The plan's
/// "`ControlPersist` can hold pipes open" trap: some OpenSSH versions leave the backgrounded
/// master holding the first ssh's stdout and stderr, so reading to EOF would hang after the
/// child is gone. What was read by then is kept.
pub const PIPE_GRACE: Duration = Duration::from_millis(250);

/// How long to wait for a `SIGKILL`ed leader to be reaped before leaving it to tokio's
/// background reaper. `SIGKILL` cannot be caught, so this only matters for a process stuck
/// in uninterruptible sleep.
const REAP_LIMIT: Duration = Duration::from_secs(5);

/// A process group that is killed when this is dropped, unless disarmed.
#[derive(Debug)]
pub struct Group(Option<Pid>);

impl Group {
    /// `SIGKILL` to every process in the group. Errors (the group is already empty) are
    /// ignored: the goal is that nothing is left, and nothing is.
    pub fn kill(&self) {
        if let Some(pgid) = self.0 {
            let _ = kill_process_group(pgid, Signal::KILL);
        }
    }

    /// Stops the guard from killing the group: called once the leader has been reaped.
    pub const fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A spawned process and its group guard.
#[derive(Debug)]
pub struct Spawned {
    pub child: Child,
    pub group: Group,
}

/// Spawns `cmd` in a new process group, with stdout and stderr piped, stdin piped when
/// `with_stdin` and `/dev/null` otherwise.
///
/// The command is run directly (`execve` of `cmd`'s program): never through a shell.
pub fn spawn(mut cmd: Command, with_stdin: bool) -> io::Result<Spawned> {
    // A new session: `setsid()` in the child, which makes it the leader of a new session
    // AND of a new process group whose id is its pid (so no separate `process_group(0)`:
    // a process that is already a group leader cannot call setsid). A new session has no
    // controlling terminal, so opening `/dev/tty` fails (ENXIO) and nothing it runs, ssh
    // or a ProxyJump hop from ~/.ssh/config, can prompt on the operator's terminal or
    // read keystrokes meant for the TUI.
    use std::os::unix::process::CommandExt as _;
    cmd.as_std_mut().setsid(true);
    cmd.kill_on_drop(true)
        .stdin(if with_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn()?;
    // A pid that does not fit (impossible on every supported platform) leaves the guard
    // unarmed rather than killing group 0, which is *our* group.
    let pgid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    Ok(Spawned {
        child,
        group: Group(pgid),
    })
}

/// Kills the group, then the leader, and reaps it. Returns its status when reaped in time.
pub async fn kill_and_reap(child: &mut Child, group: &mut Group) -> Option<ExitStatus> {
    group.kill();
    let _ = child.start_kill();
    match tokio::time::timeout(REAP_LIMIT, child.wait()).await {
        Ok(Ok(status)) => {
            group.disarm();
            Some(status)
        }
        // Left armed: the drop guard kills the group again, and tokio reaps the leader.
        _ => None,
    }
}

/// The first `cap` bytes of a stream.
#[derive(Debug)]
pub struct Head {
    pub buf: Vec<u8>,
    scratch: Vec<u8>,
    cap: usize,
    /// More than `cap` bytes arrived.
    pub truncated: bool,
    /// EOF, an error, or the cap reached: do not read again.
    pub done: bool,
}

impl Head {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            scratch: vec![0; 8192],
            cap,
            truncated: false,
            done: false,
        }
    }

    /// One read. Cancel-safe.
    pub async fn step<R: AsyncRead + Unpin>(&mut self, r: &mut R) {
        match r.read(&mut self.scratch).await {
            Ok(0) | Err(_) => self.done = true,
            Ok(n) => {
                let room = self.cap - self.buf.len();
                if n > room {
                    self.buf.extend_from_slice(&self.scratch[..room]);
                    self.truncated = true;
                    self.done = true;
                } else {
                    self.buf.extend_from_slice(&self.scratch[..n]);
                }
            }
        }
    }
}

/// The last `cap` bytes of a stream. The useful part of an error is at its end (ssh's
/// "Host key verification failed.", nix's `error:` line), so stderr keeps its tail.
#[derive(Debug)]
pub struct Tail {
    pub buf: Vec<u8>,
    scratch: Vec<u8>,
    cap: usize,
    pub done: bool,
}

impl Tail {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            scratch: vec![0; 8192],
            cap,
            done: false,
        }
    }

    /// One read. Cancel-safe.
    pub async fn step<R: AsyncRead + Unpin>(&mut self, r: &mut R) {
        match r.read(&mut self.scratch).await {
            Ok(0) | Err(_) => self.done = true,
            Ok(n) => {
                self.buf.extend_from_slice(&self.scratch[..n]);
                if self.buf.len() > self.cap {
                    let excess = self.buf.len() - self.cap;
                    self.buf.drain(..excess);
                }
            }
        }
    }

    /// Reads an optional pipe; a missing one is already at EOF.
    pub async fn step_opt(&mut self, r: &mut Option<ChildStderr>) {
        match r {
            Some(r) => self.step(r).await,
            None => self.done = true,
        }
    }
}

/// One line from [`Lines`].
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A line, without its `\n`.
    Full(Vec<u8>),
    /// A line longer than the cap, discarded.
    TooLong,
}

/// Newline-separated lines of at most `cap` bytes each, read without ever holding more than
/// `cap` bytes of one line.
#[derive(Debug)]
pub struct Lines<R> {
    inner: BufReader<R>,
    cur: Vec<u8>,
    cap: usize,
    skipping: bool,
    pub done: bool,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    pub fn new(r: R, cap: usize) -> Self {
        Self {
            inner: BufReader::new(r),
            cur: Vec::new(),
            cap,
            skipping: false,
            done: false,
        }
    }

    /// The next line, or `None` at EOF (or on a read error). Cancel-safe: the only await is
    /// `fill_buf`, and every bit of state is updated synchronously after it.
    pub async fn next(&mut self) -> Option<Line> {
        loop {
            let Ok(buf) = self.inner.fill_buf().await else {
                self.done = true;
                return None;
            };
            if buf.is_empty() {
                self.done = true;
                if self.skipping {
                    self.skipping = false;
                    return Some(Line::TooLong);
                }
                if self.cur.is_empty() {
                    return None;
                }
                return Some(Line::Full(std::mem::take(&mut self.cur)));
            }
            let (piece, used, ends) = buf
                .iter()
                .position(|&b| b == b'\n')
                .map_or((buf, buf.len(), false), |i| (&buf[..i], i + 1, true));
            if !self.skipping {
                if self.cur.len() + piece.len() > self.cap {
                    self.skipping = true;
                    self.cur = Vec::new();
                } else {
                    self.cur.extend_from_slice(piece);
                }
            }
            self.inner.consume(used);
            if ends {
                if std::mem::take(&mut self.skipping) {
                    return Some(Line::TooLong);
                }
                return Some(Line::Full(std::mem::take(&mut self.cur)));
            }
        }
    }
}

/// How a [`run`] ended.
#[derive(Debug)]
pub enum End {
    /// The process exited (or was killed by someone else).
    Exited(ExitStatus),
    /// The deadline passed; the group was killed.
    TimedOut,
    /// The token was cancelled; the group was killed.
    Cancelled,
    /// stdout passed its cap; the group was killed, since nothing after the cap is used.
    Overflow,
}

/// Output caps for [`run`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bytes of stdout kept (the head).
    pub stdout: usize,
    /// Bytes of stderr kept (the tail).
    pub stderr: usize,
}

/// What [`run`] collected.
#[derive(Debug)]
pub struct Ran {
    pub end: End,
    /// The status, when the process was reaped (always, unless it would not die).
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
}

/// Runs `cmd` to completion, feeding it `stdin` (when given), until it exits, `deadline`
/// passes, `cancel` fires or stdout passes its cap.
///
/// After the process has exited or been killed, the pipe readers get [`PIPE_GRACE`] to
/// finish, and are then dropped with what they read. A cancelled run returns at once after
/// the kill and reap.
///
/// # Errors
///
/// The spawn error. Everything after a successful spawn is reported in [`Ran`].
pub async fn run(
    cmd: Command,
    stdin: Option<Vec<u8>>,
    limits: Limits,
    deadline: Instant,
    cancel: &CancellationToken,
) -> io::Result<Ran> {
    let Spawned {
        mut child,
        mut group,
    } = spawn(cmd, stdin.is_some())?;
    let mut out_pipe: Option<ChildStdout> = child.stdout.take();
    let mut err_pipe: Option<ChildStderr> = child.stderr.take();
    let stdin_pipe = child.stdin.take();
    let mut out = Head::new(limits.stdout);
    let mut err = Tail::new(limits.stderr);

    // Written concurrently with reading, so a child that prints before it reads its input
    // cannot deadlock against us. A child that never reads stdin (a host that never
    // answers) leaves this pending until the end; EPIPE is ignored.
    let writer = async move {
        if let (Some(mut pipe), Some(data)) = (stdin_pipe, stdin) {
            let _ = pipe.write_all(&data).await;
            let _ = pipe.shutdown().await;
        }
    };
    tokio::pin!(writer);
    let mut written = false;

    let end = loop {
        tokio::select! {
            biased;
            status = child.wait() => match status {
                Ok(s) => break End::Exited(s),
                // Waiting itself failed (it should not): stop as for a cancellation, but
                // report it, so nothing is left running.
                Err(e) => {
                    kill_and_reap(&mut child, &mut group).await;
                    return Err(e);
                }
            },
            () = cancel.cancelled() => break End::Cancelled,
            () = sleep_until(deadline) => break End::TimedOut,
            () = &mut writer, if !written => written = true,
            () = step_out(&mut out, &mut out_pipe), if !out.done => {
                if out.truncated {
                    break End::Overflow;
                }
            }
            () = err.step_opt(&mut err_pipe), if !err.done => {}
        }
    };

    let status = match end {
        End::Exited(s) => {
            group.disarm();
            Some(s)
        }
        End::TimedOut | End::Cancelled | End::Overflow => {
            kill_and_reap(&mut child, &mut group).await
        }
    };

    if !matches!(end, End::Cancelled) {
        let grace = sleep(PIPE_GRACE);
        tokio::pin!(grace);
        while !(out.done && err.done) {
            tokio::select! {
                biased;
                () = &mut grace => break,
                () = step_out(&mut out, &mut out_pipe), if !out.done => {}
                () = err.step_opt(&mut err_pipe), if !err.done => {}
            }
        }
    }

    Ok(Ran {
        end,
        status,
        stdout_truncated: out.truncated,
        stdout: out.buf,
        stderr: err.buf,
    })
}

async fn step_out(out: &mut Head, pipe: &mut Option<ChildStdout>) {
    match pipe {
        Some(p) => out.step(p).await,
        None => out.done = true,
    }
}

/// A human description of a spawn failure, naming the program.
pub fn spawn_error(what: &str, program: &std::ffi::OsStr, e: &io::Error) -> String {
    let shown = program.to_string_lossy();
    let on_path = !shown.contains('/');
    match e.kind() {
        io::ErrorKind::NotFound if on_path => format!("{what}: {shown} not found on PATH"),
        io::ErrorKind::NotFound => format!("{what}: {shown} not found"),
        io::ErrorKind::PermissionDenied => format!("{what}: {shown} is not executable"),
        _ => format!("{what}: could not run {shown}: {e}"),
    }
}

/// How a process ended, for an error message.
pub fn describe_status(status: Option<ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    status.map_or_else(
        || "would not die".to_owned(),
        |s| match (s.code(), s.signal()) {
            (Some(c), _) => format!("exited {c}"),
            (None, Some(sig)) => format!("killed by signal {sig}"),
            (None, None) => "ended".to_owned(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_capped_and_split() {
        let data: &[u8] = b"short\n0123456789abcdef\nok\nlast-without-newline";
        let mut l = Lines::new(data, 8);
        assert_eq!(l.next().await, Some(Line::Full(b"short".to_vec())));
        assert_eq!(l.next().await, Some(Line::TooLong));
        assert_eq!(l.next().await, Some(Line::Full(b"ok".to_vec())));
        assert_eq!(l.next().await, Some(Line::TooLong));
        assert_eq!(l.next().await, None);
        assert!(l.done);
    }

    #[tokio::test]
    async fn head_and_tail_keep_their_ends() {
        let data = vec![b'x'; 20_000];
        let mut h = Head::new(10_000);
        let mut r: &[u8] = &data;
        while !h.done {
            h.step(&mut r).await;
        }
        assert_eq!(h.buf.len(), 10_000);
        assert!(h.truncated);

        let mut data = vec![b'a'; 20_000];
        data.extend_from_slice(b"end");
        let mut t = Tail::new(100);
        let mut r: &[u8] = &data;
        while !t.done {
            t.step(&mut r).await;
        }
        assert_eq!(t.buf.len(), 100);
        assert!(t.buf.ends_with(b"aend"));
    }
}
