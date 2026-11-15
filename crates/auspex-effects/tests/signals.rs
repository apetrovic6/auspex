//! Review finding F4: children run in sessions of their own, so when auspex itself is
//! told to stop by a signal, only auspex can stop them. `termination()` is the wait the
//! shell uses; this shows that a process using it the documented way (cancel, then return)
//! leaves nothing behind for each of `SIGHUP`, `SIGTERM`, `SIGINT` and `SIGQUIT`.
//!
//! The process that gets the signal is a child test process (this binary, re-run on the
//! inner test), so the signal never reaches the test harness.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use auspex_domain::fleet::MachineClass;
use auspex_effects::nix::{EvalMachine, EvalParams, FlakeRef, evaluate};
use auspex_effects::{CancellationToken, Cancelled, termination};
use common::{cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};
use rustix::process::{Pid, Signal, kill_process};
use tokio::sync::mpsc::unbounded_channel;

const INNER: &str = "AUSPEX_TEST_SIGNAL_DIR";

/// Runs an evaluation whose evaluator forks a worker, until a termination signal arrives;
/// then cancels and returns. Does nothing unless started by [`every_termination_signal_kills_the_children`].
#[tokio::test]
async fn signal_inner() {
    let Some(dir) = std::env::var_os(INNER).map(PathBuf::from) else {
        return;
    };
    let t = tools(&["sleep"]).expect("sleep");
    let prog = t.script(
        &dir,
        "nix-eval-jobs",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let mut params = EvalParams::new();
    params.program = prog.as_os_str();
    params.timeout = Duration::from_secs(60);
    let machines = [EvalMachine {
        name: "a".to_owned(),
        class: MachineClass::Nixos,
    }];
    let flake = FlakeRef::new("path:/x").unwrap();
    let cancel = CancellationToken::new();
    let (tx, _rx) = unbounded_channel();
    let run = evaluate(&flake, &machines, &params, &cancel, tx);
    tokio::pin!(run);
    let term = termination();
    tokio::pin!(term);
    let signal = tokio::select! {
        s = &mut term => s.unwrap(),
        r = &mut run => panic!("the evaluation ended first: {r:?}"),
    };
    cancel.cancel();
    assert_eq!(run.await, Err(Cancelled));
    std::fs::write(dir.join("signal"), signal).unwrap();
}

#[test]
fn every_termination_signal_kills_the_children() {
    if std::env::var_os(INNER).is_some() {
        return;
    }
    if tools(&["sleep"]).is_none() {
        return;
    }
    for (signal, name) in [
        (Signal::HUP, "SIGHUP"),
        (Signal::TERM, "SIGTERM"),
        (Signal::INT, "SIGINT"),
        (Signal::QUIT, "SIGQUIT"),
    ] {
        let dir = tempdir();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signal_inner", "--nocapture", "--test-threads=1"])
            .env(INNER, dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pids = wait_for_pids(&dir.path().join("pids"), 2);
        // The handlers are registered in the inner's first poll, before the evaluator was
        // spawned, so they are in place once the evaluator has written its pid.
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        kill_process(pid, signal).unwrap();
        let status = child.wait().unwrap();
        let left = still_alive_after_wait(&pids);
        cleanup(&pids);
        assert!(
            status.success(),
            "{name}: the inner test failed: {status:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("signal")).unwrap(),
            name
        );
        assert!(left.is_empty(), "{name}: still running: {left:?}");
    }
}
