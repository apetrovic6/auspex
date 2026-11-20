//! The ssh probe: one round trip per machine per refresh, through the system `ssh`.
//!
//! Every phase's argv comes from [`ssh_args`]. A control check precedes the probe; a cold
//! destination authenticates by running `true` under a separate approval deadline first.
//! The probe then appends [`PROBE_REMOTE_COMMAND`] and sends its script on stdin.
//! Nothing from a target, a machine name or a config value reaches argv any other way, and
//! no shell is involved locally: the program is executed directly.
//!
//! ssh never prompts: it runs in a new session (no controlling terminal, so `/dev/tty`
//! cannot be opened), with `SSH_ASKPASS` removed and `SSH_ASKPASS_REQUIRE=never`, and stdin
//! is `/dev/null` for control operations or the pipe carrying the probe script.
//! A `ProxyJump` hop from `~/.ssh/config` inherits these prompt restrictions too.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;
use std::{fmt, fs, io};

use auspex_domain::fleet::{
    ConnectTimeout, MachineClass, PROBE_REMOTE_COMMAND, SshOptions, Target, ssh_args,
};
use auspex_domain::observe::{ProbeExit, ProbeOutcome, parse_probe, probe_script};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::process::{End, Limits, OnExit, Ran, command, describe_status, run, spawn_error};
use crate::text::lossy;

/// The default ssh program: `ssh`, looked up on `PATH`.
pub const DEFAULT_SSH: &str = "ssh";

/// The default deadline for running the script after authentication.
pub const DEFAULT_PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// How much stdout a probe keeps: 256 KiB.
///
/// A real probe prints about a kilobyte; the
/// largest field, `failed_units`, is a JSON list of unit names. Beyond the cap the run is
/// killed and the output treated as cut off, as the domain treats any probe without its end
/// marker: a host that prints without end is not a probe.
pub const STDOUT_CAP: usize = 256 * 1024;

/// How much of stderr's *tail* a probe keeps: 16 KiB. Only the last line is shown ("Host
/// key verification failed."); the rest is login banners and warnings.
pub const STDERR_CAP: usize = 16 * 1024;

/// The longest control directory accepted, in bytes.
///
/// A unix socket path holds at most 104 bytes on macOS (108 on Linux), including the
/// terminating NUL. ssh's control path is `<dir>/%C`, `%C` expanding to 40 hex characters,
/// and while setting up a master ssh binds `<path>.<16 random characters>` before renaming
/// it into place. So `<dir>` + 1 + 40 + 1 + 16 + NUL must fit in 104: 45 bytes.
pub const MAX_CONTROL_DIR_LEN: usize = 104 - 1 - (1 + 40) - (1 + 16);

/// The shared default for control sockets and evaluator machine-list files.
pub(crate) fn default_runtime_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if Path::new(&d).is_absolute() => Path::new(&d).join("auspex"),
        _ => std::env::temp_dir().join(format!("auspex-{}", rustix::process::getuid().as_raw())),
    }
}

/// Creates a private directory without changing an existing directory's permissions.
/// Socket-path restrictions belong to `ControlDir`, not to regular runtime files.
pub(crate) fn prepare_private_dir(path: &Path, kind: &str) -> Result<(), String> {
    let shown = path.display();
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {kind} {shown}: {e}")),
    }
    // Inspect after creating too: creation does not guarantee what remains at this path.
    let meta =
        fs::symlink_metadata(path).map_err(|e| format!("cannot inspect {kind} {shown}: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{kind} {shown} is a symlink; refusing to use it"));
    }
    if !meta.is_dir() {
        return Err(format!("{kind} {shown} exists and is not a directory"));
    }
    let me = rustix::process::getuid().as_raw();
    if meta.uid() != me {
        return Err(format!(
            "{kind} {shown} is owned by uid {}, not by this user (uid {me}); \
             refusing to use it",
            meta.uid()
        ));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode != 0o700 {
        return Err(format!(
            "{kind} {shown} has mode {mode:04o}, expected 0700; refusing to use it"
        ));
    }
    Ok(())
}

