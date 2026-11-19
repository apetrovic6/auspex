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
    t.script(dir, "ssh", "for arg; do [ \"$arg\" = '-O' ] && exit 0; done\necho $$ >> '@dir@/ssh.pids'\nexec @sleep@ 300")
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

// ---------------------------------------------------------------------------------------
// Each config value is used, not only read (review mutations B to E, G and H).
// ---------------------------------------------------------------------------------------

/// An ssh that records its arguments, then answers like `ssh_ok`.
fn ssh_recording(t: &Tools, dir: &Path, current: &str) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh",
        &format!(
            r#"echo "$*" >> '@dir@/ssh.argv'
for a; do case "$prev" in --) dest=$a;; esac; prev=$a; done
case "$dest" in
  *192.0.2.11*) echo 'ssh: connect to host 192.0.2.11 port 22: No route to host' >&2; exit 255;;
esac
cat > /dev/null
printf 'auspex_probe=1\nprobe=nixos\ncurrent_system={current}\nsystem_state=running\nfailed_units=[]\nauspex_end=1\n'"#
        ),
    )
}

/// An evaluator that records its arguments, then answers `terra` with `out` and `luna`.
fn eval_recording(t: &Tools, dir: &Path, out: &str) -> std::path::PathBuf {
    let luna = store("nixos-system-luna");
    t.script(
        dir,
        "nix-eval-jobs",
        &format!(
            r#"printf '%s\n' "$@" > '@dir@/eval.argv'
printf '%s\n' '{{"attr":"terra","attrPath":["terra"],"drvPath":"{out}.drv","outputs":{{"out":"{out}"}}}}' '{{"attr":"luna","attrPath":["luna"],"drvPath":"{luna}.drv","outputs":{{"out":"{luna}"}}}}'"#
        ),
    )
}

#[test]
fn the_ssh_and_evaluator_settings_reach_the_programs() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = nix_ok(&t, dir.path());
    let eval = eval_recording(&t, dir.path(), &store("nixos-system-terra"));
    let ssh = ssh_recording(&t, dir.path(), &store("nixos-system-terra"));
    let cfg = config(
        dir.path(),
        "[ssh]\ncontrol_persist = 1200\n[timeouts]\nssh_connect = 7\n[eval]\nworkers = 3\nmax_memory_mb = 1234\n",
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ssh_argv = std::fs::read_to_string(dir.path().join("ssh.argv")).unwrap();
    assert!(ssh_argv.contains("ConnectTimeout=7"), "{ssh_argv}");
    assert!(ssh_argv.contains("ControlPersist=1200s"), "{ssh_argv}");
    let eval_argv: Vec<String> = std::fs::read_to_string(dir.path().join("eval.argv"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let after = |flag: &str| {
        eval_argv
            .iter()
            .position(|a| a == flag)
            .and_then(|i| eval_argv.get(i + 1))
            .cloned()
    };
    assert_eq!(after("--workers").as_deref(), Some("3"), "{eval_argv:?}");
    assert_eq!(
        after("--max-memory-size").as_deref(),
        Some("1234"),
        "{eval_argv:?}"
    );
}

#[test]
fn the_probe_timeout_cuts_a_hung_probe_before_the_deadline() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let (nix, eval, ssh) = (
        nix_ok(&t, dir.path()),
        eval_ok(&t, dir.path()),
        ssh_hangs(&t, dir.path()),
    );
    let cfg = config(dir.path(), "[timeouts]\nprobe = 1\nsettle = 120\n");
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let start = Instant::now();
    let out = run_within(&mut cmd, LIMIT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        start.elapsed() < Duration::from_secs(9),
        "the configured 1 s probe timeout: not the 10 s default, nor the 120 s deadline"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "a timed-out probe is a finding: {stdout}"
    );
    let terra = stdout.lines().find(|l| l.contains("terra")).unwrap();
    assert!(
        terra.contains("unreachable") && !terra.contains("global deadline"),
        "{terra}"
    );
}

#[test]
fn the_eval_timeout_cuts_a_hung_evaluator_before_the_deadline() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let (nix, eval, ssh) = (
        nix_ok(&t, dir.path()),
        eval_hangs(&t, dir.path()),
        ssh_ok(&t, dir.path()),
    );
    let cfg = config(dir.path(), "[timeouts]\neval = 1\nsettle = 120\n");
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let start = Instant::now();
    let out = run_within(&mut cmd, LIMIT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "the eval timeout, not the deadline"
    );
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("error: evaluation_failed"), "{stdout}");
    assert!(!stdout.contains("did not settle"), "{stdout}");
}

