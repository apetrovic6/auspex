//! Finding and running `clan` against fake `nix` and fake `clan` scripts, for what the real
//! tools cannot show cheaply: each step's failure, a timeout and a cancellation killing the
//! whole process group, the configured path winning without nix, and the shape of a run's
//! output. Nothing here runs the real `clan` or the real `nix` (`tests/clan_fixture.rs` runs
//! the real `nix` against a fixture flake).

#![cfg(unix)]

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use auspex_effects::clan::{
    ClanLine, ClanLocator, ClanSource, ClanStep, ResolveParams, RunEnd, RunParams, Stream, run,
};
use auspex_effects::nix::FlakeRef;
use auspex_effects::{CancellationToken, Cancelled};
use common::{Tools, cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const REV: &str = "dbae2100000000000000000000000000000000aa";

/// The lock of a fleet that pins `clan-core` (or does not): `flake.lock`'s own content.
fn locks(with_clan_core: bool) -> String {
    let inputs = if with_clan_core {
        r#""clan-core":"clan-core""#
    } else {
        r#""nixpkgs":"nixpkgs""#
    };
    format!(
        r#"{{"nodes":{{"clan-core":{{"locked":{{"rev":"{REV}","narHash":"sha256-core","type":"github"}}}},"nixpkgs":{{"locked":{{"rev":"abc","type":"github"}}}},"root":{{"inputs":{{{inputs}}}}}}},"root":"root","version":7}}"#
    )
}

/// What `nix flake metadata --json` prints for it.
fn lock_json(with_clan_core: bool) -> String {
    format!(r#"{{"locks":{}}}"#, locks(with_clan_core))
}

/// A fake `nix`: `flake metadata ...` runs `metadata`, `build ...` runs `build`. Both are
/// `sh` snippets; `@dir@` and `@sleep@` are replaced.
fn fake_nix(t: &Tools, dir: &Path, metadata: &str, build: &str) -> PathBuf {
    t.script(
        dir,
        "nix",
        &format!(
            "echo \"$@\" >> '@dir@/argv'\ncase \"$1\" in\nflake)\n{metadata}\n;;\nbuild)\necho x >> '@dir@/built'\n{build}\n;;\nesac"
        ),
    )
}

fn metadata_ok() -> String {
    format!("printf '%s\\n' '{}'", lock_json(true))
}

/// A local fleet directory whose `flake.lock` agrees with the fake `nix`'s metadata, as the
/// resolution requires.
fn fleet() -> (tempfile::TempDir, FlakeRef) {
    let dir = tempdir();
    fs::write(dir.path().join("flake.lock"), locks(true)).unwrap();
    let flake = FlakeRef::new(dir.path().to_str().unwrap()).unwrap();
    (dir, flake)
}

async fn resolve<'a>(
    nix: &'a Path,
    tweak: impl FnOnce(&mut ResolveParams<'a>),
    cancel: &CancellationToken,
) -> Result<Result<auspex_effects::clan::ClanBinary, auspex_effects::clan::ClanError>, Cancelled> {
    let mut p = common::resolve_params();
    p.nix = nix.as_os_str();
    p.system = Some("x86_64-linux");
    p.lock_timeout = Duration::from_secs(10);
    p.build_timeout = Duration::from_secs(10);
    tweak(&mut p);
    let (_fleet, flake) = fleet();
    ClanLocator::new().resolve(&p, &flake, cancel).await
}

#[tokio::test]
async fn a_configured_path_wins_and_nix_is_never_run() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    let clan = t.script(dir.path(), "my-clan", "echo clan");
    let got = resolve(
        &nix,
        |p| p.config_path = Some(&clan),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(got.path, clan);
    assert_eq!(got.source, ClanSource::Config);
    assert_eq!(got.source.to_string(), "config");
    assert_eq!(got.revision, None);
    assert!(
        got.trail
            .iter()
            .any(|l| l.contains("explicit path from configuration")),
        "{:?}",
        got.trail
    );
    assert!(
        !dir.path().join("argv").exists(),
        "nix ran although a path was configured"
    );
}

#[tokio::test]
async fn a_configured_path_must_be_an_executable_regular_file() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    let plain = dir.path().join("plain");
    fs::write(&plain, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
    let missing = dir.path().join("missing");
    let relative = PathBuf::from("clan");
    for (path, why) in [
        (plain.as_path(), "not executable"),
        (missing.as_path(), "No such file"),
        (dir.path(), "not a regular file"),
        (relative.as_path(), "not an absolute path"),
    ] {
        let e = resolve(
            &nix,
            |p| p.config_path = Some(path),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(e.step, ClanStep::Config, "{e}");
        assert!(e.reason.contains(why), "{path:?}: {e}");
    }
    // A configured path that is wrong is an error, not a reason to look elsewhere.
    assert!(!dir.path().join("argv").exists());
}

#[tokio::test]
async fn a_lock_without_clan_core_is_a_lock_error_and_nothing_is_built() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(
        &t,
        dir.path(),
        &format!("printf '%s\\n' '{}'", lock_json(false)),
        "echo /nix/store/x",
    );
    let e = resolve(&nix, |_| {}, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("no input named clan-core"), "{e}");
    assert!(e.to_string().starts_with("reading the fleet lock:"), "{e}");
    assert!(!dir.path().join("built").exists(), "built without a pin");
}

#[tokio::test]
async fn a_lock_file_that_does_not_pin_what_nix_resolved_is_out_of_date() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    // nix says clan-core is at REV; the file on disk pins something else.
    let (fleet, flake) = fleet();
    let file = fleet.path().join("flake.lock");
    fs::write(&file, locks(true).replace(REV, &"0".repeat(40))).unwrap();
    let mut p = common::resolve_params();
    p.nix = nix.as_os_str();
    p.system = Some("x86_64-linux");
    let e = ClanLocator::new()
        .resolve(&p, &flake, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("out of date"), "{e}");
    assert!(
        !dir.path().join("built").exists(),
        "built from a lock that is out of date"
    );
    // And a lock file that is simply not there.
    fs::remove_file(&file).unwrap();
    let e = ClanLocator::new()
        .resolve(&p, &flake, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("no usable flake.lock"), "{e}");
}

#[tokio::test]
async fn nix_failing_to_read_the_lock_names_the_step_and_its_stderr() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(
        &t,
        dir.path(),
        "printf 'error: cannot write modified lock file\\n' >&2\nexit 1",
        "echo /nix/store/x",
    );
    let e = resolve(&nix, |_| {}, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("exited 1"), "{e}");
    assert!(e.reason.contains("cannot write modified lock file"), "{e}");
}