/// A control-socket directory that has passed [`ControlDir::prepare`]'s checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlDir {
    path: String,
}

/// Why a control directory was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlDirError(pub String);

impl fmt::Display for ControlDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ControlDirError {}

impl ControlDir {
    /// The directory to use, from the environment: `$XDG_RUNTIME_DIR/auspex`, or, when
    /// `XDG_RUNTIME_DIR` is unset, empty or relative, `auspex-<uid>` under the system
    /// temporary directory. This is the one place this module reads the environment
    /// (besides `PATH`, which `process::command` reads to find ssh).
    ///
    /// On macOS, where `XDG_RUNTIME_DIR` is usually unset and `TMPDIR` is long, the fallback
    /// is too long for a socket path and [`ControlDir::prepare`] refuses it (review finding
    /// F9, out of scope: auspex runs on a Linux desktop).
    #[must_use]
    pub fn default_path() -> PathBuf {
        default_runtime_path()
    }

    /// Creates `path` with mode 0700 if it is missing, and checks it either way.
    ///
    /// Refused unless it is a directory (not a symlink to one), owned by the current user,
    /// with mode exactly 0700, and short enough for a socket path
    /// ([`MAX_CONTROL_DIR_LEN`]). An existing directory with the wrong mode is refused
    /// rather than fixed: auspex did not make it, so something else owns its meaning.
    ///
    /// # Errors
    ///
    /// [`ControlDirError`] saying which check failed.
    pub fn prepare(path: &Path) -> Result<Self, ControlDirError> {
        let shown = path.display();
        let s = path
            .to_str()
            .ok_or_else(|| ControlDirError(format!("control directory {shown} is not UTF-8")))?;
        if s.len() > MAX_CONTROL_DIR_LEN {
            return Err(ControlDirError(format!(
                "control directory {s} is {} bytes long; ssh's control socket path must fit \
                 in a unix socket path (104 bytes, with %C expanded to 40 characters and ssh's \
                 temporary suffix), so it can be at most {MAX_CONTROL_DIR_LEN} bytes. Set \
                 XDG_RUNTIME_DIR to a shorter directory",
                s.len()
            )));
        }
        prepare_private_dir(path, "control directory").map_err(ControlDirError)?;
        // ssh_args refuses whitespace, control characters and '%'; asking it now turns that
        // into an error here rather than on every probe.
        let probe_target = Target::parse("x").map_err(|e| ControlDirError(e.to_string()))?;
        ssh_args(&probe_target, &SshOptions::new(s)).map_err(|e| ControlDirError(e.to_string()))?;
        Ok(Self { path: s.to_owned() })
    }

    /// The directory.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.path
    }
}

/// Everything a probe needs besides the target.
#[derive(Clone)]
pub struct ProbeParams<'a> {
    /// The ssh program: [`DEFAULT_SSH`] (looked up on `PATH`), or a path.
    pub ssh_program: &'a OsStr,
    /// The checked control directory.
    pub control_dir: &'a ControlDir,
    /// ssh's `ConnectTimeout`.
    pub connect_timeout: ConnectTimeout,
    /// How long an idle master remains available, in seconds.
    pub control_persist: u64,
    /// The separate connect/authentication allowance, per connect phase: it ends this long after
    /// the later of the phase's start and the moment the most recent phase in the control
    /// directory ended, so a one-at-a-time agent gives each waiting phase a fresh window as
    /// the earlier prompts are answered (see `ConnectWindow`).
    pub approval_deadline: Duration,
    /// Shell-side phase signal: true on entry, false on every exit, including drop.
    pub on_approval: Option<&'a (dyn Fn(bool) + Sync)>,
    /// The script deadline after authentication ([`DEFAULT_PROBE_DEADLINE`] by default).
    /// Past it, ssh's process group is killed and the probe is `ProbeExit::TimedOut`.
    pub deadline: Duration,
}