/// A one-shot HTTP/1.1 responder on 127.0.0.1 for both HTTP adapters: Prometheus's query
/// gets `prometheus`; `HEAD /<cached>.narinfo` gets 200, any other narinfo 404.
fn serve(prometheus: String, cached: &str) -> u16 {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cached = format!("/{cached}.narinfo");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && s.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head);
            let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
            let (status, body) = if path.starts_with("/api/v1/query") {
                ("200 OK", prometheus.clone())
            } else if path == cached {
                ("200 OK", String::new())
            } else {
                ("404 Not Found", String::new())
            };
            let head_only = head.starts_with("HEAD");
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                if head_only { "" } else { &body }
            );
        }
    });
    port
}

#[test]
fn prometheus_answers_and_the_cached_badge_reach_the_report() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let hash = "1123456789abcdfghijklmnpqrsvwxyz";
    let pending = format!("/nix/store/{hash}-nixos-system-terra-new");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // terra has no instance label in the config, so it is asked for as `terra:9100`.
    let prometheus = format!(
        r#"{{"status":"success","data":{{"resultType":"vector","result":[{{"metric":{{"instance":"terra:9100","auspex_metric":"scraped"}},"value":[0,"{now}"]}},{{"metric":{{"instance":"terra:9100","auspex_metric":"uptime"}},"value":[0,"4242"]}}]}}}}"#
    );
    let port = serve(prometheus, hash);
    let nix = nix_ok(&t, dir.path());
    let eval = eval_recording(&t, dir.path(), &pending);
    let ssh = ssh_recording(&t, dir.path(), &store("nixos-system-terra"));
    let cfg = config(
        dir.path(),
        &format!(
            "[prometheus]\nurl = \"http://127.0.0.1:{port}\"\n[cache]\nurl = \"http://127.0.0.1:{port}\"\n"
        ),
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--json", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let terra = report["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "terra")
        .unwrap();
    assert_eq!(
        terra["health"]["uptime_seconds"]["value"], 4242,
        "{terra:#}"
    );
    assert_eq!(terra["health"]["uptime_seconds"]["source"], "prometheus");
    assert!(
        terra["badges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b == "cached"),
        "{terra:#}"
    );
}

/// Review C3: Prometheus down was invisible in headless output.
#[test]
fn prometheus_unavailable_is_said_on_stderr() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    // A port nothing listens on: bound, then released.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (nix, eval, ssh) = (
        nix_ok(&t, dir.path()),
        eval_ok(&t, dir.path()),
        ssh_ok(&t, dir.path()),
    );
    let cfg = config(
        dir.path(),
        &format!("[prometheus]\nurl = \"http://127.0.0.1:{port}\"\n"),
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--json", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("prometheus unavailable"), "{stderr}");
}

/// Review C5: an empty fleet printed nothing at all.
#[test]
fn plain_with_no_machines_says_so() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = t.script(dir.path(), "nix", "echo '{}'");
    let (eval, ssh) = (eval_ok(&t, dir.path()), ssh_ok(&t, dir.path()));
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--flake"]).arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "no machines\n");
}

