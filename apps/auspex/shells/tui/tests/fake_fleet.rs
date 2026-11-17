//! The built binary against fake `nix`, `nix-eval-jobs` and `ssh` programs: exit statuses
//! (a clean scan, a fleet-level error, the deadline) and termination signals.
//!
//! The fakes are pointed at through the undocumented `AUSPEX_NIX`, `AUSPEX_NIX_EVAL_JOBS` and
//! `AUSPEX_SSH` variables. They need only `sh` and `sleep`, so these tests run inside the
//! `ci-pure` sandbox too, where there is no real nix.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Tools, alive, auspex, cleanup, run_within, tempdir, tools, wait_for_pids};
use rustix::process::{Pid, Signal, kill_process};

const LIMIT: Duration = Duration::from_secs(60);

const INVENTORY: &str = r#"{"terra":{"deploy":{"targetHost":"root@192.0.2.10"}},"luna":{"deploy":{"targetHost":"root@192.0.2.11"}}}"#;

fn store(name: &str) -> String {
    format!("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{name}")
}

/// A `nix` that prints the two-machine inventory.
fn nix_ok(t: &Tools, dir: &Path) -> std::path::PathBuf {
    t.script(dir, "nix", &format!("printf '%s\\n' '{INVENTORY}'"))
}

/// An evaluator that answers both machines at once.
fn eval_ok(t: &Tools, dir: &Path) -> std::path::PathBuf {
    let line = |m: &str| {
        format!(
            r#"{{"attr":"{m}","attrPath":["{m}"],"drvPath":"{}.drv","outputs":{{"out":"{}"}}}}"#,
            store(&format!("nixos-system-{m}")),
            store(&format!("nixos-system-{m}"))
        )
    };
    t.script(
        dir,
        "nix-eval-jobs",
        &format!("printf '%s\\n' '{}' '{}'", line("terra"), line("luna")),
    )
}

/// An ssh that answers `terra` in sync and fails `luna` the way ssh does (exit 255).
fn ssh_ok(t: &Tools, dir: &Path) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh",
        &format!(
            r#"for a; do last=$a; case "$prev" in --) dest=$a;; esac; prev=$a; done
case "$dest" in
  *192.0.2.11*) echo 'ssh: connect to host 192.0.2.11 port 22: No route to host' >&2; exit 255;;
esac
cat > /dev/null
printf 'auspex_probe=1\nprobe=nixos\ncurrent_system={}\nsystem_state=running\nfailed_units=[]\nauspex_end=1\n'"#,
            store("nixos-system-terra")
        ),
    )
}

fn flake_dir(dir: &Path) -> std::path::PathBuf {
    let f = dir.join("fleet");
    std::fs::create_dir_all(&f).unwrap();
    f
}

fn config(dir: &Path, text: &str) -> std::path::PathBuf {
    let p = dir.join("auspex.toml");
    std::fs::write(&p, text).unwrap();
    p
}

fn with_fakes(cmd: &mut Command, nix: &Path, eval: &Path, ssh: &Path) {
    cmd.env("AUSPEX_NIX", nix)
        .env("AUSPEX_NIX_EVAL_JOBS", eval)
        .env("AUSPEX_SSH", ssh);
}

#[test]
fn a_clean_scan_exits_0_and_an_unreachable_machine_is_a_finding() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let (nix, eval, ssh) = (
        nix_ok(&t, dir.path()),
        eval_ok(&t, dir.path()),
        ssh_ok(&t, dir.path()),
    );
    // An override for a machine the fleet does not have: said once, once the list is in.
    let cfg = config(
        dir.path(),
        "[machines.ghost]\ntarget = \"root@192.0.2.50\"\n[machines.luna]\nprometheus_instance = \"luna:9100\"\n",
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(stderr.matches("`machines.ghost`").count(), 1, "{stderr}");
    assert!(!stderr.contains("`machines.luna`"), "{stderr}");
    let luna = stdout.lines().find(|l| l.contains("luna")).unwrap();
    assert!(
        luna.contains("unreachable") && luna.contains("No route to host"),
        "{luna}"
    );
    let terra = stdout.lines().find(|l| l.contains("terra")).unwrap();
    assert!(
        terra.contains("in sync") && terra.contains("inventory"),
        "{terra}"
    );
    // Not a git repository: caching is off, and the log says why.
    assert!(stderr.contains("eval cache off"), "{stderr}");
}

