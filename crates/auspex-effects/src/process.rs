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
//!   only then does tokio reap the leader in the background (`kill_on_drop`): `Spawned`
//!   declares `group` before `child`, so it is dropped first;
//! - when it exits on its own ([`Spawned::exited`]), what is left of its group is killed
//!   too ([`OnExit::KillGroup`]): `nix-eval-jobs` workers outlive a crashed leader
//!   otherwise (review finding F3). ssh is the exception ([`OnExit::LeaveGroup`]); see
//!   `ssh::probe`.
//!
//! The group is always killed **before** the leader is reaped, so the group id cannot have
//! been reused: the exit is detected with `waitid(WEXITED | WNOWAIT | WNOHANG)`, which
//! leaves the leader a zombie (its pid, and so the group id, still taken), woken by
//! `SIGCHLD` with a slow tick as a fallback; only then is it reaped.
//!
//! Programs are resolved to an absolute path by [`command`] before anything runs, skipping
//! empty and relative `PATH` entries (review finding F6): with a child working directory
//! inside the fleet repository, a relative entry would run a `git` committed to it.
//!
//! Output is read through capped readers whose state lives in a struct, not in a future,
//! so a `select!` that drops a read keeps everything already read. Each read is one
//! `read`/`fill_buf` call, which tokio documents as cancel-safe.

use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
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

/// What happens to the rest of a process group when its leader exits on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExit {
    /// Kill whatever is left of the group (`nix`, `nix-eval-jobs`, `git`).
    KillGroup,
    /// Leave it (ssh: see `ssh::probe`).
    LeaveGroup,
}

/// Waits for a child's exit without reaping it.
#[derive(Debug)]
struct ExitWatch {
    pid: Option<Pid>,
    sigchld: Option<tokio::signal::unix::Signal>,
    tick: tokio::time::Interval,
}

impl ExitWatch {
    fn new(pid: Option<Pid>) -> Self {
        // Registered before the first check, so a SIGCHLD in between is not lost. If the
        // signal driver is unavailable the tick alone still finds the exit.
        let sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self { pid, sigchld, tick }
    }

    /// Returns once the child has exited, leaving it a zombie. Cancel-safe: every await is
    /// a signal or tick receive.
    async fn exited(&mut self) -> io::Result<()> {
        use rustix::process::{WaitId, WaitIdOptions, waitid};
        let Some(pid) = self.pid else {
            return Ok(());
        };
        loop {
            match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            ) {
                Ok(Some(_)) => return Ok(()),
                Ok(None) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            match &mut self.sigchld {
                Some(s) => {
                    tokio::select! {
                        _ = s.recv() => {}
                        _ = self.tick.tick() => {}
                    }
                }
                None => {
                    self.tick.tick().await;
                }
            }
        }
    }
}

/// A spawned process, its group guard, and how it is waited for.
#[derive(Debug)]
pub struct Spawned {
    // `group` is declared before `child`, so it is dropped first: on the drop path the group
    // is killed while the leader is still unreaped, and only then does `child`'s
    // `kill_on_drop` hand the leader to tokio's reaper (review finding N3; fields drop in
    // declaration order).
    pub group: Group,
    pub child: Child,
    watch: ExitWatch,
    on_exit: OnExit,
}

impl Spawned {
    /// Waits for the leader to exit on its own, kills the rest of its group if
    /// [`OnExit::KillGroup`], then reaps it. Cancel-safe.
    pub async fn exited(&mut self) -> io::Result<ExitStatus> {
        self.watch.exited().await?;
        if self.on_exit == OnExit::KillGroup {
            // The leader is a zombie: the group id is still taken.
            self.group.kill();
        }
        let status = self.child.wait().await?;
        self.group.disarm();
        Ok(status)
    }

    /// Kills the group, then the leader, and reaps it. Returns its status when reaped in
    /// time.
    pub async fn kill_and_reap(&mut self) -> Option<ExitStatus> {
        self.group.kill();
        let _ = self.child.start_kill();
        match tokio::time::timeout(REAP_LIMIT, self.child.wait()).await {
            Ok(Ok(status)) => {
                self.group.disarm();
                Some(status)
            }
            // Left armed: the drop guard kills the group again, and tokio reaps the leader.
            _ => None,
        }
    }
}