/// Review C4: a report that could not be delivered is auspex's failure (2), not the
/// fleet's (1); a closed pipe is quiet about it.
#[test]
fn a_report_that_cannot_be_written_exits_2() {
    let dir = tempdir();
    if Path::new("/dev/full").exists() {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let out = auspex(dir.path())
            .args(["--demo", "--json"])
            .stdout(full)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot write the report"));
    } else {
        eprintln!("SKIPPED part: no /dev/full");
    }
    let mut child = auspex(dir.path())
        .args(["--demo", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // the reader is gone before anything is written
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("cannot write"),
        "quiet"
    );
}

/// Review C2: every probe ran at once, so a large fleet ran out of file descriptors. At most
/// `MAX_CONCURRENT` per-machine calls run together; the rest wait.
#[test]
fn at_most_max_concurrent_probes_run_at_once() {
    let Some(t) = tools(&["sleep"]) else { return };
    let limit = auspex_tui::shell::MAX_CONCURRENT;
    let dir = tempdir();
    let fleet: Vec<String> = (0..limit + 8)
        .map(|i| {
            format!(
                r#""m{i:03}":{{"deploy":{{"targetHost":"root@192.0.2.{}"}}}}"#,
                i + 1
            )
        })
        .collect();
    let nix = t.script(
        dir.path(),
        "nix",
        &format!("printf '%s\\n' '{{{}}}'", fleet.join(",")),
    );
    let (eval, ssh) = (eval_ok(&t, dir.path()), ssh_hangs(&t, dir.path()));
    let cfg = config(dir.path(), "[timeouts]\nsettle = 600\nprobe = 600\n");
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let mut child = cmd.spawn().unwrap();
    let pids = wait_for_pids(&dir.path().join("ssh.pids"), limit);
    // Give any probe beyond the limit time to start, if one could.
    std::thread::sleep(Duration::from_millis(1500));
    let all = wait_for_pids(&dir.path().join("ssh.pids"), limit);
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    let status = child.wait().unwrap();
    let left: Vec<i32> = all.iter().copied().filter(|&p| alive(p)).collect();
    cleanup(&all);
    assert_eq!(pids.len(), limit);
    assert_eq!(all.len(), limit, "no more than {limit} probes ran at once");
    assert_eq!(status.code(), Some(128 + Signal::TERM.as_raw()));
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn headless_reports_approval_once_and_uses_the_configured_allowance() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let nix = nix_ok(&t, dir.path());
    let eval = eval_ok(&t, dir.path());
    let ssh = t.script(
        dir.path(),
        "ssh-approval",
        r#"for arg; do
case "$arg" in
  -O) exit 1;;
  true) exec @sleep@ 15;;
esac
done
exit 99"#,
    );
    let cfg = config(
        dir.path(),
        "[timeouts]\nssh_approval = 5\nprobe = 1\nsettle = 30\n",
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--plain", "--config"])
        .arg(cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()));
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let start = Instant::now();
    let out = run_within(&mut cmd, LIMIT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        stderr
            .matches("ssh: connecting/authenticating (agent approval may be pending)")
            .count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        stdout
            .matches("ssh connection/authentication timed out (agent approval may be pending)")
            .count(),
        2,
        "{stdout}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(12),
        "configured 5s approval not used"
    );
}

// ---------------------------------------------------------------------------------------
// The package preview, end to end through the binary
// ---------------------------------------------------------------------------------------

const GLIBC_OLD: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-glibc-2.40-66";
const GLIBC_NEW: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-glibc-2.40-77";
const HTOP: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-htop-3.3.0";
const RIPGREP: &str = "/nix/store/3123456789abcdfghijklmnpqrsvwxyz-ripgrep-14.1.1";

/// A binary cache that has everything: answers every request with 200 and no body, which is
/// all the `.narinfo` lookup needs. Returns its URL; the thread ends with the test process.
fn cache_that_has_everything() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    url
}

/// A fleet where `luna` runs an older system than the evaluation's. The fake ssh answers the
/// probe, and, for the closure query (told apart by the script on its stdin), prints luna's
/// deployed closure; every call's argv is appended to `ssh.argv`. The fake `nix` answers the
/// inventory and `path-info` (the pending closure), and records that it was asked; the fake
/// `nix-store` says the pending toplevel is not valid locally.
fn preview_fakes(t: &Tools, dir: &Path) -> [std::path::PathBuf; 4] {
    let nix = t.script(
        dir,
        "nix",
        &format!(
            r#"case "$1" in
  eval) printf '%s\n' '{INVENTORY}';;
  path-info) printf '%s\n' "$*" >> '@dir@/nix.path-info'
    printf '%s\n' '{GLIBC_NEW}' '{RIPGREP}' '{}';;
  *) exit 64;;