#[tokio::test]
async fn a_failed_build_carries_its_sanitised_stderr_tail() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(
        &t,
        dir.path(),
        &metadata_ok(),
        "printf '\\033[31merror:\\033[0m builder failed\\n' >&2\nprintf 'last line\\n' >&2\nexit 1",
    );
    let e = resolve(&nix, |_| {}, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Build);
    assert!(e.reason.contains("exited 1"), "{e}");
    assert!(e.reason.contains("error: builder failed\nlast line"), "{e}");
    assert!(!e.reason.contains('\u{1b}'), "{e:?}");
}

#[tokio::test]
async fn a_build_that_prints_nothing_usable_is_a_binary_error() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    for (printed, why) in [
        ("", "no output path"),
        ("/tmp/elsewhere", "not a store path"),
        (
            "/nix/store/00000000000000000000000000000000-absent",
            "no usable bin/clan",
        ),
    ] {
        let nix = fake_nix(
            &t,
            dir.path(),
            &metadata_ok(),
            &format!("printf '%s' '{printed}'"),
        );
        let e = resolve(&nix, |_| {}, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(e.step, ClanStep::Binary, "{printed:?}: {e}");
        assert!(e.reason.contains(why), "{printed:?}: {e}");
    }
}

#[tokio::test]
async fn a_missing_nix_is_named() {
    let dir = tempdir();
    let e = resolve(
        &dir.path().join("no-such-nix"),
        |_| {},
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("no-such-nix"), "{e}");
}

#[tokio::test]
async fn a_build_past_its_deadline_is_killed_with_its_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let nix = fake_nix(
        &t,
        dir.path(),
        &metadata_ok(),
        "@sleep@ 60 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let began = Instant::now();
    let e = resolve(
        &nix,
        |p| p.build_timeout = Duration::from_secs(2),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(e.step, ClanStep::Build);
    assert!(e.reason.contains("timed out"), "{e}");
    let pids = wait_for_pids(&dir.path().join("pids"), 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "still alive after the timeout: {left:?}");
}

#[tokio::test]
async fn reading_the_lock_past_its_deadline_is_a_lock_timeout() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), "@sleep@ 60", "echo /nix/store/x");
    let began = Instant::now();
    let e = resolve(
        &nix,
        |p| p.lock_timeout = Duration::from_millis(300),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    // The lock read's own deadline (the build's is 10 s here), not the build's.
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("timed out"), "{e}");
}