#[test]
fn a_fleet_level_error_exits_1_and_says_where_the_flake_came_from() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = t.script(
        dir.path(),
        "nix",
        "echo 'error: flake does not provide attribute clanInternals' >&2; exit 1",
    );
    let (eval, ssh) = (eval_ok(&t, dir.path()), ssh_ok(&t, dir.path()));
    let fleet = flake_dir(dir.path());
    let mut cmd = auspex(dir.path());
    cmd.arg("--plain").env("CLAN_DIR", &fleet);
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    let error = stdout
        .lines()
        .find(|l| l.starts_with("error: inventory_failed"))
        .unwrap();
    assert!(error.contains("cannot read the fleet at"), "{error}");
    assert!(
        error.contains("CLAN_DIR"),
        "names the source that won: {error}"
    );
    assert!(error.contains("does not provide attribute"), "{error}");

    // The same in JSON: the error is in the report, and the status is still 1.
    let mut cmd = auspex(dir.path());
    cmd.arg("--json").env("CLAN_DIR", &fleet);
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    assert_eq!(out.status.code(), Some(1));
    let report: auspex_domain::report::FleetReport = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report.errors.len(), 1);
}

/// An ssh that records its pid and never answers.
fn ssh_hangs(t: &Tools, dir: &Path) -> std::path::PathBuf {
    t.script(dir, "ssh", "echo $$ >> '@dir@/ssh.pids'\nexec @sleep@ 300")
}

/// An evaluator that records its pid and a worker's, and never answers.
fn eval_hangs(t: &Tools, dir: &Path) -> std::path::PathBuf {
    t.script(
        dir,
        "nix-eval-jobs",
        "@sleep@ 300 &\necho $! >> '@dir@/eval.pids'\necho $$ >> '@dir@/eval.pids'\nwait",
    )
}

#[test]
fn the_deadline_settles_the_scan_kills_the_stragglers_and_exits_1() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let (nix, eval, ssh) = (
        nix_ok(&t, dir.path()),
        eval_hangs(&t, dir.path()),
        ssh_hangs(&t, dir.path()),
    );
    let cfg = config(
        dir.path(),
        "[timeouts]\nsettle = 2\nprobe = 60\neval = 60\n",
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let start = Instant::now();
    let out = run_within(&mut cmd, LIMIT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "the deadline, not the probe timeout"
    );
    assert!(
        stdout.contains("did not settle within timeouts.settle (2 s)"),
        "{stdout}"
    );
    let terra = stdout.lines().find(|l| l.contains("terra")).unwrap();
    assert!(
        terra.contains("unreachable") && terra.contains("global deadline"),
        "{terra}"
    );
    // The core cancelled them at the deadline, and the adapters killed every process.
    let mut pids = wait_for_pids(&dir.path().join("ssh.pids"), 2);
    pids.extend(wait_for_pids(&dir.path().join("eval.pids"), 2));
    let left: Vec<i32> = pids.iter().copied().filter(|&p| alive(p)).collect();
    cleanup(&pids);
    assert!(left.is_empty(), "still alive after exit: {left:?}");
}

fn signal_kills_everything(signal: Signal, name: &str) {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let (nix, eval, ssh) = (
        nix_ok(&t, dir.path()),
        eval_hangs(&t, dir.path()),
        ssh_hangs(&t, dir.path()),
    );
    let cfg = config(
        dir.path(),
        "[timeouts]\nsettle = 600\nprobe = 600\neval = 600\n",
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let mut child = cmd.spawn().unwrap();
    // Every fake is running: two probes, the evaluator and its worker.
    let mut pids = wait_for_pids(&dir.path().join("ssh.pids"), 2);
    pids.extend(wait_for_pids(&dir.path().join("eval.pids"), 2));
    let auspex_pid = i32::try_from(child.id()).unwrap();
    kill_process(Pid::from_raw(auspex_pid).unwrap(), signal).unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            cleanup(&pids);
            panic!("{name}: auspex did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let left: Vec<i32> = pids.iter().copied().filter(|&p| alive(p)).collect();
    cleanup(&pids);
    let out = child.wait_with_output().unwrap();
    assert!(left.is_empty(), "{name}: children still alive: {left:?}");
    assert!(!alive(auspex_pid), "{name}: auspex itself is gone");
    assert_eq!(
        status.code(),
        Some(128 + signal.as_raw()),
        "{name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"", "{name}: no report for an interrupted scan");
}

#[test]
fn sigint_stops_auspex_and_every_child() {
    signal_kills_everything(Signal::INT, "SIGINT");
}

#[test]
fn sigterm_stops_auspex_and_every_child() {
    signal_kills_everything(Signal::TERM, "SIGTERM");
}

#[test]
fn sighup_stops_auspex_and_every_child() {
    signal_kills_everything(Signal::HUP, "SIGHUP");
}

#[test]
fn sigquit_stops_auspex_and_every_child() {
    signal_kills_everything(Signal::QUIT, "SIGQUIT");
}