esac"#,
            store("nixos-system-luna-new")
        ),
    );
    let nix_store = t.script(
        dir,
        "nix-store",
        "echo 'error: path is not valid' >&2\nexit 1",
    );
    let eval = {
        let line = |m: &str, out: &str| {
            format!(
                r#"{{"attr":"{m}","attrPath":["{m}"],"drvPath":"{out}.drv","outputs":{{"out":"{out}"}}}}"#
            )
        };
        t.script(
            dir,
            "nix-eval-jobs",
            &format!(
                "printf '%s\\n' '{}' '{}'",
                line("terra", &store("nixos-system-terra")),
                line("luna", &store("nixos-system-luna-new"))
            ),
        )
    };
    let ssh = t.script(
        dir,
        "ssh",
        &format!(
            r#"for a; do [ "$a" = '-O' ] && exit 0; done
printf '%s\n' "$*" >> '@dir@/ssh.argv'
script=$(cat)
case "$script" in
  *'nix-store --query --requisites'*)
    echo query >> '@dir@/ssh.queries'
    printf 'Last login: yesterday\n%s\n%s\n%s\n' '{GLIBC_OLD}' '{HTOP}' '{}';;
  *)
    case "$*" in
      *192.0.2.10*) cur='{}';;
      *) cur='{}';;
    esac
    printf 'auspex_probe=1\nprobe=nixos\ncurrent_system=%s\nsystem_state=running\nfailed_units=[]\nauspex_end=1\n' "$cur";;
esac"#,
            store("nixos-system-luna-old"),
            store("nixos-system-terra"),
            store("nixos-system-luna-old"),
        ),
    );
    [nix, nix_store, eval, ssh]
}

#[test]
fn the_preview_flows_through_the_binary_with_fake_ssh_nix_and_cache() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let [nix, nix_store, eval, ssh] = preview_fakes(&t, dir.path());
    let cache = cache_that_has_everything();
    let cfg = config(dir.path(), &format!("[cache]\nurl = \"{cache}\"\n"));
    let mut cmd = auspex(dir.path());
    cmd.args(["--json", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()))
        .env("AUSPEX_NIX_STORE", &nix_store);
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let report: auspex_domain::report::FleetReport = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report.settled, "{report:?}");
    let luna = report.machines.iter().find(|m| m.name == "luna").unwrap();
    let terra = report.machines.iter().find(|m| m.name == "terra").unwrap();
    assert_eq!(terra.package_diff, None, "in sync: nothing to preview");
    let diff = luna.package_diff.as_ref().expect("luna has a preview");
    assert_eq!(
        diff.state,
        auspex_domain::diff::DiffState::Computed,
        "{diff:?}"
    );
    let names = |list: &[auspex_domain::diff::PackageChange]| -> Vec<String> {
        list.iter().map(|c| c.name.clone()).collect()
    };
    assert_eq!(names(&diff.changed), ["glibc"]);
    assert_eq!(names(&diff.added), ["ripgrep"]);
    assert_eq!(names(&diff.removed), ["htop"]);
    assert_eq!(
        diff.invalid.deployed, 1,
        "the login banner on stdout was dropped and counted"
    );
    assert!(
        luna.settled
            && luna
                .reason
                .as_deref()
                .unwrap()
                .contains("glibc 2.40-66 → 2.40-77")
    );

    // The deployed query went to luna only, once, over the probe's own ssh options; the
    // pending query asked the cache, writing nothing to the store, for exactly luna's toplevel.
    let queries = std::fs::read_to_string(dir.path().join("ssh.queries")).unwrap();
    assert_eq!(queries.lines().count(), 1, "{queries}");
    let path_info = std::fs::read_to_string(dir.path().join("nix.path-info")).unwrap();
    assert_eq!(
        path_info.trim(),
        format!(
            "path-info --recursive --option narinfo-cache-negative-ttl 0 --store {cache}/ -- {}",
            store("nixos-system-luna-new")
        )
    );
}

