//! The ssh probe against a fake `ssh` (a script passed as the program), and the
//! control-directory checks. Nothing here contacts a machine.

#![cfg(unix)]

mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _, symlink};
use std::path::Path;
use std::time::{Duration, Instant};

use auspex_domain::fleet::{
    ConnectTimeout, MachineClass, PROBE_REMOTE_COMMAND, SshOptions, Target, ssh_args,
};
use auspex_domain::observe::{ProbeOutcome, probe_script};
use auspex_effects::ssh::{ControlDir, MAX_CONTROL_DIR_LEN, ProbeParams, STDOUT_CAP, probe};
use auspex_effects::{CancellationToken, Cancelled};
use common::{cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};

const SYSTEM: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-25.05";

fn probe_output() -> String {
    format!("auspex_probe=1\\nprobe=nixos\\ncurrent_system={SYSTEM}\\nauspex_end=1\\n")
}

fn control(dir: &Path) -> ControlDir {
    ControlDir::prepare(&dir.join("c")).unwrap()
}

/// Legacy script tests exercise the probe over an already-authenticated master.
fn warm_script(t: &common::Tools, dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    t.script(
        dir,
        name,
        &format!("for arg; do [ \"$arg\" = '-O' ] && exit 0; done\n{body}"),
    )
}

/// A fake master appears only after the simulated approval. All probes need that master.
fn cold_script(t: &common::Tools, dir: &Path, delay: u64) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh-cold",
        &format!(
            r#"for arg; do
  case "$arg" in
    -O) [ -f '@dir@/master' ]; exit $?;;
    true)
      [ "${{SSH_ASKPASS-unset}}|${{SSH_ASKPASS_REQUIRE-unset}}" = 'unset|never' ] || exit 73
      if (exec 3</dev/tty) 2>/dev/null; then exit 74; fi
      echo $$ >> '@dir@/connections'
      @sleep@ {delay} &
      echo $! >> '@dir@/children'
      wait
      : > '@dir@/master'
      exit 0;;
  esac
done
[ -f '@dir@/master' ] || exit 255
printf '{}'
"#,
            probe_output()
        ),
    )
}

#[tokio::test]
async fn approval_has_its_own_deadline_and_a_warm_master_is_reused() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = cold_script(&t, dir.path(), 15);
    let cd = control(dir.path());
    let target = Target::parse("h").unwrap();
    let phases = std::sync::Mutex::new(Vec::new());
    let signal = |waiting| phases.lock().unwrap().push(waiting);
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.deadline = Duration::from_secs(10);
    params.approval_deadline = Duration::from_secs(60);
    params.on_approval = Some(&signal);
    for _ in 0..2 {
        let result = probe(
            &target,
            MachineClass::Nixos,
            &params,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(result, ProbeOutcome::Observed(_)), "{result:?}");
    }
    assert_eq!(*phases.lock().unwrap(), [true, false]);
    assert_eq!(
        fs::read_to_string(dir.path().join("connections"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn approval_timeout_is_distinct_and_reaps_the_waiter() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = cold_script(&t, dir.path(), 15);
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.approval_deadline = Duration::from_secs(5);
    let result = probe(
        &Target::parse("h").unwrap(),
        MachineClass::Nixos,
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let pids = wait_for_pids(&dir.path().join("connections"), 1);
    let children = wait_for_pids(&dir.path().join("children"), 1);
    let left = still_alive_after_wait(&children);
    cleanup(&children);
    assert!(!common::pid_exists(pids[0]), "approval ssh was not reaped");
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(
        result,
        ProbeOutcome::Unreachable {
            reason: "ssh connection/authentication timed out (agent approval may be pending)"
                .to_owned()
        }
    );
}

#[tokio::test]
async fn cancelling_approval_kills_and_reaps_and_clears_the_signal() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = cold_script(&t, dir.path(), 30);
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let phases = std::sync::Mutex::new(Vec::new());
    let signal = |waiting| phases.lock().unwrap().push(waiting);
    params.on_approval = Some(&signal);
    let cancel = CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let path = dir.path().join("children");
        tokio::task::spawn_blocking(move || {
            wait_for_pids(&path, 1);
            cancel.cancel();
        })
    };
    let result = probe(
        &Target::parse("h").unwrap(),
        MachineClass::Nixos,
        &params,
        &cancel,
    )
    .await;
    canceller.await.unwrap();
    let pids = wait_for_pids(&dir.path().join("connections"), 1);
    let children = wait_for_pids(&dir.path().join("children"), 1);
    let left = still_alive_after_wait(&children);
    cleanup(&children);
    assert_eq!(result, Err(Cancelled));
    assert!(!common::pid_exists(pids[0]), "approval ssh was not reaped");
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(*phases.lock().unwrap(), [true, false]);
}

#[tokio::test]
async fn review_two_concurrent_probes_to_one_destination_start_one_approval() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = t.script(
        dir.path(),
        "ssh-concurrent",
        &format!(
            r#"echo "$*" >> '@dir@/argv'
for arg; do
  case "$arg" in
    -O) [ -f '@dir@/master' ]; exit $?;;
    true)
      echo $$ >> '@dir@/connections'
      @sleep@ 2
      : > '@dir@/master'
      exit 0;;
  esac
done
[ -f '@dir@/master' ] || exit 255
printf '{}'"#,
            probe_output()
        ),
    );
    let cd = control(dir.path());
    let target = Target::parse("root@h").unwrap();
    // A second prepared handle and different transport options still name the same socket.
    let cd2 = ControlDir::prepare(Path::new(cd.as_str())).unwrap();
    let target2 = Target::parse("root@h?Compression=yes").unwrap();
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let mut params2 = params.clone();
    params2.control_dir = &cd2;
    let cancel = CancellationToken::new();
    let (a, b) = tokio::join!(
        probe(&target, MachineClass::Nixos, &params, &cancel),
        probe(&target2, MachineClass::Nixos, &params2, &cancel),
    );
    assert!(matches!(a.unwrap(), ProbeOutcome::Observed(_)));
    assert!(matches!(b.unwrap(), ProbeOutcome::Observed(_)));
    let argv = fs::read_to_string(dir.path().join("argv")).unwrap();
    let connections = fs::read_to_string(dir.path().join("connections"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(connections, 1, "two approvals for one destination:\n{argv}");
}

#[tokio::test]
async fn control_persist_reaches_every_phase_and_stale_sockets_recover() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    fs::write(dir.path().join("master"), "stale\n").unwrap();
    let program = t.script(
        dir.path(),
        "ssh-stale",
        &format!(
            r#"echo "$*" >> '@dir@/argv'
for arg; do
case "$arg" in
  -O) IFS= read -r state < '@dir@/master'; [ "$state" = live ]; exit $?;;
  true)
    case "$*" in *'ControlMaster=auto'*) ;; *) exit 76;; esac
    printf 'live\n' > '@dir@/master'
    exit 0;;
esac
done
IFS= read -r state < '@dir@/master'
[ "$state" = live ] || exit 255
printf '{}'"#,
            probe_output()
        ),
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.control_persist = 1234;
    let result = probe(
        &Target::parse("h").unwrap(),
        MachineClass::Nixos,
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(result, ProbeOutcome::Observed(_)), "{result:?}");
    let argv = fs::read_to_string(dir.path().join("argv")).unwrap();
    let lines: Vec<_> = argv.lines().collect();
    assert_eq!(lines.len(), 3, "{argv}");
    assert!(lines[0].contains(" -O check "), "{argv}");
    assert!(lines[1].ends_with(" -- h true"), "{argv}");
    assert!(lines[2].ends_with(" -- h /bin/sh -s"), "{argv}");
    for line in lines {
        assert!(line.contains(" -o ControlPersist=1234s "), "{line}");
    }
}