#[tokio::test]
async fn cancelling_a_build_kills_it_with_its_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let nix = fake_nix(
        &t,
        dir.path(),
        &metadata_ok(),
        "@sleep@ 60 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
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
    let began = Instant::now();
    let r = resolve(&nix, |p| p.build_timeout = Duration::from_secs(60), &cancel).await;
    canceller.await.unwrap();
    assert_eq!(r.unwrap_err(), Cancelled);
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    let pids = wait_for_pids(&pids_file, 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "still alive after the cancel: {left:?}");
}

// --- run ---

fn argv(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

async fn collect(rx: &mut UnboundedReceiver<ClanLine>) -> Vec<ClanLine> {
    let mut v = Vec::new();
    while let Some(l) = rx.recv().await {
        v.push(l);
    }
    v
}

fn of(lines: &[ClanLine], s: Stream) -> Vec<&str> {
    lines
        .iter()
        .filter(|l| l.stream == s)
        .map(|l| l.text.as_str())
        .collect()
}

#[tokio::test]
async fn run_streams_lines_in_order_from_the_fleet_directory_with_flake_passed() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let clan = t.script(
        dir.path(),
        "clan",
        "printf 'cwd=%s\\n' \"$PWD\"\nfor a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done\nprintf 'one\\ntwo\\n'\nprintf 'warn\\n' >&2\nprintf '\\033[32mthree\\033[0m\\n'\nexit 3",
    );
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let (tx, mut rx) = unbounded_channel();
    let ran = run(
        &clan,
        &flake,
        &argv(&["machines", "update", "m one"]),
        &RunParams::new(),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    let lines = collect(&mut rx).await;
    assert_eq!(
        ran.end,
        RunEnd::Exited {
            code: Some(3),
            signal: None
        }
    );
    assert!(!ran.end.success());
    let out = of(&lines, Stream::Stdout);
    let cwd = fs::canonicalize(fleet.path()).unwrap();
    assert_eq!(out[0], format!("cwd={}", cwd.display()));
    assert_eq!(
        &out[1..],
        [
            "arg=machines",
            "arg=update",
            "arg=m one",
            "arg=--flake",
            &format!("arg={}", cwd.display()),
            "one",
            "two",
            "three",
        ]
    );
    assert_eq!(of(&lines, Stream::Stderr), ["warn"]);
    assert_eq!((ran.lines_dropped, ran.long_lines), (0, 0));
}

#[tokio::test]
async fn run_delivers_lines_while_the_process_is_still_running() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let clan = t.script(
        dir.path(),
        "clan",
        "echo first\nwhile [ ! -e '@dir@/go' ]; do @sleep@ 0.02; done\necho second",
    );
    let (tx, mut rx) = unbounded_channel();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let params = RunParams::new();
    let args = argv(&["x"]);
    let fut = run(&clan, &flake, &args, &params, &cancel, tx);
    let check = async {
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a line within 5 s")
            .unwrap();
        assert_eq!(first.text, "first");
        assert!(
            !dir.path().join("go").exists(),
            "the run had already finished"
        );
        fs::write(dir.path().join("go"), "").unwrap();
        let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a line within 5 s")
            .unwrap();
        assert_eq!(second.text, "second");
    };
    let (r, ()) = tokio::join!(fut, check);
    assert!(r.unwrap().unwrap().end.success());
}

#[tokio::test]
async fn run_caps_line_length_and_line_count_without_stopping_the_process() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "echo short\nprintf '%0200d\\n' 0\necho a\necho b\necho c\necho done > '@dir@/finished'",
    );
    let (tx, mut rx) = unbounded_channel();
    let params = RunParams {
        line_cap: 50,
        max_lines: 3,
        ..RunParams::new()
    };
    let ran = run(&clan, &flake, &[], &params, &CancellationToken::new(), tx)
        .await
        .unwrap()
        .unwrap();
    let lines = collect(&mut rx).await;
    let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
    // The long line is a note, the cap on lines is announced once, and the rest is counted.
    assert_eq!(texts[0], "short");
    assert!(texts[1].contains("more than 50 bytes"), "{texts:?}");
    assert_eq!(texts[2], "a");
    assert!(texts[3].contains("3 lines shown"), "{texts:?}");
    assert_eq!(texts.len(), 4, "{texts:?}");
    assert_eq!((ran.long_lines, ran.lines_dropped), (1, 2));
    // The process was left to finish: capping the log never kills a deploy.
    assert!(ran.end.success());
    assert!(dir.path().join("finished").exists());
}