#[test]
fn diff_enabled_false_never_queries_a_closure_and_leaves_the_field_null() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let [nix, nix_store, eval, ssh] = preview_fakes(&t, dir.path());
    let cache = cache_that_has_everything();
    let cfg = config(
        dir.path(),
        &format!("[cache]\nurl = \"{cache}\"\n[diff]\nenabled = false\n"),
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--json", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()))
        .env("AUSPEX_NIX_STORE", &nix_store);
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    assert_eq!(out.status.code(), Some(0));
    let report: auspex_domain::report::FleetReport = serde_json::from_slice(&out.stdout).unwrap();
    for m in &report.machines {
        assert_eq!(m.package_diff, None, "{}", m.name);
    }
    assert!(!dir.path().join("ssh.queries").exists());
    assert!(!dir.path().join("nix.path-info").exists());
}

#[test]
fn a_preview_that_fails_is_failed_and_the_scan_still_exits_0() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let [nix, nix_store, eval, _] = preview_fakes(&t, dir.path());
    // This ssh answers the probe and fails the closure query.
    let ssh = t.script(
        dir.path(),
        "ssh-failing",
        &format!(
            r#"for a; do [ "$a" = '-O' ] && exit 0; done
script=$(cat)
case "$script" in
  *'nix-store --query --requisites'*) echo 'error: cannot connect to daemon' >&2; exit 1;;
esac
printf 'auspex_probe=1\nprobe=nixos\ncurrent_system={}\nsystem_state=running\nfailed_units=[]\nauspex_end=1\n'"#,
            store("nixos-system-luna-old")
        ),
    );
    let cache = cache_that_has_everything();
    let cfg = config(dir.path(), &format!("[cache]\nurl = \"{cache}\"\n"));
    let mut cmd = auspex(dir.path());
    cmd.args(["--json", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()))
        .env("AUSPEX_NIX_STORE", &nix_store);
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a failed preview is not a failed scan"
    );
    let report: auspex_domain::report::FleetReport = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report.settled && report.errors.is_empty());
    for m in &report.machines {
        // Both machines run the "old" luna system, which differs from terra's evaluation
        // only for terra; luna's pending toplevel is the new one. Either way no machine is
        // reported unreachable or failed by a failed preview.
        assert_ne!(
            m.status,
            auspex_domain::status::Badge::Unreachable,
            "{}",
            m.name
        );
    }
    let luna = report.machines.iter().find(|m| m.name == "luna").unwrap();
    assert_eq!(luna.status, auspex_domain::status::Badge::UpdatePending);
    let diff = luna.package_diff.as_ref().unwrap();
    assert_eq!(diff.state, auspex_domain::diff::DiffState::Failed);
    assert!(
        diff.reason
            .as_deref()
            .unwrap()
            .contains("cannot connect to daemon"),
        "{diff:?}"
    );
}

// ---------------------------------------------------------------------------------------
// Previews and the shared permit pool (review F1)
// ---------------------------------------------------------------------------------------