#[tokio::test]
async fn review_a_down_host_fails_at_connect_timeout_not_approval() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = t.script(
        dir.path(),
        "ssh-down",
        r#"echo "$*" >> '@dir@/argv'
for arg; do
case "$arg" in
  -O) exit 255;;
  ConnectTimeout=1) timeout=1;;
esac
done
if [ "$timeout" != 1 ]; then exit 77; fi
echo attempt >> '@dir@/attempts'
@sleep@ 1
echo 'ssh: connect to host h port 22: Connection timed out' >&2
exit 255"#,
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.connect_timeout = ConnectTimeout::new(1).unwrap();
    params.approval_deadline = Duration::from_secs(60);
    let start = Instant::now();
    let result = probe(
        &Target::parse("h").unwrap(),
        MachineClass::Nixos,
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(
        result,
        ProbeOutcome::Unreachable {
            reason: "ssh: connect to host h port 22: Connection timed out".to_owned()
        }
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("attempts")).unwrap(),
        "attempt\n",
        "failed Connect must not fall through to a second connection attempt"
    );
}

#[tokio::test]
async fn cancelling_a_single_flight_waiter_does_not_cancel_the_owner() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = t.script(
        dir.path(),
        "ssh-waiter",
        &format!(
            r#"for arg; do
case "$arg" in
  -O) [ -f '@dir@/master' ]; exit $?;;
  true)
    echo $$ >> '@dir@/connections'
    while [ ! -f '@dir@/release' ]; do @sleep@ 0.01; done
    : > '@dir@/master'
    exit 0;;
esac
done
printf '{}'"#,
            probe_output()
        ),
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let target = Target::parse("h").unwrap();
    let owner = CancellationToken::new();
    let waiter = CancellationToken::new();
    let connections = dir.path().join("connections");
    let (a, b) = tokio::join!(
        probe(&target, MachineClass::Nixos, &params, &owner),
        async {
            tokio::task::spawn_blocking(move || wait_for_pids(&connections, 1))
                .await
                .unwrap();
            let result = probe(&target, MachineClass::Nixos, &params, &waiter);
            tokio::pin!(result);
            tokio::select! {
                result = &mut result => panic!("waiter finished before cancellation: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            waiter.cancel();
            let result = result.await;
            fs::write(dir.path().join("release"), "").unwrap();
            result
        },
    );
    assert!(matches!(a.unwrap(), ProbeOutcome::Observed(_)));
    assert_eq!(b, Err(Cancelled));
    assert_eq!(
        fs::read_to_string(dir.path().join("connections"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

async fn run_probe(
    program: &Path,
    control: &ControlDir,
    target: &Target,
    deadline: Duration,
) -> (Result<ProbeOutcome, Cancelled>, Duration) {
    let mut params = ProbeParams::new(control);
    params.ssh_program = program.as_os_str();
    params.deadline = deadline;
    let start = Instant::now();
    let r = probe(
        target,
        MachineClass::Nixos,
        &params,
        &CancellationToken::new(),
    )
    .await;
    (r, start.elapsed())
}

#[tokio::test]
async fn a_healthy_probe_gets_exactly_ssh_args_and_the_script_on_stdin() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    // The fake lives where a shell would misread its path: run through `sh -c`, the
    // space and the `;` would split it.
    let bin = dir.path().join("bin dir; x");
    fs::create_dir(&bin).unwrap();
    let prog = warm_script(
        &t,
        &bin,
        "ssh",
        &format!(
            r#"printf '%s\0' "$@" > '@dir@/argv'
while IFS= read -r l || [ -n "$l" ]; do printf '%s\n' "$l"; done > '@dir@/stdin'
printf '{}'"#,
            probe_output()
        ),
    );
    let cd = control(dir.path());
    // A target with a port and an option, so the argv has every part.
    let target = Target::parse("root@[fd00::1]:2222?IdentitiesOnly=yes").unwrap();
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = prog.as_os_str();
    params.connect_timeout = ConnectTimeout::new(7).unwrap();
    let outcome = probe(
        &target,
        MachineClass::Darwin,
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    let ProbeOutcome::Observed(obs) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(obs.current_system.unwrap().as_str(), SYSTEM);

    let mut options = SshOptions::new(cd.as_str());
    options.connect_timeout = ConnectTimeout::new(7).unwrap();
    let mut expected = ssh_args(&target, &options).unwrap();
    expected.push(PROBE_REMOTE_COMMAND.to_owned());
    let argv = fs::read(bin.join("argv")).unwrap();
    let argv: Vec<String> = argv
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8(a.to_vec()).unwrap())
        .collect();
    assert_eq!(argv, expected);
    assert_eq!(
        fs::read_to_string(bin.join("stdin")).unwrap(),
        probe_script(MachineClass::Darwin)
    );
}

#[tokio::test]
async fn exit_255_is_unreachable_with_ssh_s_last_line() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        "printf 'Warning: something\\nHost key verification failed.\\n' >&2\nexit 255",
    );
    let (r, _) = run_probe(
        &prog,
        &control(dir.path()),
        &Target::parse("h").unwrap(),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        r.unwrap(),
        ProbeOutcome::Unreachable {
            reason: "Host key verification failed.".to_owned()
        }
    );
}

#[tokio::test]
async fn a_host_that_never_answers_times_out_and_the_process_is_gone() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        "@sleep@ 30 &\necho $! >> '@dir@/pid'\necho $$ >> '@dir@/pid'\nprintf 'connecting...\\n' >&2\nwait",
    );
    let (r, took) = run_probe(
        &prog,
        &control(dir.path()),
        &Target::parse("h").unwrap(),
        Duration::from_millis(500),
    )
    .await;
    let pids = wait_for_pids(&dir.path().join("pid"), 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    let ProbeOutcome::Unreachable { reason } = r.unwrap() else {
        panic!()
    };
    assert_eq!(reason, "timed out: connecting...");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert!(left.is_empty(), "still running: {left:?}");
}

/// The plan's "`ControlPersist` can hold pipes open" trap: the ssh process exits, but a
/// background process it left holds stdout and stderr open.
#[tokio::test]
async fn a_grandchild_holding_stdout_does_not_hold_up_the_probe() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        &format!(
            "@sleep@ 30 &\necho $! > '@dir@/gpid'\nprintf '{}'\nexit 0",
            probe_output()
        ),
    );
    let (r, took) = run_probe(
        &prog,
        &control(dir.path()),
        &Target::parse("h").unwrap(),
        Duration::from_secs(10),
    )
    .await;
    let gpid = wait_for_pids(&dir.path().join("gpid"), 1);
    cleanup(&gpid);
    assert!(
        matches!(r.unwrap(), ProbeOutcome::Observed(_)),
        "the output read before the grace period ended is kept"
    );
    assert!(
        took < Duration::from_secs(2),
        "took {took:?}: waited for EOF"
    );
}

