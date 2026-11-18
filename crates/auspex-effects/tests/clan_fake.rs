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
        r#"{{"nodes":{{"clan-core":{{"locked":{{"rev":"{REV}","type":"github"}}}},"nixpkgs":{{"locked":{{"rev":"abc","type":"github"}}}},"root":{{"inputs":{{{inputs}}}}}}},"root":"root","version":7}}"#
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
    let mut p = ResolveParams::new();
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
    let mut p = ResolveParams::new();
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
        |p| p.build_timeout = Duration::from_millis(500),
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
    let e = resolve(
        &nix,
        |p| p.lock_timeout = Duration::from_millis(300),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
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
    let canceller = {
        let cancel = cancel.clone();
        let pids_file = pids_file.clone();
        tokio::task::spawn_blocking(move || {
            wait_for_pids(&pids_file, 3);
            cancel.cancel();
        })
    };
    let (tx, mut rx) = unbounded_channel();
    let began = Instant::now();
    let r = run(&clan, &flake, &[], &RunParams::new(), &cancel, tx).await;
    canceller.await.unwrap();
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
    // What was printed before the cancel was delivered.
    assert_eq!(
        collect(&mut rx).await.first().map(|l| l.text.as_str()),
        Some("started")
    );
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
        timeout: Duration::from_millis(600),
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