impl<'a> ProbeParams<'a> {
    /// Defaults: `ssh`, 5 s connect, 60 s approval, 10 s script, 600 s persistence.
    #[must_use]
    pub fn new(control_dir: &'a ControlDir) -> Self {
        Self {
            ssh_program: OsStr::new(DEFAULT_SSH),
            control_dir,
            connect_timeout: ConnectTimeout::DEFAULT,
            control_persist: 600,
            approval_deadline: Duration::from_secs(60),
            on_approval: None,
            deadline: DEFAULT_PROBE_DEADLINE,
        }
    }
}

/// The exact argv (after the program) a probe of `target` runs.
///
/// # Errors
///
/// The domain's refusal of the control directory, as text. [`ControlDir::prepare`] has
/// already run the same check, so this does not happen for a prepared directory.
pub fn probe_argv(target: &Target, params: &ProbeParams<'_>) -> Result<Vec<String>, String> {
    let mut options = SshOptions::new(params.control_dir.as_str());
    options.connect_timeout = params.connect_timeout;
    options.control_persist = params.control_persist;
    let mut args = ssh_args(target, &options).map_err(|e| e.to_string())?;
    args.push(PROBE_REMOTE_COMMAND.to_owned());
    Ok(args)
}

/// What `s` runs: the program (resolved to an absolute path now, as every program here is) and
/// the argv of [`SshOperation::Interactive`] for `target`.
///
/// This is the one ssh in the crate that is **not** run by the crate: the TUI hands it the
/// terminal (a tty, the operator's environment with `SSH_ASKPASS` and the agent as they are,
/// the operator's `~/.ssh/config`), which every probe above is built to avoid. The crate only
/// says what to run, so the argv still comes from the domain and nothing else reaches it.
///
/// # Errors
///
/// The program cannot be found (or is a relative path), or the domain refuses the control
/// directory (not so for a prepared one).
pub fn interactive_command(
    program: &OsStr,
    target: &Target,
    control_dir: &ControlDir,
) -> Result<(PathBuf, Vec<String>), String> {
    let path = crate::process::resolve_in(program, std::env::var_os("PATH").as_deref())
        .map_err(|e| spawn_error("run", program, &e))?;
    let mut options = SshOptions::new(control_dir.as_str());
    options.operation = auspex_domain::fleet::SshOperation::Interactive;
    let args = ssh_args(target, &options).map_err(|e| e.to_string())?;
    Ok((path, args))
}

/// A running connect phase: counted while it runs, and on the way out (return, cancel or
/// drop alike) it stops counting and tells the phases still waiting that one has ended
/// ([`ConnectWindow`]).
struct PhaseGuard<'a> {
    signal: Option<&'a (dyn Fn(bool) + Sync)>,
    window: Arc<ConnectWindow>,
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        if let Some(signal) = self.signal {
            signal(false);
        }
        self.window.ended.send_replace(Some(Instant::now()));
    }
}

// What ssh hashes into `%C` (besides the local host name, the same for every call here): the
// remote user as written, the host lowercased, and the port, 22 when none is given. Keying on
// those rather than on the target as spelled makes one socket one flight: `root@H`, `root@h`
// and `root@h:22` are one socket, and ssh itself cannot tell them apart. Query options are
// not part of the key, because `%C` ignores them. The directory is, so that separately
// prepared handles for the same directory still share a flight. Coordination is process-wide
// for the same reason. Entries exist only while calls hold the flight.
//
// The key is built from the target as auspex has it, and does not read `~/.ssh/config`, which
// ssh applies before it expands `%C`: a `Port`, `User` or `HostName` there changes the socket
// without changing the key. Two spellings of one socket then make two flights, which is
// harmless (the second finds the first's master, so no second prompt). One key for two sockets
// is possible too: `Port 2222` in the config for `h`, with fleet targets `root@h` and
// `root@h:22`. Then the second target's probe finds no master of its own and authenticates
// by itself under `timeouts.probe`, which costs at most one extra prompt. Exact keying would
// need `ssh -G` (one more process per cold host) and was not worth that.
#[derive(PartialEq, Eq, Hash)]
struct ControlKey {
    directory: String,
    user: Option<String>,
    host: String,
    port: u16,
}