#[tokio::test]
async fn unbounded_output_is_capped_and_cut_off() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        "echo $$ > '@dir@/pid'\nprintf 'auspex_probe=1\\n'\nwhile :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n'; done",
    );
    let (r, took) = run_probe(
        &prog,
        &control(dir.path()),
        &Target::parse("h").unwrap(),
        Duration::from_secs(10),
    )
    .await;
    let pids = wait_for_pids(&dir.path().join("pid"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    let ProbeOutcome::Failed { reason } = r.unwrap() else {
        panic!()
    };
    assert!(
        reason.starts_with(&format!(
            "probe output cut off: more than {STDOUT_CAP} bytes"
        )),
        "{reason}"
    );
    // Stopped at the cap, not at the deadline.
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(left.is_empty(), "still running: {left:?}");
}

#[tokio::test]
async fn invalid_utf8_is_decoded_lossily() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        &format!(
            "printf 'auspex_probe=1\\nprobe=nixos\\ncurrent_system={SYSTEM}\\nconfiguration_revision=ab\\377cd\\nauspex_end=1\\n'\nprintf 'bad \\377 stderr\\n' >&2"
        ),
    );
    let (r, _) = run_probe(
        &prog,
        &control(dir.path()),
        &Target::parse("h").unwrap(),
        Duration::from_secs(5),
    )
    .await;
    let ProbeOutcome::Observed(obs) = r.unwrap() else {
        panic!()
    };
    assert_eq!(obs.configuration_revision.as_deref(), Some("ab\u{fffd}cd"));
    assert_eq!(obs.current_system.unwrap().as_str(), SYSTEM);
}

#[tokio::test]
async fn cancelling_kills_ssh_and_its_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let cd = control(dir.path());
    let target = Target::parse("h").unwrap();
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = prog.as_os_str();
    let cancel = CancellationToken::new();
    let pids_file = dir.path().join("pids");
    let canceller = {
        let cancel = cancel.clone();
        let pids_file = pids_file.clone();
        tokio::task::spawn_blocking(move || {
            wait_for_pids(&pids_file, 2);
            cancel.cancel();
        })
    };
    let start = Instant::now();
    let r = probe(&target, MachineClass::Nixos, &params, &cancel).await;
    canceller.await.unwrap();
    let pids = wait_for_pids(&pids_file, 2);
    // The leader (second line, ssh itself) is checked at once and must not exist at all,
    // not even as a zombie: the call reaps before returning. The grandchild was SIGKILLed
    // with the group; it is reparented, so its own reaping is someone else's and it is
    // given the usual wait.
    let leader_present = common::pid_exists(pids[1]);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(r, Err(Cancelled));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(
        !leader_present,
        "ssh was not reaped before the call returned"
    );
    assert!(left.is_empty(), "still running: {left:?}");
}

#[tokio::test]
async fn dropping_the_future_kills_ssh_and_its_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let cd = control(dir.path());
    let target = Target::parse("h").unwrap();
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = prog.as_os_str();
    params.deadline = Duration::from_secs(30);
    let cancel = CancellationToken::new();
    let fut = probe(&target, MachineClass::Nixos, &params, &cancel);
    // Give it time to start, then drop it.
    let r = tokio::time::timeout(Duration::from_millis(500), fut).await;
    assert!(r.is_err(), "the fake should still be running");
    let pids = wait_for_pids(&dir.path().join("pids"), 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "still running: {left:?}");
}

#[tokio::test]
async fn a_signalled_or_missing_ssh_has_a_reason() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let cd = control(dir.path());
    let target = Target::parse("h").unwrap();

    let prog = warm_script(&t, dir.path(), "ssh", "kill -TERM $$");
    let (r, _) = run_probe(&prog, &cd, &target, Duration::from_secs(5)).await;
    let ProbeOutcome::Failed { reason } = r.unwrap() else {
        panic!()
    };
    assert!(reason.contains("signal 15"), "{reason}");

    let (r, _) = run_probe(
        &dir.path().join("no-such-ssh"),
        &cd,
        &target,
        Duration::from_secs(5),
    )
    .await;
    let ProbeOutcome::Unreachable { reason } = r.unwrap() else {
        panic!()
    };
    assert!(reason.ends_with("no-such-ssh not found"), "{reason}");

    let mut params = ProbeParams::new(&cd);
    params.ssh_program = OsStr::new("auspex-test-no-such-ssh");
    let r = probe(
        &target,
        MachineClass::Nixos,
        &params,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(
        r.unwrap(),
        ProbeOutcome::Unreachable {
            reason: "cannot connect: auspex-test-no-such-ssh not found on PATH".to_owned()
        }
    );

    let not_exec = dir.path().join("plain");
    fs::write(&not_exec, "").unwrap();
    let (r, _) = run_probe(&not_exec, &cd, &target, Duration::from_secs(5)).await;
    let ProbeOutcome::Unreachable { reason } = r.unwrap() else {
        panic!()
    };
    assert!(reason.ends_with("is not executable"), "{reason}");
}

// ---------------------------------------------------------------------------------------
// The control directory
// ---------------------------------------------------------------------------------------

#[test]
fn the_control_directory_is_created_0700_and_reused() {
    let dir = tempdir();
    let path = dir.path().join("c");
    let cd = ControlDir::prepare(&path).unwrap();
    assert_eq!(cd.as_str(), path.to_str().unwrap());
    let meta = fs::symlink_metadata(&path).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o700);
    // A second run finds it and accepts it.
    ControlDir::prepare(&path).unwrap();
}

#[test]
fn a_control_directory_with_the_wrong_mode_is_refused() {
    let dir = tempdir();
    let path = dir.path().join("c");
    fs::DirBuilder::new().mode(0o755).create(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let e = ControlDir::prepare(&path).unwrap_err();
    assert!(e.0.contains("has mode 0755, expected 0700"), "{e}");
    // And it was not "fixed".
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o755
    );
}

#[test]
fn a_symlinked_control_directory_is_refused() {
    let dir = tempdir();
    let real = dir.path().join("real");
    fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
    let link = dir.path().join("c");
    symlink(&real, &link).unwrap();
    let e = ControlDir::prepare(&link).unwrap_err();
    assert!(e.0.contains("is a symlink"), "{e}");
}

#[test]
fn a_file_is_not_a_control_directory() {
    let dir = tempdir();
    let path = dir.path().join("c");
    fs::write(&path, "").unwrap();
    let e = ControlDir::prepare(&path).unwrap_err();
    assert!(e.0.contains("is not a directory"), "{e}");
}

#[test]
fn a_control_directory_owned_by_someone_else_is_refused() {
    use std::os::unix::fs::MetadataExt as _;
    let me = rustix::process::getuid().as_raw();
    // Any existing directory owned by another user will do; one cannot be made without
    // root. `/` is owned by root on every system this runs on, root excepted.
    let Some(theirs) = ["/", "/proc", "/dev", "/nix/store"]
        .iter()
        .map(Path::new)
        .find(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && m.uid() != me))
    else {
        eprintln!("SKIPPED: no directory owned by another user to test with (running as root?)");
        return;
    };
    let e = ControlDir::prepare(theirs).unwrap_err();
    assert!(e.0.contains("not by this user"), "{e}");
}