/// Machines `m00`..`m17` (18, so previews alone could hold more than the 32 shared permits),
/// all update pending and cached. Every closure query takes 5 s; `m17`'s probe takes 2 s, so
/// its cache lookup comes after every other machine's previews have started.
fn crowd_fakes(t: &Tools, dir: &Path, machines: usize) -> [std::path::PathBuf; 3] {
    let name = |i: usize| format!("m{i:02}");
    let inventory = format!(
        "{{{}}}",
        (0..machines)
            .map(|i| format!(
                r#""{}":{{"deploy":{{"targetHost":"root@192.0.2.{}"}}}}"#,
                name(i),
                10 + i
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let closure = |side: &str| {
        format!(
            "printf '%s\\n' '{GLIBC_OLD}' '{HTOP}' \"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-$m-{side}\""
        )
    };
    let nix = t.script(
        dir,
        "nix",
        &format!(
            r#"case "$1" in
  eval) printf '%s\n' '{inventory}';;
  path-info)
    for last; do :; done
    echo + >> '@dir@/conc.log'
    @sleep@ 5
    echo - >> '@dir@/conc.log'
    printf '%s\n' '{GLIBC_NEW}' '{RIPGREP}' "$last";;
  *) exit 64;;
esac"#
        ),
    );
    let eval = t.script(
        dir,
        "nix-eval-jobs",
        &(0..machines)
            .map(|i| {
                let out = store(&format!("nixos-system-{}-new", name(i)));
                format!(
                    r#"printf '%s\n' '{{"attr":"{m}","attrPath":["{m}"],"drvPath":"{out}.drv","outputs":{{"out":"{out}"}}}}'"#,
                    m = name(i)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let ssh = t.script(
        dir,
        "ssh",
        &format!(
            r#"for a; do [ "$a" = '-O' ] && exit 0; done
for a; do case "$prev" in --) dest=$a;; esac; prev=$a; done
n=${{dest##*.}}
m=$(printf 'm%02d' $((n - 10)))
script=$(@cat@)
case "$script" in
  *'nix-store --query --requisites'*)
    echo + >> '@dir@/conc.log'
    @sleep@ 5
    echo - >> '@dir@/conc.log'
    {}
    ;;
  *)
    [ "$m" = 'm{:02}' ] && @sleep@ 2
    printf 'auspex_probe=1\nprobe=nixos\ncurrent_system=/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-%s-old\nsystem_state=running\nfailed_units=[]\nauspex_end=1\n' "$m";;
esac"#,
            closure("old"),
            machines - 1
        ),
    );
    [nix, eval, ssh]
}

#[test]
fn many_previews_never_starve_a_late_machines_cache_lookup() {
    crowd(4, 40);
}

/// Re-review N2: previews queued for a slot do not spend their `timeouts.diff`. Each query
/// takes 5 s and `diff` is 8 s, so a budget that counted the wait would cut every wave after
/// the first; the clock starts when a query holds its slot, so all 18 machines compute.
#[test]
fn previews_waiting_for_a_slot_do_not_spend_their_timeout() {
    crowd(30, 8);
}

fn crowd(settle: u64, diff: u64) {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let [nix, eval, ssh] = crowd_fakes(&t, dir.path(), 18);
    let nix_store = t.script(
        dir.path(),
        "nix-store",
        "echo 'error: path is not valid' >&2\nexit 1",
    );
    let cache = cache_that_has_everything();
    let cfg = config(
        dir.path(),
        &format!("[cache]\nurl = \"{cache}\"\n[timeouts]\nsettle = {settle}\ndiff = {diff}\n"),
    );
    let mut cmd = auspex(dir.path());
    cmd.args(["--json", "--config"])
        .arg(&cfg)
        .arg("--flake")
        .arg(flake_dir(dir.path()))
        .env("AUSPEX_NIX_STORE", &nix_store);
    with_fakes(&mut cmd, &nix, &eval, &ssh);
    let out = run_within(&mut cmd, LIMIT);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "slow previews must not time the scan out: {stderr}"
    );
    // Both kinds of query count against the preview pool of 8 (4 machines, two queries each),
    // and that pool is not the shared one: replay the + / - log the fakes wrote.
    let log = std::fs::read_to_string(dir.path().join("conc.log")).unwrap();
    let (mut now, mut peak) = (0i32, 0i32);
    for line in log.lines() {
        now += if line == "+" { 1 } else { -1 };
        peak = peak.max(now);
    }
    assert_eq!(now, 0, "every query ended: {log}");
    assert!(
        (1..=8).contains(&peak),
        "at most 8 closure queries at once, saw {peak}"
    );
    let report: auspex_domain::report::FleetReport = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report.settled && report.errors.is_empty());
    let late = report.machines.iter().find(|m| m.name == "m17").unwrap();
    assert!(
        late.badges.contains(&auspex_domain::status::Badge::Cached),
        "the late machine keeps its cached badge: {late:?}"
    );
    for m in &report.machines {
        let diff = m.package_diff.as_ref().unwrap();
        assert_eq!(
            diff.state,
            auspex_domain::diff::DiffState::Computed,
            "{}: {:?}",
            m.name,
            diff.reason
        );
    }
}