/// ssh's default port, which `%C` hashes when the target names none.
const DEFAULT_SSH_PORT: u16 = 22;

/// What the callers of one flight share. Only "a master now exists" is: a failure belongs to
/// the target that had it. Target options (`PubkeyAuthentication=no`, `IdentitiesOnly`,
/// `AddressFamily`, ...) change whether authentication succeeds without changing the socket,
/// so one target's refusal says nothing about another target on the same socket.
enum ConnectionState {
    /// No known master. A cancelled or failed owner leaves this, so the next caller runs its
    /// own connect phase.
    Pending,
    MasterUp,
}

struct ConnectionFlight {
    key: Arc<ControlKey>,
    state: tokio::sync::Mutex<ConnectionState>,
}

static CONNECTIONS: LazyLock<Mutex<HashMap<Arc<ControlKey>, Weak<ConnectionFlight>>>> =
    LazyLock::new(Mutex::default);

impl Drop for ConnectionFlight {
    fn drop(&mut self) {
        let mut connections = CONNECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if connections
            .get(&self.key)
            .is_some_and(|entry| entry.strong_count() == 0)
        {
            connections.remove(&self.key);
        }
    }
}

fn connection_flight(target: &Target, control_dir: &ControlDir) -> Arc<ConnectionFlight> {
    let key = Arc::new(ControlKey {
        directory: control_dir.as_str().to_owned(),
        user: target.user().map(str::to_owned),
        host: target.host().to_ascii_lowercase(),
        port: target.port().unwrap_or(DEFAULT_SSH_PORT),
    });
    let mut connections = CONNECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = connections.entry(Arc::clone(&key)).or_default();
    if let Some(flight) = entry.upgrade() {
        drop(connections);
        return flight;
    }
    let flight = Arc::new(ConnectionFlight {
        key,
        state: tokio::sync::Mutex::new(ConnectionState::Pending),
    });
    *entry = Arc::downgrade(&flight);
    drop(connections);
    flight
}

/// When the last connect phase in a control directory ended, and who is told.
///
/// **What is serialised is the approval window, not the phases.** A connect phase is the step
/// that may need the agent's approval, and an agent such as Bitwarden shows its prompts one at
/// a time. Every phase used to start at once with a window (`approval_deadline`) of its own
/// from its own start, so later prompts appeared with their window nearly gone, were answered
/// too late, and the answer was thrown away with the killed ssh. Now a phase's window ends
/// `approval_deadline` after the later of its own start and the moment the most recent phase
/// in the directory ended (approved, refused, failed, timed out or cancelled), and it is
/// worked out again whenever another phase ends. Each finished approval therefore gives the
/// ones still waiting a fresh window, which is what a one-at-a-time agent needs.
///
/// The phases themselves run concurrently. A first version held one slot through the whole
/// phase, and that serialised work that needs no agent (the TCP connect, a down host's
/// `ConnectTimeout`, a stalled authentication): down or stalled hosts delayed every cold host
/// behind them, and in the same order on every refresh. Now a down host fails at
/// `ConnectTimeout` in parallel, and a stalled host blocks nobody: it only times out on its
/// own window. The agent keeps its own queue of prompts.
///
/// Nothing here makes a phase wait past `timeouts.settle`: the core's deadline cancels it.
///
/// Per control directory rather than one static: the agent is the shared resource, and auspex
/// prepares one control directory per run, so in the program this is process-wide. Tests give
/// every case a directory of its own, and a global one would make their timing assertions
/// depend on which other test happens to have a phase ending. The window is per process: two
/// auspex processes (a TUI and a `--json` run) do not see each other's phases.
struct ConnectWindow {
    ended: watch::Sender<Option<Instant>>,
}

static CONNECT_WINDOWS: LazyLock<Mutex<HashMap<String, Arc<ConnectWindow>>>> =
    LazyLock::new(Mutex::default);