/// A [`Command`] for `program`, resolved to an absolute path now.
///
/// A program with a `/` must be an absolute path. A bare name is looked up in `PATH`,
/// skipping empty and relative entries, which would otherwise be resolved against the
/// child's working directory (inside the fleet repository, for git). This is the one place
/// the crate reads `PATH`.
///
/// # Errors
///
/// `NotFound` when nothing on `PATH` matches, `InvalidInput` for a relative path.
pub fn command(program: &OsStr) -> io::Result<Command> {
    resolve_in(program, std::env::var_os("PATH").as_deref()).map(Command::new)
}

fn resolve_in(program: &OsStr, path: Option<&OsStr>) -> io::Result<PathBuf> {
    let p = Path::new(program);
    if program.as_encoded_bytes().contains(&b'/') {
        if p.is_absolute() {
            return Ok(p.to_path_buf());
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a relative program path; give an absolute path or a bare name",
        ));
    }
    if program.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "empty program name",
        ));
    }
    let found = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(program))
        .find(|c| {
            std::fs::metadata(c).is_ok_and(|m| {
                m.is_file()
                    && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
            })
        });
    found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found on PATH"))
}

/// Spawns `cmd` in a new session, with stdout and stderr piped, stdin piped when
/// `with_stdin` and `/dev/null` otherwise.
///
/// The command is never run through a shell. With `memory`, an absolute-resolved
/// util-linux `prlimit` sets both address-space limits before execing the target; failure
/// to find the launcher or set the limit never runs the target. Build `cmd` with
/// [`command`], configuring only arguments, environment overrides and working directory.
pub fn spawn(
    mut cmd: Command,
    with_stdin: bool,
    on_exit: OnExit,
    memory: Option<u64>,
) -> io::Result<Spawned> {
    if let Some(bytes) = memory {
        cmd = limited_command(&cmd, bytes)?;
    }
    // A new session: `setsid()` in the child, which makes it the leader of a new session
    // AND of a new process group whose id is its pid (so no separate `process_group(0)`:
    // a process that is already a group leader cannot call setsid). A new session has no
    // controlling terminal, so opening `/dev/tty` fails (ENXIO) and nothing it runs, ssh
    // or a ProxyJump hop from ~/.ssh/config, can prompt on the operator's terminal or
    // read keystrokes meant for the TUI.
    cmd.as_std_mut().setsid(true);
    cmd.kill_on_drop(true)
        .stdin(if with_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The exit watch registers for SIGCHLD before the child can exit.
    let mut watch = ExitWatch::new(None);
    let child = cmd.spawn()?;
    // A pid that does not fit (impossible on every supported platform) leaves the guard
    // unarmed rather than killing group 0, which is *our* group.
    let pgid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    watch.pid = pgid;
    Ok(Spawned {
        group: Group(pgid),
        child,
        watch,
        on_exit,
    })
}

/// Launching through `prlimit` closes the post-spawn race: the target's loader and first
/// instruction already have the limit. It execs, so sessions, group guards and exit
/// watching still refer to the same leader.
#[cfg(target_os = "linux")]
fn limited_command(cmd: &Command, bytes: u64) -> io::Result<Command> {
    let original = cmd.as_std();
    if !Path::new(original.get_program()).is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a bounded command needs an absolute program path",
        ));
    }
    // A packaged build names util-linux's `prlimit` by its absolute store path at compile
    // time (`AUSPEX_PRLIMIT`, set in nix/auspex/package.nix), so the installed program needs
    // nothing on `PATH` and changes no child's `PATH`. Elsewhere (dev shell, CI) it is found
    // on `PATH` like every other program here.
    //
    // Not `NotFound` on failure: `spawn_error` would report that as the *target* missing
    // ("git not found on PATH"), which sends the operator after the wrong program.
    let launcher = match option_env!("AUSPEX_PRLIMIT") {
        Some(p) if Path::new(p).is_absolute() => Ok(Command::new(p)),
        _ => command(OsStr::new("prlimit")),
    };
    let mut limited = launcher.map_err(|e| {
        io::Error::other(format!(
            "the util-linux prlimit memory-limit launcher is {e}; git is not run without it"
        ))
    })?;
    limited
        .arg(format!("--as={bytes}:{bytes}"))
        .arg("--")
        .arg(original.get_program())
        .args(original.get_args());
    if let Some(dir) = original.get_current_dir() {
        limited.current_dir(dir);
    }
    for (name, value) in original.get_envs() {
        if let Some(value) = value {
            limited.env(name, value);
        } else {
            limited.env_remove(name);
        }
    }
    Ok(limited)
}