#[test]
fn a_control_directory_too_long_for_a_socket_path_is_refused() {
    let long = format!("/tmp/{}", "d".repeat(MAX_CONTROL_DIR_LEN));
    let e = ControlDir::prepare(Path::new(&long)).unwrap_err();
    assert!(e.0.contains("unix socket path"), "{e}");
    assert!(
        !Path::new(&long).exists(),
        "refused before creating anything"
    );
}

#[test]
fn the_default_control_directory_is_absolute() {
    assert!(ControlDir::default_path().is_absolute());
}

// ---------------------------------------------------------------------------------------
// No prompts: no controlling terminal, no askpass
// ---------------------------------------------------------------------------------------

/// The fields of `/proc/<pid>/stat` after the parenthesised command name.
fn stat_fields(line: &str) -> Vec<String> {
    line.rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The body of [`ssh_cannot_prompt`], run in a child test process whose environment has
/// `SSH_ASKPASS` set: the variable has to be in auspex's own environment for its removal
/// to show, and `set_var` is `unsafe` in edition 2024 (and would leak into every other
/// test on this process's threads). Does nothing unless started that way.
#[tokio::test]
async fn ssh_cannot_prompt_inner() {
    if std::env::var_os("AUSPEX_TEST_ASKPASS_INNER").is_none() {
        return;
    }
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "ssh",
        &format!(
            r#"if ( exec 3</dev/tty ) 2>/dev/null; then echo opened > '@dir@/tty'; else echo refused > '@dir@/tty'; fi
echo $$ > '@dir@/pid'
if [ -r /proc/$$/stat ]; then IFS= read -r st < /proc/$$/stat; printf '%s\n' "$st" > '@dir@/stat'; fi
printf '%s|%s\n' "${{SSH_ASKPASS-unset}}" "${{SSH_ASKPASS_REQUIRE-unset}}" > '@dir@/env'
printf '{}'"#,
            probe_output()
        ),
    );
    let (r, _) = run_probe(
        &prog,
        &control(dir.path()),
        &Target::parse("h").unwrap(),
        Duration::from_secs(5),
    )
    .await;
    assert!(matches!(r.unwrap(), ProbeOutcome::Observed(_)));
    let read = |f: &str| fs::read_to_string(dir.path().join(f)).unwrap();
    assert_eq!(
        read("env").trim(),
        "unset|never",
        "SSH_ASKPASS removed, REQUIRE=never"
    );
    assert_eq!(
        read("tty").trim(),
        "refused",
        "the fake could open /dev/tty"
    );
    if let Ok(stat) = fs::read_to_string(dir.path().join("stat")) {
        let pid = read("pid").trim().to_owned();
        let theirs = stat_fields(&stat);
        let ours = stat_fields(&fs::read_to_string("/proc/self/stat").unwrap());
        // After ')': state, ppid, pgrp, session, tty_nr.
        assert_eq!(theirs[3], pid, "ssh leads a session of its own");
        assert_ne!(theirs[3], ours[3], "not in the test's session");
        assert_eq!(theirs[2], pid, "and a process group of its own");
        assert_eq!(theirs[4], "0", "with no controlling terminal");
    } else {
        eprintln!("no /proc here: the session checks are skipped, the /dev/tty one ran");
    }
}

/// ssh, and any `ProxyJump` hop it starts, can never prompt on the operator's terminal: it
/// has no controlling terminal (a new session), `SSH_ASKPASS` is removed and
/// `SSH_ASKPASS_REQUIRE=never` is set. A fake ssh that tries to open `/dev/tty` must fail.
///
/// Where this test itself has a terminal, opening `/dev/tty` would succeed for a child in
/// our session, so the refusal is the new session's doing; where it has none (CI, the
/// sandbox), the session and `tty_nr` checks are what can fail.
#[test]
fn ssh_cannot_prompt() {
    if std::env::var_os("AUSPEX_TEST_ASKPASS_INNER").is_some() {
        return;
    }
    let have_tty = fs::File::open("/dev/tty").is_ok();
    eprintln!("this test process has a controlling terminal: {have_tty}");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ssh_cannot_prompt_inner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AUSPEX_TEST_ASKPASS_INNER", "1")
        .env("SSH_ASKPASS", "/usr/bin/auspex-test-askpass")
        .env("SSH_ASKPASS_REQUIRE", "force")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the inner test ran: {text}");
}

#[tokio::test]
async fn approval_master_retaining_pipes_does_not_hold_up_the_probe() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = t.script(
        dir.path(),
        "ssh-detach",
        &format!(
            r#"for arg; do
case "$arg" in
  -O) exit 1;;
  true)
    if IFS= read -r line; then exit 75; fi
    @sleep@ 30 &
    echo $! > '@dir@/master.pid'
    exit 0;;
esac
done
printf '{}'"#,
            probe_output()
        ),
    );
    let (result, elapsed) = run_probe(
        &program,
        &control(dir.path()),
        &Target::parse("h").unwrap(),
        Duration::from_secs(10),
    )
    .await;
    let pids = wait_for_pids(&dir.path().join("master.pid"), 1);
    // The stand-in for a `ProxyJump` hop carrying the detached master's transport. It is
    // in the connect leader's group and must outlive the leader: killing the group when
    // the leader exits on its own would drop the master's connection, and every refresh
    // would prompt again for every machine behind a jump host.
    let alive = pids.iter().all(|&p| common::alive(p));
    cleanup(&pids);
    assert!(
        alive,
        "the connect leader's process group was killed when it exited"
    );
    assert!(matches!(result.unwrap(), ProbeOutcome::Observed(_)));
    assert!(
        elapsed < Duration::from_secs(2),
        "master held pipes: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------------------
// One approval at a time (AUS-37)
//
// The operator's ssh agent shows its approval prompts one at a time. Every connect phase
// used to start at once, each with its own window from its own start, so the ninth prompt
// appeared when its window was nearly gone and was wasted.
// ---------------------------------------------------------------------------------------

/// Polls every future to completion, in order of the input. (No `futures` crate here.)
#[expect(
    clippy::future_not_send,
    reason = "a test helper, polled on the test's own task"
)]
async fn join_all<T>(
    futures: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = T> + '_>>>,
) -> Vec<T> {
    let mut futures: Vec<_> = futures.into_iter().map(Some).collect();
    let mut done: Vec<Option<T>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, out) in futures.iter_mut().zip(done.iter_mut()) {
            if let Some(f) = slot {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => {
                        *out = Some(v);
                        *slot = None;
                    }
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    done.into_iter()
        .map(|v| v.expect("polled to completion"))
        .collect()
}

/// An ssh agent that approves one connection at a time, each after `delay` seconds: a
/// connection that finds the agent busy waits for it, as a second prompt waits behind the
/// first. A destination's master appears when its connection was approved, and is shared by
/// every phase that names the destination.
///
/// `@dir@/events` gets "start <dest>" when a connection attempt starts (not when it is
/// approved) and "approved <dest>" when it is. `@dir@/overlap` gets a line if a second
/// prompt ever showed while one was showing: the agent's own invariant, checked with a
/// `running` marker made with `mkdir`, so the fake cannot quietly stop being one-at-a-time.
fn serial_agent(t: &common::Tools, dir: &Path, hosts: usize, delay: &str) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh-agent",
        &format!(
            r#"mode=probe
for arg; do
  prev2=$prev; prev=$arg
  [ "$arg" = -O ] && mode=check
  [ "$arg" = true ] && mode=connect
done
case "$mode" in
  check) dest=$prev;;
  *) dest=$prev2;;