fn connect_window(control_dir: &ControlDir) -> Arc<ConnectWindow> {
    // Never removed: bounded by the number of control directories, which is one.
    Arc::clone(
        CONNECT_WINDOWS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(control_dir.as_str().to_owned())
            .or_insert_with(|| {
                Arc::new(ConnectWindow {
                    ended: watch::channel(None).0,
                })
            }),
    )
}

/// Resolves, with `true`, when a connect phase that started at `start` has used up its window;
/// with `false`, when `done` is cancelled first. Its deadline moves later each time another
/// phase in the directory ends.
async fn window_expired(
    window: &ConnectWindow,
    start: Instant,
    approval_deadline: Duration,
    done: &CancellationToken,
) -> bool {
    let mut ended = window.ended.subscribe();
    loop {
        let last_end = *ended.borrow_and_update();
        let from = match last_end {
            Some(at) if at > start => at,
            _ => start,
        };
        tokio::select! {
            biased;
            () = done.cancelled() => return false,
            // Another phase ended: work the deadline out again.
            _ = ended.changed() => {}
            () = tokio::time::sleep_until(from + approval_deadline) => return true,
        }
    }
}

fn approval_timeout() -> ProbeOutcome {
    ProbeOutcome::Unreachable {
        reason: "ssh connection/authentication timed out (agent approval may be pending)"
            .to_owned(),
    }
}

/// Makes sure a master exists for `target`'s control socket: `None` when one does, or the
/// outcome to report when none could be made.
///
/// Callers for one socket take turns, and only the one holding the turn runs ssh. A caller
/// waits for its turn with no timer of its own: the holder is bounded (by `ConnectTimeout`
/// for the check, by `approval_deadline` for the connect phase, plus the kill), and so is the
/// whole refresh, by `timeouts.settle`, which cancels. A timer on the wait cut off waiters
/// whose holder went on to succeed. When the holder fails, each waiter runs a connect phase
/// of its own, because the failure may be the holder's own.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired, whether waiting for the socket's turn or while ssh ran;
/// what ssh started has been killed and reaped, and the next caller finds
/// the flight [`ConnectionState::Pending`].
pub(crate) async fn connect(
    target: &Target,
    params: &ProbeParams<'_>,
    cancel: &CancellationToken,
) -> Result<Option<ProbeOutcome>, Cancelled> {
    let flight = connection_flight(target, params.control_dir);
    let mut state = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Cancelled),
        state = flight.state.lock() => state,
    };
    if matches!(&*state, ConnectionState::MasterUp) {
        return Ok(None);
    }
    let outcome = connect_once(target, params, cancel).await?;
    if outcome.is_none() {
        *state = ConnectionState::MasterUp;
    }
    drop(state);
    Ok(outcome)
}