#[tokio::test]
async fn cancelling_a_run_kills_the_process_group_and_reaps_it() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    // `ssh` and `nixos-rebuild`, standing in: children that outlive a polite exit.
    let clan = t.script(
        dir.path(),
        "clan",
        "@sleep@ 60 &\necho $! >> '@dir@/pids'\n@sleep@ 60 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\necho started\nwait",
    );
    let cancel = CancellationToken::new();
    let pids_file = dir.path().join("pids");
    let (tx, mut rx) = unbounded_channel();
    let began = Instant::now();
    let params = RunParams::new();
    let fut = run(&clan, &flake, &[], &params, &cancel, tx);
    // The script writes its three pids and then prints `started`: once that line is here the
    // group is all there, and the cancel cannot race the line.
    let driver = async {
        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a line within 10 s")
            .unwrap();
        assert_eq!(first.text, "started");
        cancel.cancel();
    };
    let (r, ()) = tokio::join!(fut, driver);
    assert_eq!(r.unwrap_err(), Cancelled);
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    let pids = wait_for_pids(&pids_file, 3);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "still alive after the cancel: {left:?}");
}

#[tokio::test]
async fn a_run_past_its_deadline_is_killed_with_its_children_and_keeps_its_output() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "echo before\n@sleep@ 60 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let (tx, mut rx) = unbounded_channel();
    let params = RunParams {
        timeout: Duration::from_secs(2),
        ..RunParams::new()
    };
    let ran = run(&clan, &flake, &[], &params, &CancellationToken::new(), tx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.end, RunEnd::TimedOut);
    let pids = wait_for_pids(&dir.path().join("pids"), 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "still alive after the timeout: {left:?}");
    assert_eq!(of(&collect(&mut rx).await, Stream::Stdout), ["before"]);
}

