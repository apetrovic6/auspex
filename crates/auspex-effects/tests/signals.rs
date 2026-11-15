//! Review findings F4 and N4: children run in sessions of their own, so when auspex itself
//! is told to stop by a signal, only auspex can stop them. [`Termination`] is how the shell
//! notices: created once, kept, `recv` called as often as it likes.
//!
//! The process that gets the signal is always a child test process (this binary, re-run on
//! the inner test with `AUSPEX_TEST_SIGNAL` naming the shape), so no signal reaches the
//! test harness.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use auspex_domain::fleet::MachineClass;
use auspex_effects::nix::{EvalMachine, EvalParams, FlakeRef, evaluate};
use auspex_effects::{CancellationToken, Cancelled, Termination};
use common::{cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};
use rustix::process::{Pid, Signal, kill_process};
use tokio::sync::mpsc::unbounded_channel;

const SHAPE: &str = "AUSPEX_TEST_SIGNAL";
const DIR: &str = "AUSPEX_TEST_SIGNAL_DIR";

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

/// The inner side. Each shape writes `ready` once its `Termination` exists, and `signal`
/// with the name it received.
#[tokio::test]
async fn signal_inner() {
    let (Ok(shape), Some(dir)) = (
        std::env::var(SHAPE),
        std::env::var_os(DIR).map(PathBuf::from),
    ) else {
        return;
    };
    let mut term = Termination::new().unwrap();
    match shape.as_str() {
        // An evaluation whose evaluator forks a worker; on the signal, cancel, then return.
        "children" => {
            let t = tools(&["sleep"]).expect("sleep");
            let prog = t.script(
                &dir,
                "nix-eval-jobs",
                "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
            );
            let mut params = EvalParams::new();
            params.runtime_dir = Some(dir.join("rt"));
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
            write(&dir, "ready", "");
            let signal = tokio::select! {
                s = term.recv() => s,
                r = &mut run => panic!("the evaluation ended first: {r:?}"),
            };
            cancel.cancel();
            assert_eq!(run.await, Err(Cancelled));
            write(&dir, "signal", signal);
        }
        // Nobody is in `recv` when the signal arrives: it is remembered. The wait is an
        // async sleep on purpose: the runtime's signal driver handles the signal during it,
        // while nobody waits, which is how a signal used to be lost (a thread sleep would
        // delay the driver until `recv` and hide that).
        "between" => {
            write(&dir, "ready", "");
            tokio::time::sleep(Duration::from_millis(500)).await;
            let signal = tokio::time::timeout(Duration::from_secs(2), term.recv())
                .await
                .expect("the signal sent between two polls was lost");
            write(&dir, "signal", signal);
        }
        // The reviewer's shape: a busy loop that polls `recv` for a moment at a time.
        "loop" => {
            write(&dir, "ready", "");
            let start = Instant::now();
            let signal = loop {
                tokio::select! {
                    s = term.recv() => break s,
                    () = tokio::time::sleep(Duration::from_millis(1)) => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                assert!(start.elapsed() < Duration::from_secs(5), "lost in the loop");
            };
            write(&dir, "signal", signal);
        }
        other => panic!("unknown shape {other}"),
    }
}

fn run_inner(shape: &str, signal: Signal) -> (tempfile::TempDir, std::process::ExitStatus) {
    let dir = tempdir();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "signal_inner", "--nocapture", "--test-threads=1"])
        .env(SHAPE, shape)
        .env(DIR, dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ready = dir
        .path()
        .join(if shape == "children" { "pids" } else { "ready" });
    let start = Instant::now();
    while !ready.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{shape}: never ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    if shape == "children" {
        wait_for_pids(&dir.path().join("pids"), 2);
    }
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    kill_process(pid, signal).unwrap();
    let status = child.wait().unwrap();
    (dir, status)
}

#[test]
fn every_termination_signal_kills_the_children() {
    if std::env::var_os(SHAPE).is_some() || tools(&["sleep"]).is_none() {
        return;
    }
    for (signal, name) in [
        (Signal::HUP, "SIGHUP"),
        (Signal::TERM, "SIGTERM"),
        (Signal::INT, "SIGINT"),
        (Signal::QUIT, "SIGQUIT"),
    ] {
        let (dir, status) = run_inner("children", signal);
        let pids = wait_for_pids(&dir.path().join("pids"), 2);
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

/// A signal that arrives while nobody is in `recv` is not lost.
#[test]
fn a_signal_between_two_polls_is_not_lost() {
    if std::env::var_os(SHAPE).is_some() {
        return;
    }
    let (dir, status) = run_inner("between", Signal::TERM);
    assert!(status.success(), "{status:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("signal")).unwrap(),
        "SIGTERM"
    );
}

/// The reviewer's loop: `select!` on `recv` and a 1 ms timer, then 100 ms of blocking
/// work. With the old `termination()` future, recreated each iteration, the signal was
/// lost.
#[test]
fn a_select_loop_does_not_lose_the_signal() {
    if std::env::var_os(SHAPE).is_some() {
        return;
    }
    let (dir, status) = run_inner("loop", Signal::INT);
    assert!(status.success(), "{status:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("signal")).unwrap(),
        "SIGINT"
    );
}
