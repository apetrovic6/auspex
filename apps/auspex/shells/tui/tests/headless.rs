//! `--plain` and `--json` from the built binary: the demo's output against expected files,
//! the JSON against the domain's type, and the exit statuses.

mod common;

use std::time::Duration;

use auspex_domain::report::FleetReport;
use common::{auspex, expect_file, run_within, tempdir};

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn demo_json_is_the_domains_report_and_nothing_else() {
    let dir = tempdir();
    let out = run_within(auspex(dir.path()).args(["--demo", "--json"]), LIMIT);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // One document: it parses as the domain's own type, whole.
    let report: FleetReport = serde_json::from_str(&stdout).unwrap();
    assert!(report.settled);
    assert_eq!(report.machines.len(), 18);
    expect_file("expected", "demo.json", &stdout);
    // The log went to stderr, not into the document.
    assert!(String::from_utf8_lossy(&out.stderr).contains("eval cache hit: terra"));
}

#[test]
fn demo_plain_is_stable_and_uncoloured_into_a_pipe() {
    let dir = tempdir();
    let out = run_within(auspex(dir.path()).args(["--demo", "--plain"]), LIMIT);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains('\x1b'),
        "no colour unless stdout is a terminal"
    );
    expect_file("expected", "demo.txt", &stdout);
}

#[test]
fn plain_and_json_together_are_a_usage_error() {
    let dir = tempdir();
    let out = run_within(
        auspex(dir.path()).args(["--demo", "--plain", "--json"]),
        LIMIT,
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(out.stdout, b"");
}

#[test]
fn version_and_help_exit_0() {
    let dir = tempdir();
    let out = run_within(auspex(dir.path()).arg("--version"), LIMIT);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("auspex "));
    let out = run_within(auspex(dir.path()).arg("--help"), LIMIT);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("--demo"));
}
