//! The ssh probe: one round trip per machine per refresh, through the system `ssh`.
//!
//! The argument vector is exactly `auspex_domain::fleet::ssh_args(target, …)` followed by
//! [`PROBE_REMOTE_COMMAND`], and the probe script for the machine's class goes on stdin.
//! Nothing from a target, a machine name or a config value reaches argv any other way, and
//! no shell is involved locally: the program is executed directly.
//!
//! ssh never prompts: it runs in a new session (no controlling terminal, so `/dev/tty`
//! cannot be opened), with `SSH_ASKPASS` removed and `SSH_ASKPASS_REQUIRE=never`, and stdin
//! is the pipe carrying the script. That holds for a `ProxyJump` hop from `~/.ssh/config`
//! too, which inherits all three.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fmt, fs, io};

use auspex_domain::fleet::{
    ConnectTimeout, MachineClass, PROBE_REMOTE_COMMAND, SshOptions, Target, ssh_args,
};
use auspex_domain::observe::{ProbeExit, ProbeOutcome, parse_probe, probe_script};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::process::{End, Limits, OnExit, command, describe_status, run, spawn_error};
use crate::text::lossy;

/// The default ssh program: `ssh`, looked up on `PATH`.
pub const DEFAULT_SSH: &str = "ssh";

/// The plan's default for the whole probe: connect, run and print, within 10 seconds.
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
#[derive(Debug, Clone)]
pub struct ProbeParams<'a> {
    /// The ssh program: [`DEFAULT_SSH`] (looked up on `PATH`), or a path.
    pub ssh_program: &'a OsStr,
    /// The checked control directory.
    pub control_dir: &'a ControlDir,
    /// ssh's `ConnectTimeout`.
    pub connect_timeout: ConnectTimeout,
    /// The whole probe's deadline ([`DEFAULT_PROBE_DEADLINE`] by default). Past it, ssh's
    /// process group is killed and the probe is `ProbeExit::TimedOut`.
    pub deadline: Duration,
}

impl<'a> ProbeParams<'a> {
    /// The defaults: `ssh` from `PATH`, a 5 s connect timeout, a 10 s deadline.
    #[must_use]
    pub fn new(control_dir: &'a ControlDir) -> Self {
        Self {
            ssh_program: OsStr::new(DEFAULT_SSH),
            control_dir,
            connect_timeout: ConnectTimeout::DEFAULT,
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
    let mut args = ssh_args(target, &options).map_err(|e| e.to_string())?;
    args.push(PROBE_REMOTE_COMMAND.to_owned());
    Ok(args)
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
    // group (a `ProxyJump` hop is its child and ends with it) has not been seen to linger.
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