#[tokio::test]
async fn a_dropped_receiver_stops_the_run_like_a_cancel() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "echo $$ >> '@dir@/pids'\nwhile :; do echo line; @sleep@ 0.05; done",
    );
    let (tx, mut rx) = unbounded_channel();
    let reader = tokio::spawn(async move {
        // One line, then nobody is listening.
        rx.recv().await;
    });
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        run(
            &clan,
            &flake,
            &[],
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        ),
    )
    .await
    .expect("the run did not stop when its receiver went");
    reader.await.unwrap();
    assert_eq!(r.unwrap_err(), Cancelled);
    let pids = wait_for_pids(&dir.path().join("pids"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(
        left.is_empty(),
        "still alive after the receiver went: {left:?}"
    );
}

#[tokio::test]
async fn a_program_that_cannot_run_is_a_run_error() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let plain = dir.path().join("plain");
    fs::write(&plain, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
    let _ = t;
    for (program, why) in [
        (plain, "not executable"),
        (dir.path().join("missing"), "not found"),
    ] {
        let (tx, _rx) = unbounded_channel();
        let e = run(
            &program,
            &flake,
            &[],
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(e.step, ClanStep::Run);
        assert!(e.reason.contains(why), "{e}");
    }
}

// --- Fix round 2 ---

/// A fake clan that prints `deployed`, exits 0, and leaves a descendant in a session of its
/// own holding stdout open: the plan's `ControlPersist` trap, the ssh master that outlives the
/// command that started it.
fn deploy_with_lingering_master(t: &Tools, dir: &Path) -> PathBuf {
    t.script(
        dir,
        "clan",
        "echo $$ > '@dir@/clan.pid'\n@setsid@ @sh@ -c 'echo $$ >> \"@dir@/pids\"; exec @sleep@ 3' &\nwhile [ ! -s '@dir@/pids' ]; do @sleep@ 0.02; done\necho deployed\nexit 0",
    )
}

/// F6 (R5): once clan has exited its status is known, and a cancel after that only cuts the
/// drain of the pipes short. The status is what step 2 writes into its log.
#[tokio::test]
async fn a_cancel_after_clan_has_exited_keeps_its_exit_status() {
    let Some(t) = tools(&["setsid", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = deploy_with_lingering_master(&t, dir.path());
    let cancel = CancellationToken::new();
    let (tx, mut rx) = unbounded_channel();
    let params = RunParams::new();
    let fut = run(&clan, &flake, &[], &params, &cancel, tx);
    let driver = async {
        let line = rx.recv().await.unwrap();
        assert_eq!(line.text, "deployed");
        // Cancel once clan has actually exited (observed, not timed): its descendant still
        // holds the pipe, so `run` is in its drain.
        let pid = dir.path().join("clan.pid");
        tokio::task::spawn_blocking(move || wait_for_exit(&pid))
            .await
            .unwrap();
        cancel.cancel();
    };
    let (r, ()) = tokio::join!(fut, driver);
    let pids = wait_for_pids(&dir.path().join("pids"), 1);
    cleanup(&pids);
    let ran = r.expect("cancelled after the exit was known").unwrap();
    assert_eq!(
        ran.end,
        RunEnd::Exited {
            code: Some(0),
            signal: None
        }
    );
}

/// F6, the same for a receiver that goes away after the exit.
#[tokio::test]
async fn a_receiver_that_goes_after_clan_has_exited_keeps_its_exit_status() {
    let Some(t) = tools(&["setsid", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = deploy_with_lingering_master(&t, dir.path());
    let (tx, mut rx) = unbounded_channel::<ClanLine>();
    let pid = dir.path().join("clan.pid");
    let reader = tokio::spawn(async move {
        let line = rx.recv().await.unwrap();
        assert_eq!(line.text, "deployed");
        tokio::task::spawn_blocking(move || wait_for_exit(&pid))
            .await
            .unwrap();
        // `rx` is dropped here: nobody is listening any more.
    });
    let r = run(
        &clan,
        &flake,
        &[],
        &RunParams::new(),
        &CancellationToken::new(),
        tx,
    )
    .await;
    reader.await.unwrap();
    let pids = wait_for_pids(&dir.path().join("pids"), 1);
    cleanup(&pids);
    assert!(
        r.expect("cancelled after the exit was known")
            .unwrap()
            .end
            .success()
    );
}

/// F7: a flake reference with a query is refused by `run`, not guessed at.
#[tokio::test]
async fn run_refuses_a_flake_reference_with_a_query() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let clan = t.script(dir.path(), "clan", "echo x > '@dir@/ran'");
    for reference in [
        "path:/fleet?dir=sub",
        "git+file:///fleet?dir=sub",
        "github:o/r?dir=x",
    ] {
        let (tx, _rx) = unbounded_channel();
        let e = run(
            &clan,
            &FlakeRef::new(reference).unwrap(),
            &argv(&["machines", "update", "m"]),
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(e.step, ClanStep::Run, "{reference}: {e}");
        assert!(e.reason.contains("query"), "{reference}: {e}");
    }
    assert!(
        !dir.path().join("ran").exists(),
        "clan ran with a guessed flake"
    );
}

/// F7: and by the resolution, for a local flake whose lock file it could not find.
#[tokio::test]
async fn resolve_refuses_a_local_flake_reference_with_a_query_before_running_nix() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    let fleet = tempdir();
    for reference in [
        format!("path:{}?dir=sub", fleet.path().display()),
        format!("git+file://{}?dir=sub", fleet.path().display()),
    ] {
        let flake = FlakeRef::new(&reference).unwrap();
        let mut p = common::resolve_params();
        p.nix = nix.as_os_str();
        let e = ClanLocator::new()
            .resolve(&p, &flake, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(e.step, ClanStep::Lock, "{reference}: {e}");
        assert!(e.reason.contains("query"), "{reference}: {e}");
    }
    assert!(
        !dir.path().join("argv").exists(),
        "nix ran for a reference it cannot check"
    );
}

/// F8: clan runs with no askpass program and `SSH_ASKPASS_REQUIRE=never` (the probe's policy),
/// and no input: stdin is `/dev/null`, so a prompt fails rather than hangs.
#[tokio::test]
async fn clan_runs_with_no_askpass_and_no_stdin() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "printf 'askpass=%s\\n' \"${SSH_ASKPASS-unset}\"\nprintf 'git=%s\\n' \"${GIT_ASKPASS-unset}\"\nprintf 'sudo=%s\\n' \"${SUDO_ASKPASS-unset}\"\nprintf 'require=%s\\n' \"${SSH_ASKPASS_REQUIRE-unset}\"\nif read -r x; then echo \"stdin=$x\"; else echo stdin=eof; fi",
    );
    let (tx, mut rx) = unbounded_channel();
    run(
        &clan,
        &flake,
        &[],
        &RunParams::new(),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    let lines = collect(&mut rx).await;
    assert_eq!(
        of(&lines, Stream::Stdout),
        [
            "askpass=unset",
            "git=unset",
            "sudo=unset",
            "require=never",
            "stdin=eof"
        ]
    );
}

/// Survivor M4: a git fleet is run from, and given, its worktree.
#[tokio::test]
async fn run_in_a_git_fleet_uses_its_worktree_as_directory_and_flake() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    fs::create_dir(fleet.path().join(".git")).unwrap();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    assert!(
        flake.as_str().starts_with("git+file://"),
        "{}",
        flake.as_str()
    );
    let clan = t.script(
        dir.path(),
        "clan",
        "printf 'cwd=%s\\n' \"$PWD\"\nfor a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done",
    );
    let (tx, mut rx) = unbounded_channel();
    run(
        &clan,
        &flake,
        &argv(&["machines", "update"]),
        &RunParams::new(),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    let wt = fs::canonicalize(fleet.path()).unwrap();
    assert_eq!(
        of(&collect(&mut rx).await, Stream::Stdout),
        [
            format!("cwd={}", wt.display()),
            "arg=machines".to_owned(),
            "arg=update".to_owned(),
            "arg=--flake".to_owned(),
            format!("arg={}", wt.display()),
        ]
    );
}

/// Survivor M4: the lock file of a git fleet is checked on disk too.
#[tokio::test]
async fn the_lock_file_of_a_git_fleet_is_checked_on_disk() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    let fleet = tempdir();
    fs::create_dir(fleet.path().join(".git")).unwrap();
    fs::write(
        fleet.path().join("flake.lock"),
        locks(true).replace(REV, &"0".repeat(40)),
    )
    .unwrap();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    assert!(
        flake.as_str().starts_with("git+file://"),
        "{}",
        flake.as_str()
    );
    let mut p = common::resolve_params();
    p.nix = nix.as_os_str();
    let e = ClanLocator::new()
        .resolve(&p, &flake, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("out of date"), "{e}");
}

/// Survivor M13: lines already waiting in the pipe when the deadline passes are still
/// delivered. The order is forced: the runtime's only thread is blocked past the deadline
/// while the child prints, so when `run` next looks, the deadline and the line are both ready.
#[tokio::test]
async fn output_waiting_in_the_pipe_at_the_deadline_is_still_delivered() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "while [ ! -e '@dir@/go' ]; do @sleep@ 0.02; done\necho late\n: > '@dir@/printed'\n@sleep@ 60",
    );
    let (tx, mut rx) = unbounded_channel();
    let params = RunParams {
        timeout: Duration::from_millis(300),
        ..RunParams::new()
    };
    let cancel = CancellationToken::new();
    let began = Instant::now();
    let fut = run(&clan, &flake, &[], &params, &cancel, tx);
    tokio::pin!(fut);
    std::future::poll_fn(|cx| {
        assert!(fut.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    fs::write(dir.path().join("go"), "").unwrap();
    // Block the runtime until the line is in the pipe (observed: the script says so) and the
    // deadline has passed (its definition: `timeout` after `run` began).
    wait_for_file(&dir.path().join("printed"));
    while began.elapsed() < params.timeout + Duration::from_millis(100) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let ran = fut.await.unwrap().unwrap();
    assert_eq!(ran.end, RunEnd::TimedOut);
    assert_eq!(of(&collect(&mut rx).await, Stream::Stdout), ["late"]);
}

/// Survivor M15: when clan exits on its own, whatever it left running in its group is killed
/// (step 2's acceptance: clan is not orphaned).
#[tokio::test]
async fn a_normal_exit_kills_what_clan_left_in_its_group() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "@sleep@ 60 &\necho $! >> '@dir@/pids'\necho done\nexit 0",
    );
    let (tx, _rx) = unbounded_channel();
    let ran = run(
        &clan,
        &flake,
        &[],
        &RunParams::new(),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(ran.end.success());
    let pids = wait_for_pids(&dir.path().join("pids"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "left running after clan exited: {left:?}");
}

/// F6: an exit that is already there wins over a cancellation that is ready at the same moment.
/// The order is forced: the runtime's only thread is blocked while clan exits and the token is
/// cancelled, so when `run` next looks, both are ready.
#[tokio::test]
async fn an_exit_that_is_already_there_wins_over_a_cancel_ready_with_it() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "echo $$ > '@dir@/pid'\necho done\nexit 7",
    );
    let (tx, _rx) = unbounded_channel();
    let cancel = CancellationToken::new();
    let params = RunParams::new();
    let fut = run(&clan, &flake, &[], &params, &cancel, tx);
    tokio::pin!(fut);
    std::future::poll_fn(|cx| {
        assert!(fut.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    // The script has exited (observed: it is a zombie); the runtime has not looked.
    wait_for_zombie(&dir.path().join("pid"));
    cancel.cancel();
    let ran = fut.await.expect("an exit that was already there").unwrap();
    assert_eq!(
        ran.end,
        RunEnd::Exited {
            code: Some(7),
            signal: None
        }
    );
}

/// Round 3, F: no test can write the developer's real state directory. Every test takes its
/// `ResolveParams` from `common::resolve_params()`, which has a private GC root directory; a
/// test that builds the defaults itself is found here.
#[test]
fn every_test_resolves_with_a_private_gc_root_directory() {
    // Built at run time, so that this file does not contain what it looks for.
    let needle = ["ResolveParams", "::new", "()"].concat();
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut seen = 0;
    for entry in fs::read_dir(&tests).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        seen += 1;
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&needle),
            "{}: builds the default ResolveParams itself; use common::resolve_params()",
            path.display()
        );
    }
    assert!(seen >= 5, "found only {seen} test files");
    let common = fs::read_to_string(tests.join("common/mod.rs")).unwrap();
    assert!(common.contains("gcroot_dir: Some(dir)"));
}

// --- Fix round 3 ---

/// `dir` as a path relative to the working directory the tests run in, which is how an operator
/// writes `path:./fleet` or `path:fleet`. Climbs to the root, then down: every `..` past the
/// root stays at the root.
fn relative_to_cwd(dir: &Path) -> String {
    let cwd = std::env::current_dir().unwrap();
    let up = "../".repeat(cwd.components().count());
    format!("{up}{}", dir.display().to_string().trim_start_matches('/'))
}

/// The relative local references the review found to skip the disk check: with a scheme, so
/// that nothing made them absolute.
fn relative_forms(dir: &Path) -> Vec<String> {
    let rel = relative_to_cwd(dir);
    vec![
        format!("path:{rel}"),
        format!("path:./{rel}"),
        format!("git+file:{rel}"),
        format!("git+file:./{rel}"),
    ]
}

/// A (A): a relative `path:` or `git+file:` reference is the same fleet as the absolute one:
/// its lock file is checked on disk, and clan is run in it.
#[tokio::test]
async fn a_relative_local_reference_gets_the_disk_check() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    // A git directory (so the `git+file:` forms have something to name), whose lock pins
    // something other than what nix will report.
    let fleet = tempdir();
    fs::create_dir(fleet.path().join(".git")).unwrap();
    fs::write(
        fleet.path().join("flake.lock"),
        locks(true).replace(REV, &"0".repeat(40)),
    )
    .unwrap();
    for reference in relative_forms(fleet.path()) {
        let flake = FlakeRef::new(&reference).unwrap();
        let mut p = common::resolve_params();
        p.nix = nix.as_os_str();
        let e = ClanLocator::new()
            .resolve(&p, &flake, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(e.step, ClanStep::Lock, "{reference}: {e}");
        assert!(e.reason.contains("out of date"), "{reference}: {e}");
    }
}

/// A (A): and clan is given that directory, absolute, and run in it.
#[tokio::test]
async fn a_relative_local_reference_is_run_in_its_directory() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let clan = t.script(
        dir.path(),
        "clan",
        "printf 'cwd=%s\\n' \"$PWD\"\nfor a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done",
    );
    let fleet = tempdir();
    fs::create_dir(fleet.path().join(".git")).unwrap();
    let want = fs::canonicalize(fleet.path()).unwrap();
    for reference in relative_forms(fleet.path()) {
        let (tx, mut rx) = unbounded_channel();
        run(
            &clan,
            &FlakeRef::new(&reference).unwrap(),
            &argv(&["machines", "update"]),
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap_or_else(|_| panic!("{reference}: cancelled"))
        .unwrap_or_else(|e| panic!("{reference}: {e}"));
        assert_eq!(
            of(&collect(&mut rx).await, Stream::Stdout),
            [
                format!("cwd={}", want.display()),
                "arg=machines".to_owned(),
                "arg=update".to_owned(),
                "arg=--flake".to_owned(),
                format!("arg={}", want.display()),
            ],
            "{reference}"
        );
    }
}

/// A (A): a local reference with no directory auspex can find is refused by both, not run
/// against whatever the working directory happens to be. `git+file://` with a host is one.
#[tokio::test]
async fn a_local_reference_without_a_directory_is_refused() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    let clan = t.script(dir.path(), "clan", "echo x > '@dir@/ran'");
    for reference in ["git+file://host/fleet", "git+file:///not/a/git/repository"] {
        let flake = FlakeRef::new(reference).unwrap();
        let mut p = common::resolve_params();
        p.nix = nix.as_os_str();
        let e = ClanLocator::new()
            .resolve(&p, &flake, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(e.step, ClanStep::Lock, "{reference}: {e}");
        assert!(e.reason.contains("no directory"), "{reference}: {e}");
        let (tx, _rx) = unbounded_channel();
        let e = run(
            &clan,
            &flake,
            &[],
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(e.step, ClanStep::Run, "{reference}: {e}");
        assert!(e.reason.contains("no directory"), "{reference}: {e}");
    }
    assert!(!dir.path().join("argv").exists(), "nix ran");
    assert!(!dir.path().join("ran").exists(), "clan ran");
}

/// H: a remote reference with a query is refused by the resolution, as `run` already does.
#[tokio::test]
async fn resolve_refuses_a_remote_reference_with_a_query_too() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nix = fake_nix(&t, dir.path(), &metadata_ok(), "echo /nix/store/x");
    let flake = FlakeRef::new("github:o/r?dir=x").unwrap();
    let mut p = common::resolve_params();
    p.nix = nix.as_os_str();
    let e = ClanLocator::new()
        .resolve(&p, &flake, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock, "{e}");
    assert!(e.reason.contains("query"), "{e}");
    assert!(!dir.path().join("argv").exists(), "nix ran");
}

/// H: an exit that lands together with the deadline is an exit, as with a cancel: the status is
/// known, and a kill and `TimedOut` would throw it away. The order is forced: the runtime's
/// only thread waits until the child is a zombie and the deadline has passed, so both are
/// ready when `run` next looks, and the exit watch (woken by events) has not noticed.
#[tokio::test]
async fn an_exit_that_lands_with_the_deadline_is_an_exit_not_a_timeout() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let flake = FlakeRef::new(fleet.path().to_str().unwrap()).unwrap();
    let clan = t.script(
        dir.path(),
        "clan",
        "echo $$ > '@dir@/pid'\necho done\nexit 5",
    );
    let (tx, _rx) = unbounded_channel();
    let params = RunParams {
        timeout: Duration::from_millis(300),
        ..RunParams::new()
    };
    let cancel = CancellationToken::new();
    let began = Instant::now();
    let fut = run(&clan, &flake, &[], &params, &cancel, tx);
    tokio::pin!(fut);
    std::future::poll_fn(|cx| {
        assert!(fut.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    wait_for_zombie(&dir.path().join("pid"));
    while began.elapsed() < Duration::from_millis(600) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let ran = fut.await.unwrap().unwrap();
    assert_eq!(
        ran.end,
        RunEnd::Exited {
            code: Some(5),
            signal: None
        }
    );
}

/// Blocks the calling thread until the process whose pid is in `pid_file` has exited and is
/// still unreaped (a zombie): the condition the forced-order tests need, observed, not timed.
fn wait_for_zombie(pid_file: &Path) {
    let began = Instant::now();
    loop {
        assert!(began.elapsed() < Duration::from_secs(20), "never exited");
        if let Ok(p) = fs::read_to_string(pid_file)
            && let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", p.trim()))
            && stat
                .rsplit(')')
                .next()
                .is_some_and(|r| r.trim_start().starts_with('Z'))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Blocks until the process whose pid is in `pid_file` has exited (a zombie or gone).
fn wait_for_exit(pid_file: &Path) {
    let began = Instant::now();
    loop {
        assert!(began.elapsed() < Duration::from_secs(20), "never exited");
        if let Ok(p) = fs::read_to_string(pid_file)
            && let Ok(pid) = p.trim().parse::<i32>()
            && !common::alive(pid)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Blocks until `file` exists.
fn wait_for_file(file: &Path) {
    let began = Instant::now();
    while !file.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "{} never appeared",
            file.display()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