/// Without the pre-exec launcher, fail closed instead of running an unbounded target.
#[cfg(not(target_os = "linux"))]
fn limited_command(_: &Command, _: u64) -> io::Result<Command> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform cannot launch a memory-bounded command",
    ))
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
    /// An address-space limit for the process, in bytes; `None` for none.
    pub memory: Option<u64>,
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
    on_exit: OnExit,
    limits: Limits,
    deadline: Instant,
    cancel: &CancellationToken,
) -> io::Result<Ran> {
    let mut sp = spawn(cmd, stdin.is_some(), on_exit, limits.memory)?;
    let mut out_pipe: Option<ChildStdout> = sp.child.stdout.take();
    let mut err_pipe: Option<ChildStderr> = sp.child.stderr.take();
    let stdin_pipe = sp.child.stdin.take();
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
            status = sp.exited() => match status {
                Ok(s) => break End::Exited(s),
                // Waiting itself failed (it should not): stop as for a cancellation, but
                // report it, so nothing is left running.
                Err(e) => {
                    sp.kill_and_reap().await;
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
        End::Exited(s) => Some(s),
        End::TimedOut | End::Cancelled | End::Overflow => sp.kill_and_reap().await,
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_unusable_limit_never_runs_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut cmd = command(OsStr::new("sh")).unwrap();
        cmd.args(["-c", "printf ran > \"$1\"", "sh"]).arg(&marker);
        let ran = run(
            cmd,
            None,
            OnExit::KillGroup,
            Limits {
                stdout: 1024,
                stderr: 1024,
                memory: Some(0),
            },
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!matches!(ran.end, End::Exited(status) if status.success()));
        assert!(
            !marker.exists(),
            "the target executed before its limit was installed"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_limiter_preserves_arguments_environment_and_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = command(OsStr::new("sh")).unwrap();
        cmd.current_dir(dir.path())
            .env("AUSPEX_LIMIT_TEST", "retained")
            .env_remove("AUSPEX_LIMIT_REMOVED")
            .args([
                "-c",
                "test \"$AUSPEX_LIMIT_TEST\" = retained && test -z \"${AUSPEX_LIMIT_REMOVED+x}\" && test \"$1\" = 'two words' && printf '%s' \"$PWD\"",
                "sh",
                "two words",
            ]);
        let ran = run(
            cmd,
            None,
            OnExit::KillGroup,
            Limits {
                stdout: 4096,
                stderr: 1024,
                memory: Some(64 << 20),
            },
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(ran.end, End::Exited(status) if status.success()));
        assert_eq!(ran.stdout, dir.path().as_os_str().as_encoded_bytes());
    }

    /// Review finding F6: empty and relative `PATH` entries are never searched, so a
    /// program committed to the fleet repository (the child's working directory) cannot
    /// be run in place of git.
    #[test]
    fn programs_resolve_only_through_absolute_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("tool");
        std::fs::write(&exe, "").unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let plain = bin.join("plain");
        std::fs::write(&plain, "").unwrap();

        let path = std::env::join_paths([bin.as_path()]).unwrap();
        assert_eq!(resolve_in(OsStr::new("tool"), Some(&path)).unwrap(), exe);
        // Not executable: not a match.
        assert!(resolve_in(OsStr::new("plain"), Some(&path)).is_err());
        // Only relative or empty entries: nothing, even though `bin/tool` exists relative
        // to wherever the child would start.
        for p in [":", "bin", ".:bin:", ""] {
            let e = resolve_in(OsStr::new("tool"), Some(OsStr::new(p))).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::NotFound, "{p:?}");
        }
        assert!(resolve_in(OsStr::new("tool"), None).is_err());
        // A relative entry that does lead to `bin` from the current directory (`../` up to
        // the root, then the absolute path): still never searched.
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count());
        let rel = format!("{up}{}", bin.display().to_string().trim_start_matches('/'));
        assert!(std::path::Path::new(&rel).join("tool").exists(), "{rel}");
        let e = resolve_in(OsStr::new("tool"), Some(OsStr::new(&rel))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        // A path is used as given when absolute, refused when relative.
        assert_eq!(resolve_in(exe.as_os_str(), None).unwrap(), exe);
        let e = resolve_in(OsStr::new("bin/tool"), Some(&path)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(spawn_error("x", OsStr::new("bin/tool"), &e).contains("relative"));
    }

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