esac
case "$mode" in
  check) [ -f "@dir@/master-$dest" ]; exit $?;;
  connect)
    echo "start $dest" >> '@dir@/events'
    : > "@dir@/started-$dest"
    # Do not approve until every connect phase is observably under way.
    n=1
    while [ "$n" -le {hosts} ]; do
      while [ ! -f "@dir@/started-root@h$n" ]; do @sleep@ 0.2; done
      n=$((n + 1))
    done
    # The agent: one prompt at a time.
    n=0
    until mkdir '@dir@/agent' 2>/dev/null; do
      n=$((n + 1)); [ $n -gt 1000 ] && exit 98
      @sleep@ 0.2
    done
    mkdir '@dir@/running' 2>/dev/null || echo "$dest" >> '@dir@/overlap'
    @sleep@ {delay}
    : > "@dir@/master-$dest"
    echo "approved $dest" >> '@dir@/events'
    rmdir '@dir@/running'
    rmdir '@dir@/agent'
    exit 0;;
esac
[ -f "@dir@/master-$dest" ] || exit 255
printf '{}'"#,
            probe_output()
        ),
    )
}

#[tokio::test]
async fn nine_hosts_behind_a_one_at_a_time_agent_all_connect_within_their_own_windows() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = serial_agent(&t, dir.path(), 9, "4");
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    // Each approval has 22 s of scheduling margin, but the fleet takes at least 36 s:
    // fixed windows from connect start must still fail. The fake gates on all starts,
    // rather than assuming every child was scheduled before the first sleep finished.
    params.approval_deadline = Duration::from_secs(26);
    let targets: Vec<Target> = (1..=9)
        .map(|n| Target::parse(&format!("root@h{n}")).unwrap())
        .collect();
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let results = join_all(
        targets
            .iter()
            .map(|target| {
                Box::pin(probe(target, MachineClass::Nixos, &params, &cancel))
                    as std::pin::Pin<Box<_>>
            })
            .collect(),
    )
    .await;
    let elapsed = start.elapsed();
    let outcomes: Vec<_> = results.into_iter().map(Result::unwrap).collect();
    let failed: Vec<_> = outcomes
        .iter()
        .filter(|o| !matches!(o, ProbeOutcome::Observed(_)))
        .collect();
    assert!(
        failed.is_empty(),
        "{} of 9 failed: {failed:?}",
        failed.len()
    );
    // One at a time at the agent: nine 4 s approvals back to back.
    assert!(elapsed >= Duration::from_secs(36), "{elapsed:?}");
    assert!(
        !dir.path().join("overlap").exists(),
        "the fake agent showed two prompts at once"
    );
    // What is serialised is the window, not the phases: all nine connections were under way
    // before the first approval.
    let events = read_lines(&dir.path().join("events"));
    let first_approval = events
        .iter()
        .position(|e| e.starts_with("approved"))
        .unwrap();
    assert_eq!(first_approval, 9, "{events:?}");
}

/// Blocks until `path` has `lines` lines (a fake's progress file), without holding up the
/// async runtime.
async fn wait_for_lines(path: &Path, lines: usize) -> Vec<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let start = Instant::now();
        loop {
            if let Ok(s) = fs::read_to_string(&path) {
                let got: Vec<String> = s.lines().map(str::to_owned).collect();
                if got.len() >= lines {
                    return got;
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{} never got {lines} lines",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    })
    .await
    .unwrap()
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// An ssh whose connect phase waits for a release file (`release-<destination>` or
/// `release`), so a test decides when an approval happens. `starts` gets a line when a
/// connect phase starts running. A destination containing `down` fails like an unreachable
/// host after one second (its `ConnectTimeout`), and one containing `stall` hangs without
/// a prompt until its own `release-<destination>` file appears. A destination's master is the file
/// `master-<destination>`, so a test can start with a warm one.
fn gated_agent(t: &common::Tools, dir: &Path) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh-gated",
        &format!(
            r#"mode=probe
for arg; do
  prev2=$prev; prev=$arg
  [ "$arg" = -O ] && mode=check
  [ "$arg" = true ] && mode=connect
done
case "$mode" in
  check) dest=$prev;;
  *) dest=$prev2;;
esac
case "$mode" in
  check) [ -f "@dir@/master-$dest" ]; exit $?;;
  connect)
    echo "$dest" >> '@dir@/starts'
    case "$dest" in
      *down*) @sleep@ 1
        echo "ssh: connect to host $dest port 22: Connection timed out" >&2
        exit 255;;
      # Hangs in authentication without a prompt: only its own release file ends it.
      *stall*)
        n=0
        while [ ! -f "@dir@/release-$dest" ]; do
          n=$((n + 1)); [ $n -gt 1000 ] && exit 99
          @sleep@ 0.02
        done
        : > "@dir@/master-$dest"
        exit 0;;
    esac
    # Bounded, so a failed test leaves no process behind.
    n=0
    while [ ! -f "@dir@/release-$dest" ] && [ ! -f '@dir@/release' ]; do
      n=$((n + 1)); [ $n -gt 1000 ] && exit 99
      @sleep@ 0.02
    done
    : > "@dir@/master-$dest"
    echo "$dest" >> '@dir@/approved'
    exit 0;;
esac
[ -f "@dir@/master-$dest" ] || exit 255
printf '{}'"#,
            probe_output()
        ),
    )
}

