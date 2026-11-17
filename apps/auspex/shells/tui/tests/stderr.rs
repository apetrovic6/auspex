//! Review N1: `eprintln!` panics when stderr cannot be written. In an adapter task that
//! killed the task, so headless waited for `settle` and printed a false report (exit 1);
//! on the main thread it was exit 101; and `auspex --plain 2>&1 | head -1` took 120 s.
//! A failed write to stderr must change neither the report nor the exit status, nor how
//! long the scan takes.

mod common;

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use common::{auspex, crate_dir, tempdir};

/// The demo scan takes about 4.3 s; the deadline is 120 s. Well under either.
const QUICK: Duration = Duration::from_secs(30);

fn expected_plain() -> String {
    std::fs::read_to_string(crate_dir().join("tests/fixtures/expected/demo.txt")).unwrap()
}

fn check(what: &str, out: &Output, took: Duration) {
    assert_eq!(out.status.code(), Some(0), "{what}: {out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        expected_plain(),
        "{what}"
    );
    assert!(took < QUICK, "{what}: took {took:?}, a stalled scan");
}

fn full() -> Option<std::fs::File> {
    Path::new("/dev/full")
        .exists()
        .then(|| OpenOptions::new().write(true).open("/dev/full").unwrap())
}

#[test]
fn a_full_stderr_changes_nothing() {
    let Some(_) = full() else {
        eprintln!("SKIPPED: no /dev/full");
        return;
    };
    let dir = tempdir();
    // A config warning too, so the first write to stderr happens on the main thread
    // (before the runtime) as well as in the adapter tasks (the log).
    let cfg = dir.path().join("warn.toml");
    std::fs::write(&cfg, "future_key = 1\n").unwrap();
    for args in [
        vec!["--demo", "--plain"],
        vec!["--demo", "--plain", "--config", cfg.to_str().unwrap()],
    ] {
        let start = Instant::now();
        let out = auspex(dir.path())
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(full().unwrap())
            .output()
            .unwrap();
        check(&format!("{args:?} 2>/dev/full"), &out, start.elapsed());
    }
}

#[test]
fn a_closed_stderr_changes_nothing() {
    let dir = tempdir();
    let start = Instant::now();
    let mut child = auspex(dir.path())
        .args(["--demo", "--plain"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The reader goes away before anything is written, as `2>&1 | head -1` does.
    drop(child.stderr.take());
    let out = child.wait_with_output().unwrap();
    check("stderr closed", &out, start.elapsed());
}

#[test]
fn usage_errors_with_a_full_stderr_are_still_2() {
    let Some(_) = full() else {
        eprintln!("SKIPPED: no /dev/full");
        return;
    };
    let dir = tempdir();
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "[eval]\nworkers = 0\n").unwrap();
    for args in [
        vec!["--no-such-flag"],
        vec!["--demo", "--plain", "--config", bad.to_str().unwrap()],
    ] {
        let out = auspex(dir.path())
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(full().unwrap())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: not a panic (101)");
    }
}