/// A check never connects; only a missing master starts the separately bounded approval.
async fn connect_once(
    target: &Target,
    params: &ProbeParams<'_>,
    cancel: &CancellationToken,
) -> Result<Option<ProbeOutcome>, Cancelled> {
    use auspex_domain::fleet::SshOperation;

    for operation in [SshOperation::Check, SshOperation::Connect] {
        let mut options = SshOptions::new(params.control_dir.as_str());
        options.connect_timeout = params.connect_timeout;
        options.control_persist = params.control_persist;
        options.operation = operation;
        let args = match ssh_args(target, &options) {
            Ok(args) => args,
            Err(e) => {
                return Ok(Some(ProbeOutcome::Failed {
                    reason: e.to_string(),
                }));
            }
        };
        // The connect phase is the one that may need the agent: it counts as waiting while it
        // runs, and its end moves the other phases' windows ([`ConnectWindow`]). The check
        // needs no approval and is not counted.
        let _phase = (operation == SshOperation::Connect).then(|| {
            if let Some(signal) = params.on_approval {
                signal(true);
            }
            PhaseGuard {
                signal: params.on_approval,
                window: connect_window(params.control_dir),
            }
        });
        let mut cmd = match command(params.ssh_program) {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(Some(ProbeOutcome::Unreachable {
                    reason: spawn_error("cannot connect", params.ssh_program, &e),
                }));
            }
        };
        cmd.args(&args)
            .env_remove("SSH_ASKPASS")
            .env("SSH_ASKPASS_REQUIRE", "never");
        // `auto` recovers stale sockets and lets cross-process race losers run `true`
        // and exit. Only ssh's own ControlPersist path detaches a reusable master;
        // unlike -N -f, there is no socketless, unbounded background fallback.
        let limits = Limits {
            stdout: STDOUT_CAP,
            stderr: STDERR_CAP,
            memory: None,
        };
        let ran = if operation == SshOperation::Check {
            run(
                cmd,
                None,
                OnExit::LeaveGroup,
                limits,
                Instant::now() + Duration::from_secs(params.connect_timeout.seconds()),
                cancel,
            )
            .await
        } else {
            run_connect(cmd, limits, params, cancel).await
        };
        let ran = match ran {
            Ok(ran) => ran,
            Err(e) => {
                return Ok(Some(ProbeOutcome::Unreachable {
                    reason: spawn_error("cannot connect", params.ssh_program, &e),
                }));
            }
        };
        match ran.end {
            End::Cancelled => return Err(Cancelled),
            End::Exited(status) if status.success() => return Ok(None),
            _ if operation == SshOperation::Check => {}
            End::TimedOut => {
                return Ok(Some(approval_timeout()));
            }
            End::Overflow => {
                return Ok(Some(ProbeOutcome::Failed {
                    reason: "ssh approval output cut off".to_owned(),
                }));
            }
            End::Exited(status) => {
                return Ok(Some(parse_probe("", &lossy(&ran.stderr), exit_of(status))));
            }
        }
    }
    Ok(None)
}

/// Runs the connect phase's ssh under its approval window ([`ConnectWindow`]).
///
/// [`run`] takes a fixed deadline, and this one moves: another phase ending gives this one a
/// fresh window. So `run` gets a deadline it never reaches and a token of its own, and
/// [`window_expired`] cancels that token when the window is used up. `run` then kills and
/// reaps the group exactly as it does for a timeout, and the result is reported as one
/// ([`End::TimedOut`]). The caller's own cancel stays a cancel.
///
/// `LeaveGroup`, not `KillGroup`: with `ControlPersist` ssh forks the master, and a
/// `ProxyJump` or `ProxyCommand` hop from `~/.ssh/config` (forked by this leader before
/// authentication, so in its group) carries that master's transport after the leader has
/// exited. Killing the group then would kill the hop, the master would lose its connection,
/// and every refresh would prompt again for every machine behind a jump host. A cancel or a
/// timeout still kills the group.
async fn run_connect(
    cmd: tokio::process::Command,
    limits: Limits,
    params: &ProbeParams<'_>,
    cancel: &CancellationToken,
) -> io::Result<Ran> {
    /// Past any phase: the real bound is the window.
    const NEVER: Duration = Duration::from_secs(24 * 60 * 60);
    let start = Instant::now();
    let window = connect_window(params.control_dir);
    let local = cancel.child_token();
    let done = CancellationToken::new();
    let (ran, expired) = tokio::join!(
        async {
            let ran = run(cmd, None, OnExit::LeaveGroup, limits, start + NEVER, &local).await;
            done.cancel();
            ran
        },
        async {
            let expired = window_expired(&window, start, params.approval_deadline, &done).await;
            if expired {
                local.cancel();
            }
            expired
        },
    );
    let mut ran = ran?;
    if expired && matches!(ran.end, End::Cancelled) && !cancel.is_cancelled() {
        ran.end = End::TimedOut;
    }
    Ok(ran)
}