#[tokio::test]
async fn phases_run_together_are_counted_and_a_cancel_ends_only_its_own() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    let cd = control(dir.path());
    // The note's count: connect phases running.
    let count = AtomicUsize::new(0);
    let signal = |waiting| {
        if waiting {
            count.fetch_add(1, Ordering::SeqCst);
        } else {
            count.fetch_sub(1, Ordering::SeqCst);
        }
    };
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.on_approval = Some(&signal);
    let names = ["root@q1", "root@q2", "root@q3"];
    let targets: Vec<Target> = names.iter().map(|n| Target::parse(n).unwrap()).collect();
    let tokens: Vec<CancellationToken> = names.iter().map(|_| CancellationToken::new()).collect();
    let starts = dir.path().join("starts");
    let (results, ()) = tokio::join!(
        join_all(
            targets
                .iter()
                .zip(&tokens)
                .map(|(target, token)| {
                    Box::pin(probe(target, MachineClass::Nixos, &params, token))
                        as std::pin::Pin<Box<_>>
                })
                .collect(),
        ),
        async {
            // All three phases run at once: none waits for another to be done.
            wait_for_lines(&starts, 3).await;
            let t0 = Instant::now();
            while count.load(Ordering::SeqCst) != 3 {
                assert!(
                    t0.elapsed() < Duration::from_secs(10),
                    "count never reached 3"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // A refresh cancels one of them: it ends at once and stops counting, and the
            // others carry on.
            tokens[1].cancel();
            let t0 = Instant::now();
            while count.load(Ordering::SeqCst) != 2 {
                assert!(
                    t0.elapsed() < Duration::from_secs(2),
                    "a cancelled phase still counts"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            fs::write(dir.path().join("release"), "").unwrap();
        },
    );
    assert!(
        matches!(results[0], Ok(ProbeOutcome::Observed(_))),
        "{results:?}"
    );
    assert_eq!(results[1], Err(Cancelled));
    assert!(
        matches!(results[2], Ok(ProbeOutcome::Observed(_))),
        "{results:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_check_with_a_live_master_is_not_queued_behind_a_slow_approval() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    fs::write(dir.path().join("master-root@warm"), "").unwrap();
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let slow = Target::parse("root@slow").unwrap();
    let warm = Target::parse("root@warm").unwrap();
    let cancel = CancellationToken::new();
    let (slow_result, (warm_result, waited)) =
        tokio::join!(probe(&slow, MachineClass::Nixos, &params, &cancel), async {
            // The slow approval holds the slot.
            wait_for_lines(&dir.path().join("starts"), 1).await;
            let start = Instant::now();
            let result = probe(&warm, MachineClass::Nixos, &params, &cancel).await;
            let waited = start.elapsed();
            fs::write(dir.path().join("release"), "").unwrap();
            (result, waited)
        },);
    assert!(
        matches!(warm_result, Ok(ProbeOutcome::Observed(_))),
        "{warm_result:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "a warm machine waited {waited:?} behind another machine's approval"
    );
    assert!(matches!(slow_result, Ok(ProbeOutcome::Observed(_))));
    assert_eq!(read_lines(&dir.path().join("starts")), ["root@slow"]);
}

// AUS-37 fix round 1 (review F1). The first version held one slot through the whole connect
// phase, which serialised work that needs no agent: down and stalled hosts delayed every cold
// host behind them, in the same order on every refresh.

#[tokio::test]
async fn a_cold_host_is_not_held_up_by_stalled_hosts() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    fs::write(dir.path().join("release"), "").unwrap();
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.approval_deadline = Duration::from_secs(3);
    // Two hosts that hang in authentication without ever prompting, then a cold host whose
    // key needs no approval.
    let targets: Vec<Target> = ["root@stall1", "root@stall2", "root@up"]
        .iter()
        .map(|s| Target::parse(s).unwrap())
        .collect();
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let results = join_all(
        targets
            .iter()
            .map(|target| {
                Box::pin(async {
                    let result = probe(target, MachineClass::Nixos, &params, &cancel).await;
                    (result, start.elapsed())
                }) as std::pin::Pin<Box<_>>
            })
            .collect(),
    )
    .await;
    let (up, waited) = &results[2];
    assert!(matches!(up, Ok(ProbeOutcome::Observed(_))), "{up:?}");
    assert!(
        *waited < Duration::from_millis(1500),
        "a cold host waited {waited:?} behind two stalled ones"
    );
    // The stalled hosts only time out on their own windows, in parallel.
    for (result, at) in &results[..2] {
        assert!(
            matches!(result, Ok(ProbeOutcome::Unreachable { reason }) if reason.contains("timed out")),
            "{result:?}"
        );
        assert!(
            *at >= Duration::from_secs(3) && *at < Duration::from_millis(4500),
            "{at:?}"
        );
    }
}

#[tokio::test]
async fn a_cold_host_is_not_held_up_by_down_hosts() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    fs::write(dir.path().join("release"), "").unwrap();
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.connect_timeout = ConnectTimeout::new(1).unwrap();
    let names = [
        "root@down1",
        "root@down2",
        "root@down3",
        "root@down4",
        "root@down5",
        "root@up",
    ];
    let targets: Vec<Target> = names.iter().map(|s| Target::parse(s).unwrap()).collect();
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let results = join_all(
        targets
            .iter()
            .map(|target| {
                Box::pin(probe(target, MachineClass::Nixos, &params, &cancel))
                    as std::pin::Pin<Box<_>>
            })
            .collect(),
    )
    .await;
    // Five 1 s timeouts in parallel, not back to back.
    assert!(
        start.elapsed() < Duration::from_millis(2500),
        "five down hosts took {:?}",
        start.elapsed()
    );
    assert!(
        matches!(results[5], Ok(ProbeOutcome::Observed(_))),
        "{results:?}"
    );
    for result in &results[..5] {
        assert!(
            matches!(result, Ok(ProbeOutcome::Unreachable { .. })),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn a_stalled_host_alone_times_out_after_its_own_window() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.approval_deadline = Duration::from_secs(2);
    let start = Instant::now();
    let result = probe(
        &Target::parse("root@stall").unwrap(),
        MachineClass::Nixos,
        &params,
        &CancellationToken::new(),
    )
    .await;
    let elapsed = start.elapsed();
    assert!(
        matches!(&result, Ok(ProbeOutcome::Unreachable { reason }) if reason.contains("timed out")),
        "{result:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(4),
        "{elapsed:?}"
    );
}

#[tokio::test]
async fn an_ending_phase_gives_the_waiting_ones_a_fresh_window() {
    // `slow` is approved after 2.5 s; `stall` is never approved. Its 3 s window is worked out
    // again when `slow`'s phase ends, so it runs to about 2.5 + 3 s, not 3 s.
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.approval_deadline = Duration::from_secs(3);
    let slow = Target::parse("root@slow").unwrap();
    let stall = Target::parse("root@stall").unwrap();
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let ((slow_result, stall_result, stalled_for), ()) = tokio::join!(
        async {
            let (a, b) = tokio::join!(probe(&slow, MachineClass::Nixos, &params, &cancel), async {
                let r = probe(&stall, MachineClass::Nixos, &params, &cancel).await;
                (r, start.elapsed())
            },);
            (a, b.0, b.1)
        },
        async {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            fs::write(dir.path().join("release-root@slow"), "").unwrap();
        },
    );
    assert!(
        matches!(slow_result, Ok(ProbeOutcome::Observed(_))),
        "{slow_result:?}"
    );
    assert!(
        matches!(&stall_result, Ok(ProbeOutcome::Unreachable { reason }) if reason.contains("timed out")),
        "{stall_result:?}"
    );
    assert!(
        stalled_for >= Duration::from_millis(5300) && stalled_for < Duration::from_secs(7),
        "the window was not moved by the phase that ended: {stalled_for:?}"
    );
}

#[tokio::test]
async fn a_phase_that_starts_long_after_an_earlier_one_ended_gets_its_full_window() {
    // The window is `approval_deadline` after the *later* of the phase's start and the last
    // end. An end that is older than the start must not shorten it.
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    fs::write(dir.path().join("release-root@first"), "").unwrap();
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.approval_deadline = Duration::from_secs(2);
    let cancel = CancellationToken::new();
    let first = probe(
        &Target::parse("root@first").unwrap(),
        MachineClass::Nixos,
        &params,
        &cancel,
    )
    .await;
    assert!(matches!(first, Ok(ProbeOutcome::Observed(_))), "{first:?}");
    // Longer than a window: the first phase's end is now older than the next one's whole window.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let second_target = Target::parse("root@second").unwrap();
    let (second, ()) = tokio::join!(
        probe(&second_target, MachineClass::Nixos, &params, &cancel),
        async {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            fs::write(dir.path().join("release-root@second"), "").unwrap();
        },
    );
    assert!(
        matches!(second, Ok(ProbeOutcome::Observed(_))),
        "an approval 1 s into a 2 s window was cut off: {second:?}"
    );
}

/// A fake whose masters are per port, and whose connect phase takes a second, so that
/// concurrent probes overlap. One line per connect phase in `connections`.
fn port_agent(t: &common::Tools, dir: &Path) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh-ports",
        &format!(
            r#"mode=probe
port=22
for arg; do
  prev2=$prev; prev=$arg
  [ "$prev2" = -p ] && port=$arg
  [ "$arg" = -O ] && mode=check
  [ "$arg" = true ] && mode=connect
done
case "$mode" in
  check) dest=$prev;;
  *) dest=$prev2;;
esac
# ssh hashes the lowercased host with the user and the port into the socket name.
socket=$(echo "$dest" | @tr@ A-Z a-z)-$port
case "$mode" in
  check) [ -f "@dir@/master-$socket" ]; exit $?;;
  connect)
    echo "$socket" >> '@dir@/connections'
    @sleep@ 1
    : > "@dir@/master-$socket"
    exit 0;;
esac
[ -f "@dir@/master-$socket" ] || exit 255
printf '{}'"#,
            probe_output()
        ),
    )
}

