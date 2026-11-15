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
    let prog = t.script(
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
    let prog = t.script(
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
    let prog = t.script(
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
    let prog = t.script(
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
    let prog = t.script(
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
    let prog = t.script(
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
    let prog = t.script(
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
    // Checked at once, without waiting: the call reaps before returning, and the
    // grandchild was SIGKILLed with its group.
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(r, Err(Cancelled));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(left.is_empty(), "still running: {left:?}");
}

#[tokio::test]
async fn dropping_the_future_kills_ssh_and_its_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
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

    let prog = t.script(dir.path(), "ssh", "kill -TERM $$");
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
            reason: "cannot probe: auspex-test-no-such-ssh not found on PATH".to_owned()
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