/// Probes one machine. Serves the core's `Effect::Probe`.
///
/// Runs `ssh` with [`probe_argv`], the class's probe script on stdin, and hands the output
/// to `parse_probe`. stdout and stderr are decoded **lossily** (invalid UTF-8 becomes
/// U+FFFD) because the domain takes text and a remote host's output is not guaranteed to be
/// UTF-8. stdout is capped at [`STDOUT_CAP`] and stderr's tail at [`STDERR_CAP`].
///
/// Every way it can end is a [`ProbeOutcome`]:
///
/// - past `params.deadline`: the process group is killed and the outcome is
///   `parse_probe(…, ProbeExit::TimedOut)` (unreachable, with ssh's last stderr line);
/// - ssh could not be run: unreachable, saying why ("ssh not found on PATH");
/// - killed by a signal it did not get from us: the domain's "probe killed by signal N";
/// - stdout over the cap: failed, "probe output cut off", unless the end marker came first.
///
/// After ssh exits, the pipes get 250 ms to drain (a `ControlPersist` master may hold
/// them open), and what was read is used.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; ssh's process group has been killed and reaped.
pub async fn probe(
    target: &Target,
    class: MachineClass,
    params: &ProbeParams<'_>,
    cancel: &CancellationToken,
) -> Result<ProbeOutcome, Cancelled> {
    if let Some(outcome) = connect(target, params, cancel).await? {
        return Ok(outcome);
    }
    let deadline = Instant::now() + params.deadline;
    let args = match probe_argv(target, params) {
        Ok(a) => a,
        Err(reason) => return Ok(ProbeOutcome::Failed { reason }),
    };
    let mut cmd = match command(params.ssh_program) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ProbeOutcome::Unreachable {
                reason: spawn_error("cannot probe", params.ssh_program, &e),
            });
        }
    };
    cmd.args(args.iter().map(OsString::from));
    // No prompt, anywhere: ssh runs in a session with no controlling terminal
    // (`process::spawn`), and with no askpass program it could open a window with instead.
    // `SSH_ASKPASS_REQUIRE=never` also covers a ProxyJump hop, which inherits this
    // environment; BatchMode (in `ssh_args`) covers the destination itself.
    cmd.env_remove("SSH_ASKPASS")
        .env("SSH_ASKPASS_REQUIRE", "never");
    let script = probe_script(class).as_bytes().to_vec();
    let limits = Limits {
        stdout: STDOUT_CAP,
        stderr: STDERR_CAP,
        memory: None,
    };
    // When ssh exits on its own, the rest of its process group is left alone, unlike
    // every other program here. The reason first given (that a `ControlPersist` master
    // stays in the group) is wrong for OpenSSH: `control_persist_detach()` calls
    // `daemon()`, which runs `setsid()`, so the master is in a session of its own and no
    // group kill reaches it. What remains is caution: a live master could not be observed
    // in review (no ssh connections were allowed), and whatever else ssh leaves in its
    // group has not been seen to linger. (A `ProxyJump` hop is the case that matters: it
    // carries a detached master's transport and ends with the master, not with this
    // process, so leaving it alone is load-bearing; see `connect_once`.)
    // A cancel, a timeout or a drop still kills the whole group.
    let ran = match run(
        cmd,
        Some(script),
        OnExit::LeaveGroup,
        limits,
        deadline,
        cancel,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(ProbeOutcome::Unreachable {
                reason: spawn_error("cannot probe", params.ssh_program, &e),
            });
        }
    };
    let exit = match ran.end {
        End::Cancelled => return Err(Cancelled),
        End::TimedOut => ProbeExit::TimedOut,
        End::Exited(s) => exit_of(s),
        End::Overflow => ran.status.map_or(ProbeExit::Signal(9), exit_of),
    };
    let stdout = lossy(&ran.stdout);
    let stderr = lossy(&ran.stderr);
    let outcome = parse_probe(&stdout, &stderr, exit);
    Ok(match outcome {
        ProbeOutcome::Failed { .. } if ran.stdout_truncated => ProbeOutcome::Failed {
            reason: format!(
                "probe output cut off: more than {STDOUT_CAP} bytes on stdout ({})",
                describe_status(ran.status)
            ),
        },
        other => other,
    })
}

fn exit_of(s: std::process::ExitStatus) -> ProbeExit {
    match (s.code(), s.signal()) {
        (Some(c), _) => ProbeExit::Code(c),
        (None, Some(sig)) => ProbeExit::Signal(sig),
        // Neither a code nor a signal is not a thing on unix; called a failure.
        (None, None) => ProbeExit::Code(-1),
    }
}
