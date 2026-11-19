//! `u` and `b` against a fake `clan` and a fake `nix`: the exact argv each runs, the refusals
//! that must happen before anything is spawned, the streamed output, and cancellation killing
//! the whole process group. Nothing here runs the real `clan` or the real `nix`.
//!
//! The fakes write what they were given to files in a private temporary directory, so a test
//! can say both "this exact argv" and "nothing ran at all" (no `ran` marker).

#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use auspex_domain::fleet::MachineClass;
use auspex_effects::act::{UPDATE_HEAD, build, build_argv, update, update_args, update_argv};
use auspex_effects::clan::{ClanLine, RunEnd, RunParams, Stream};
use auspex_effects::nix::FlakeRef;
use auspex_effects::{CancellationToken, Cancelled};
use common::{Tools, cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const LOCAL: &str = "x86_64-linux";

/// A fake program that records each argument on a line of `argv` and that it ran.
fn recorder(t: &Tools, dir: &Path, name: &str, then: &str) -> PathBuf {
    t.script(
        dir,
        name,
        &format!(
            "for a in \"$@\"; do printf '%s\\n' \"$a\" >> '@dir@/argv'; done\necho ran >> '@dir@/ran'\n{then}"
        ),
    )
}

fn recorded(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("argv"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn ran(dir: &Path) -> bool {
    dir.join("ran").exists()
}

fn fleet() -> (tempfile::TempDir, FlakeRef) {
    let dir = tempdir();
    let flake = FlakeRef::new(dir.path().to_str().unwrap()).unwrap();
    (dir, flake)
}

async fn collect(rx: &mut UnboundedReceiver<ClanLine>) -> Vec<ClanLine> {
    let mut out = Vec::new();
    while let Ok(Some(l)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        out.push(l);
    }
    out
}

fn of(lines: &[ClanLine], stream: Stream) -> Vec<&str> {
    lines
        .iter()
        .filter(|l| l.stream == stream)
        .map(|l| l.text.as_str())
        .collect()
}

// ---------------------------------------------------------------------------------------
// u: the argv
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn u_runs_exactly_clan_machines_update_strict_flake_dashdash_machine() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let (fleet_dir, flake) = fleet();
    let clan = recorder(&t, dir.path(), "clan", "");
    let (tx, mut rx) = unbounded_channel();
    let end = update(
        &clan,
        &flake,
        "luna",
        &RunParams::new(),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(end.end.success());
    drop(collect(&mut rx).await);

    let cwd = fs::canonicalize(fleet_dir.path()).unwrap();
    let expected = [
        "machines",
        "update",
        "--host-key-check",
        "strict",
        "--flake",
        cwd.to_str().unwrap(),
        "--",
        "luna",
    ];
    assert_eq!(recorded(dir.path()), expected, "what clan was given");
    // The pure builder says the same, so a caller that only builds the argv sees what runs.
    let built: Vec<String> = update_argv(&flake, "luna")
        .unwrap()
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(built, expected);
    assert_eq!(
        UPDATE_HEAD,
        ["machines", "update", "--host-key-check", "strict"]
    );
}

/// The one safety property that matters most: a bare `clan machines update` updates every
/// machine, so the argv has exactly one machine, and it comes after the `--`.
#[tokio::test]
async fn the_argv_has_one_machine_after_the_dashdash_and_the_flake_before_it() {
    let (_dir, flake) = fleet();
    let argv: Vec<String> = update_argv(&flake, "terra")
        .unwrap()
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let dashes: Vec<usize> = (0..argv.len()).filter(|&i| argv[i] == "--").collect();
    assert_eq!(dashes.len(), 1, "exactly one `--`: {argv:?}");
    let dd = dashes[0];
    assert_eq!(&argv[dd + 1..], ["terra"], "exactly one operand, after it");
    let flake_at = argv.iter().position(|a| a == "--flake").unwrap();
    assert!(
        flake_at + 1 < dd,
        "--flake <dir> precedes the `--`: {argv:?}"
    );
    assert_eq!(argv.iter().filter(|a| *a == "terra").count(), 1);
}

#[tokio::test]
async fn u_refuses_an_empty_unknown_or_malformed_name_before_anything_runs() {
    let Some(t) = tools(&[]) else { return };
    let long = "a".repeat(64);
    let names: Vec<&str> = vec![
        "",
        " ",
        "-h",
        "--help",
        "--flake",
        "-luna",
        "luna terra",
        "luna\nterra",
        "luna;reboot",
        "a/b",
        "..",
        ".",
        "lu.na",
        "luna@host",
        "luna\u{202e}",
        "лuna",
        &long,
    ];
    for name in names {
        let dir = tempdir();
        let (_fleet_dir, flake) = fleet();
        let clan = recorder(&t, dir.path(), "clan", "");
        assert!(
            update_args(name).is_err(),
            "{name:?} must not build an argv"
        );
        assert!(update_argv(&flake, name).is_err(), "{name:?}");
        let (tx, _rx) = unbounded_channel();
        let answer = update(
            &clan,
            &flake,
            name,
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
        let err = answer.expect_err(&format!("{name:?} must be refused"));
        assert!(err.reason.starts_with("refused:"), "{name:?}: {err}");
        assert!(
            !ran(dir.path()),
            "{name:?}: clan must never have been started"
        );
    }
}

#[tokio::test]
async fn the_empty_name_is_refused_with_the_reason_it_matters() {
    let err = update_args("").unwrap_err();
    assert!(err.0.contains("every machine"), "{err}");
}

// ---------------------------------------------------------------------------------------
// u: the stream
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_stream_keeps_each_streams_order_and_is_sanitised() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let (_fleet_dir, flake) = fleet();
    let clan = t.script(
        dir.path(),
        "clan",
        "printf 'out1\\n'\nprintf 'err1\\n' >&2\nprintf '\\033[31mred\\033[0m\\007\\n'\nprintf '%s\\n' 'this line is much longer than the cap'\nprintf 'err2\\n' >&2\nprintf 'out5\\n'\nexit 1",
    );
    let params = RunParams {
        timeout: Duration::from_secs(30),
        line_cap: 16,
        max_lines: 1000,
    };
    let (tx, mut rx) = unbounded_channel();
    let ran = update(
        &clan,
        &flake,
        "luna",
        &params,
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
            code: Some(1),
            signal: None
        }
    );
    assert_eq!(
        of(&lines, Stream::Stdout),
        [
            "out1",
            "red",
            "[a line of more than 16 bytes was left out]",
            "out5"
        ],
        "stdout in order, escapes and BEL removed, the long line replaced by a note"
    );
    assert_eq!(
        of(&lines, Stream::Stderr),
        ["err1", "err2"],
        "stderr in order"
    );
    assert_eq!((ran.lines_dropped, ran.long_lines), (0, 1));
}

#[tokio::test]
async fn output_past_the_line_cap_is_counted_not_sent_and_the_process_is_not_stopped() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let (_fleet_dir, flake) = fleet();
    let clan = t.script(
        dir.path(),
        "clan",
        "for i in 1 2 3 4 5 6 7 8; do echo \"line $i\"; done\necho finished >> '@dir@/done'\nexit 0",
    );
    let params = RunParams {
        timeout: Duration::from_secs(30),
        line_cap: 1024,
        max_lines: 3,
    };
    let (tx, mut rx) = unbounded_channel();
    let ran = update(
        &clan,
        &flake,
        "luna",
        &params,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    let lines = collect(&mut rx).await;
    assert!(ran.end.success(), "a chatty deploy is not killed for it");
    assert!(dir.path().join("done").exists(), "it ran to its end");
    assert_eq!(of(&lines, Stream::Stdout), ["line 1", "line 2", "line 3"]);
    assert_eq!(ran.lines_dropped, 5);
    assert!(
        of(&lines, Stream::Stderr)
            .iter()
            .any(|l| l.contains("3 lines shown")),
        "one note says output was cut: {lines:?}"
    );
}

// ---------------------------------------------------------------------------------------
// u: cancellation
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_an_update_kills_the_whole_group_and_reaps_it() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let (_fleet_dir, flake) = fleet();
    // A deploy is clan, which starts ssh and nixos-rebuild: stand-in a child that outlives it.
    let clan = t.script(
        dir.path(),
        "clan",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\necho started\nwait",
    );
    let cancel = CancellationToken::new();
    let (tx, mut rx) = unbounded_channel();
    let params = RunParams::new();
    let flake2 = flake.clone();
    let c = cancel.clone();
    let task = tokio::spawn(async move { update(&clan, &flake2, "luna", &params, &c, tx).await });
    let pids = tokio::task::spawn_blocking({
        let p = dir.path().join("pids");
        move || wait_for_pids(&p, 2)
    })
    .await
    .unwrap();
    let first = rx.recv().await.expect("its first line arrived live");
    assert_eq!(first.text, "started");
    cancel.cancel();
    let answer = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("returns once cancelled")
        .unwrap();
    assert_eq!(answer.unwrap_err(), Cancelled);
    let left = still_alive_after_wait(&pids);
    cleanup(&left);
    assert!(left.is_empty(), "left behind: {left:?}");
}

// ---------------------------------------------------------------------------------------
// b
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn b_runs_exactly_nix_build_no_link_print_out_paths_installable() {
    let Some(t) = tools(&[]) else { return };
    let (_fleet_dir, flake) = fleet();
    for (machine, class, attr) in [
        (
            "luna",
            MachineClass::Nixos,
            "nixosConfigurations.luna.config.system.build.toplevel",
        ),
        (
            "genetor",
            MachineClass::Darwin,
            "darwinConfigurations.genetor.system",
        ),
    ] {
        let dir = tempdir();
        let nix = recorder(
            &t,
            dir.path(),
            "nix",
            "echo /nix/store/0123456789abcdfghijklmnpqrsvwxyz-built",
        );
        let (tx, mut rx) = unbounded_channel();
        let end = build(
            nix.as_os_str(),
            &flake,
            machine,
            class,
            Some(LOCAL),
            LOCAL,
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(end.end.success());
        let lines = collect(&mut rx).await;
        assert_eq!(
            of(&lines, Stream::Stdout),
            ["/nix/store/0123456789abcdfghijklmnpqrsvwxyz-built"],
            "nix's output is streamed"
        );
        let expected = vec![
            "build".to_owned(),
            "--no-link".to_owned(),
            "--print-out-paths".to_owned(),
            "--no-write-lock-file".to_owned(),
            format!("{}#{attr}", flake.as_str()),
        ];
        assert_eq!(recorded(dir.path()), expected, "{machine}");
        let built: Vec<String> = build_argv(&flake, machine, class)
            .unwrap()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(built, expected);
    }
}

#[tokio::test]
async fn b_refuses_another_system_or_an_unknown_one_without_running_nix() {
    let Some(t) = tools(&[]) else { return };
    let (_fleet_dir, flake) = fleet();
    for (system, expect) in [
        (Some("aarch64-darwin"), "another system's derivations"),
        (Some("aarch64-linux"), "another system's derivations"),
        (Some(""), "another system's derivations"),
        (None, "not known"),
    ] {
        let dir = tempdir();
        let nix = recorder(&t, dir.path(), "nix", "");
        let (tx, _rx) = unbounded_channel();
        let answer = build(
            nix.as_os_str(),
            &flake,
            "luna",
            MachineClass::Nixos,
            system,
            LOCAL,
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
        let err = answer.expect_err(&format!("{system:?} must be refused"));
        assert!(err.reason.contains(expect), "{system:?}: {err}");
        assert!(
            !ran(dir.path()),
            "{system:?}: nix must never have been started"
        );
    }
}

#[tokio::test]
async fn b_refuses_a_malformed_name_without_running_nix() {
    let Some(t) = tools(&[]) else { return };
    let (_fleet_dir, flake) = fleet();
    for name in ["", "-h", "a b", "x#y", "../etc"] {
        let dir = tempdir();
        let nix = recorder(&t, dir.path(), "nix", "");
        let (tx, _rx) = unbounded_channel();
        let answer = build(
            nix.as_os_str(),
            &flake,
            name,
            MachineClass::Nixos,
            Some(LOCAL),
            LOCAL,
            &RunParams::new(),
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
        assert!(answer.is_err(), "{name:?}");
        assert!(!ran(dir.path()), "{name:?}");
    }
}

#[tokio::test]
async fn cancelling_a_build_kills_the_whole_group_and_reaps_it() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let (_fleet_dir, flake) = fleet();
    let nix = t.script(
        dir.path(),
        "nix",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let cancel = CancellationToken::new();
    let (tx, _rx) = unbounded_channel();
    let c = cancel.clone();
    let task = tokio::spawn(async move {
        build(
            nix.as_os_str(),
            &flake,
            "luna",
            MachineClass::Nixos,
            Some(LOCAL),
            LOCAL,
            &RunParams::new(),
            &c,
            tx,
        )
        .await
    });
    let pids = tokio::task::spawn_blocking({
        let p = dir.path().join("pids");
        move || wait_for_pids(&p, 2)
    })
    .await
    .unwrap();
    cancel.cancel();
    let answer = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("returns once cancelled")
        .unwrap();
    assert_eq!(answer.unwrap_err(), Cancelled);
    let left = still_alive_after_wait(&pids);
    cleanup(&left);
    assert!(left.is_empty(), "left behind: {left:?}");
}