#[tokio::test]
async fn one_socket_spelled_three_ways_is_one_connection() {
    let Some(t) = tools(&["sleep", "tr"]) else {
        return;
    };
    let dir = tempdir();
    let program = port_agent(&t, dir.path());
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    // ssh lowercases the host and fills in port 22 before it expands `%C`: these three name
    // one socket, and a second connection would be a second prompt for nothing.
    let targets: Vec<Target> = ["root@H", "root@h:22", "root@h"]
        .iter()
        .map(|s| Target::parse(s).unwrap())
        .collect();
    let cancel = CancellationToken::new();
    let results = join_all(
        targets
            .iter()
            .map(|target| {
                Box::pin(probe(target, MachineClass::Nixos, &params, &cancel))
                    as std::pin::Pin<Box<_>>
            })
            .collect(),
    )
    .await;
    for result in results {
        assert!(
            matches!(result, Ok(ProbeOutcome::Observed(_))),
            "{result:?}"
        );
    }
    assert_eq!(read_lines(&dir.path().join("connections")), ["root@h-22"]);
}

#[tokio::test]
async fn distinct_ports_are_distinct_sockets_and_each_connects() {
    let Some(t) = tools(&["sleep", "tr"]) else {
        return;
    };
    let dir = tempdir();
    let program = port_agent(&t, dir.path());
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let a = Target::parse("root@h").unwrap();
    let b = Target::parse("root@h:2222").unwrap();
    let cancel = CancellationToken::new();
    let (ra, rb) = tokio::join!(
        probe(&a, MachineClass::Nixos, &params, &cancel),
        probe(&b, MachineClass::Nixos, &params, &cancel),
    );
    assert!(matches!(ra, Ok(ProbeOutcome::Observed(_))), "{ra:?}");
    assert!(matches!(rb, Ok(ProbeOutcome::Observed(_))), "{rb:?}");
    let mut ports = read_lines(&dir.path().join("connections"));
    ports.sort();
    assert_eq!(ports, ["root@h-22", "root@h-2222"]);
}

#[tokio::test]
async fn one_targets_own_authentication_failure_is_not_shared_with_another() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    // Refuses the connect phase only for a target that disabled public keys.
    let program = t.script(
        dir.path(),
        "ssh-options",
        &format!(
            r#"mode=probe
for arg; do
  prev2=$prev; prev=$arg
  [ "$arg" = -O ] && mode=check
  [ "$arg" = true ] && mode=connect
  [ "$arg" = PubkeyAuthentication=no ] && restricted=1
done
case "$mode" in
  check) [ -f '@dir@/master' ]; exit $?;;
  connect)
    echo "$*" >> '@dir@/connects'
    @sleep@ 0.5
    if [ -n "$restricted" ]; then
      echo 'root@h: Permission denied (publickey).' >&2
      exit 255
    fi
    : > '@dir@/master'
    exit 0;;
esac
[ -f '@dir@/master' ] || exit 255
printf '{}'"#,
            probe_output()
        ),
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let restricted = Target::parse("root@h?PubkeyAuthentication=no").unwrap();
    let plain = Target::parse("root@h").unwrap();
    let cancel = CancellationToken::new();
    let (a, b) = tokio::join!(
        probe(&restricted, MachineClass::Nixos, &params, &cancel),
        probe(&plain, MachineClass::Nixos, &params, &cancel),
    );
    assert_eq!(
        a,
        Ok(ProbeOutcome::Unreachable {
            reason: "root@h: Permission denied (publickey).".to_owned()
        })
    );
    assert!(
        matches!(b, Ok(ProbeOutcome::Observed(_))),
        "the failure of a target with other options was handed to this one: {b:?}"
    );
    assert_eq!(read_lines(&dir.path().join("connects")).len(), 2);
}

#[tokio::test]
async fn a_waiter_does_not_time_out_before_an_owner_that_succeeds() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    // A wedged master: the check hangs until ConnectTimeout cuts it off. The approval then
    // takes 3.5 s of its 4 s window. The owner holds the destination for 4.5 s in all,
    // longer than the waiter's own approval window.
    let program = t.script(
        dir.path(),
        "ssh-slow-check",
        &format!(
            r#"mode=probe
for arg; do
  [ "$arg" = -O ] && mode=check
  [ "$arg" = true ] && mode=connect
done
case "$mode" in
  check) [ -f '@dir@/master' ] && exit 0; @sleep@ 30;;
  connect) @sleep@ 3.5; : > '@dir@/master'; exit 0;;
esac
[ -f '@dir@/master' ] || exit 255
printf '{}'"#,
            probe_output()
        ),
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.connect_timeout = ConnectTimeout::new(1).unwrap();
    params.approval_deadline = Duration::from_secs(4);
    let target = Target::parse("root@h").unwrap();
    let cancel = CancellationToken::new();
    let (a, b) = tokio::join!(
        probe(&target, MachineClass::Nixos, &params, &cancel),
        probe(&target, MachineClass::Nixos, &params, &cancel),
    );
    assert!(matches!(a, Ok(ProbeOutcome::Observed(_))), "{a:?}");
    assert!(
        matches!(b, Ok(ProbeOutcome::Observed(_))),
        "the waiter timed out: {b:?}"
    );
}

#[tokio::test]
async fn a_cancelled_owner_lets_the_waiter_connect_for_itself() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let program = gated_agent(&t, dir.path());
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let target = Target::parse("root@h").unwrap();
    let owner = CancellationToken::new();
    let waiter = CancellationToken::new();
    let starts = dir.path().join("starts");
    let (a, b, ()) = {
        let (a, (b, ())) = tokio::join!(
            probe(&target, MachineClass::Nixos, &params, &owner),
            async {
                // The waiter must be queued behind the owner, which is the first caller
                // polled.
                tokio::join!(
                    probe(&target, MachineClass::Nixos, &params, &waiter),
                    async {
                        wait_for_lines(&starts, 1).await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        owner.cancel();
                        // The waiter now runs a connect phase of its own.
                        wait_for_lines(&starts, 2).await;
                        fs::write(dir.path().join("release"), "").unwrap();
                    }
                )
            },
        );
        (a, b, ())
    };
    assert_eq!(a, Err(Cancelled));
    assert!(
        matches!(b, Ok(ProbeOutcome::Observed(_))),
        "the cancelled owner's outcome was shared: {b:?}"
    );
    assert_eq!(read_lines(&starts), ["root@h", "root@h"]);
}

#[tokio::test]
async fn distinct_users_are_distinct_sockets_and_each_connects() {
    let Some(t) = tools(&["sleep", "tr"]) else {
        return;
    };
    let dir = tempdir();
    let program = port_agent(&t, dir.path());
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let a = Target::parse("alice@h").unwrap();
    let b = Target::parse("bob@h").unwrap();
    let cancel = CancellationToken::new();
    let (ra, rb) = tokio::join!(
        probe(&a, MachineClass::Nixos, &params, &cancel),
        probe(&b, MachineClass::Nixos, &params, &cancel),
    );
    assert!(matches!(ra, Ok(ProbeOutcome::Observed(_))), "{ra:?}");
    assert!(matches!(rb, Ok(ProbeOutcome::Observed(_))), "{rb:?}");
    let mut sockets = read_lines(&dir.path().join("connections"));
    sockets.sort();
    assert_eq!(sockets, ["alice@h-22", "bob@h-22"]);
}

// ---------------------------------------------------------------------------------------
// Review of PR #37, finding 3: the control directory is checked before every ssh
// ---------------------------------------------------------------------------------------

/// `recheck` accepts the directory as prepared, and refuses what a periodic clean-up of `/tmp`
/// and a local user could make of it: gone, made again (even 0700 and ours, it is another
/// directory), another mode, a symlink.
#[test]
fn a_control_directory_that_changed_since_it_was_prepared_is_refused() {
    let dir = tempdir();
    let path = dir.path().join("c");
    let cd = ControlDir::prepare(&path).unwrap();
    cd.recheck().unwrap();

    // Made again, 0700 and ours: not the directory auspex prepared. The prepared directory is
    // renamed aside, not removed, and kept alive: a filesystem may hand a freed inode number to
    // the next directory at once (the forge runner's did), and then device and inode would be
    // equal for a directory that is not the prepared one.
    let aside = dir.path().join("c.aside");
    fs::rename(&path, &aside).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let e = cd.recheck().unwrap_err();
    assert!(e.0.contains("not the directory auspex made"), "{e}");
    assert!(e.0.contains("XDG_RUNTIME_DIR"), "{e}");
    // Put the prepared directory back: it is accepted again, so the refusal was the identity.
    fs::remove_dir(&path).unwrap();
    fs::rename(&aside, &path).unwrap();
    cd.recheck().unwrap();

    // Removed: nothing is created for it.
    fs::remove_dir(&path).unwrap();
    let e = cd.recheck().unwrap_err();
    assert!(e.0.contains("cannot inspect"), "{e}");
    assert!(!path.exists(), "recheck must not create the directory");

    // A fresh preparation of it is a fresh start.
    let cd = ControlDir::prepare(&path).unwrap();
    cd.recheck().unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(cd.recheck().unwrap_err().0.contains("mode 0755"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    cd.recheck().unwrap();

    // A symlink to a directory that is ours and 0700.
    let elsewhere = dir.path().join("elsewhere");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&elsewhere)
        .unwrap();
    fs::remove_dir(&path).unwrap();
    symlink(&elsewhere, &path).unwrap();
    assert!(cd.recheck().unwrap_err().0.contains("symlink"));
}

/// The directory's owner is named with a way out: set `XDG_RUNTIME_DIR`.
#[test]
fn a_control_directory_owned_by_someone_else_says_to_set_xdg_runtime_dir() {
    use std::os::unix::fs::MetadataExt as _;
    let me = rustix::process::getuid().as_raw();
    let Some(theirs) = ["/", "/proc", "/dev", "/nix/store"]
        .iter()
        .map(Path::new)
        .find(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && m.uid() != me))
    else {
        eprintln!("SKIPPED: no directory owned by another user to test with (running as root?)");
        return;
    };
    let e = ControlDir::prepare(theirs).unwrap_err();
    assert!(e.0.contains("XDG_RUNTIME_DIR"), "{e}");
}

/// No ssh is started in a directory that is no longer the prepared one: not the first ssh of a
/// probe (connect), not the later ones (a master that is already up), and not the interactive
/// one the shell runs itself.
#[tokio::test]
async fn no_ssh_is_started_after_the_control_directory_changed() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = warm_script(
        &t,
        dir.path(),
        "ssh",
        &format!("echo ran >> '@dir@/ran'\nprintf '{}'", probe_output()),
    );
    let ran = || fs::read_to_string(dir.path().join("ran")).map_or(0, |s| s.lines().count());
    let cd = control(dir.path());
    let target = Target::parse("h").unwrap();

    // Healthy: ssh runs (the master check, then the probe).
    let (r, _) = run_probe(&prog, &cd, &target, Duration::from_secs(10)).await;
    assert!(matches!(r.unwrap(), ProbeOutcome::Observed(_)));
    let before = ran();
    assert!(before >= 1);

    // The directory is replaced by a new one: the master is believed up, so only the probe's
    // own spawn is left to refuse it. The prepared directory is renamed aside and kept alive,
    // so its inode number cannot be handed to the new one (a filesystem may do that at once).
    let path = dir.path().join("c");
    fs::rename(&path, dir.path().join("c.aside")).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let (r, _) = run_probe(&prog, &cd, &target, Duration::from_secs(10)).await;
    let ProbeOutcome::Failed { reason } = r.unwrap() else {
        panic!("a probe in a replaced control directory must fail")
    };
    assert!(reason.contains("not the directory auspex made"), "{reason}");
    assert_eq!(ran(), before, "ssh ran in a replaced directory");

    // A host not seen before has no master to believe in: the connect phase refuses.
    let other = Target::parse("other").unwrap();
    let (r, _) = run_probe(&prog, &cd, &other, Duration::from_secs(10)).await;
    assert!(matches!(r.unwrap(), ProbeOutcome::Failed { .. }));
    assert_eq!(ran(), before);

    // And the interactive command is not handed out.
    let e = auspex_effects::ssh::interactive_command(prog.as_os_str(), &target, &cd).unwrap_err();
    assert!(e.contains("not the directory auspex made"), "{e}");
}

/// Each spawn rechecks on its own (review of PR #37, finding 3): a fake ssh whose master check
/// replaces the control directory and then answers. With the answer "no master", the next ssh
/// is the connect phase; with "master up", it is the probe itself. Either must be refused, and
/// only the check may have run.
#[tokio::test]
async fn the_connect_phase_and_the_probe_each_recheck_the_control_directory() {
    let Some(t) = tools(&[]) else { return };
    for (answer, phase) in [(1, "connect"), (0, "probe")] {
        let dir = tempdir();
        let prog = t.script(
            dir.path(),
            "ssh",
            &format!(
                "echo run >> '@dir@/ran'\nfor arg; do\n  if [ \"$arg\" = -O ]; then\n    mv '@dir@/c' '@dir@/c.aside' && mkdir -m 700 '@dir@/c'\n    exit {answer}\n  fi\ndone\nprintf '{}'",
                probe_output()
            ),
        );
        let cd = control(dir.path());
        let target = Target::parse("h").unwrap();
        let (r, _) = run_probe(&prog, &cd, &target, Duration::from_secs(10)).await;
        let ProbeOutcome::Failed { reason } = r.unwrap() else {
            panic!("{phase}: a replaced control directory must fail the probe")
        };
        assert!(
            reason.contains("not the directory auspex made"),
            "{phase}: {reason}"
        );
        let ran = fs::read_to_string(dir.path().join("ran")).unwrap();
        assert_eq!(
            ran.lines().count(),
            1,
            "{phase}: only the check may run: {ran}"
        );
    }
}
