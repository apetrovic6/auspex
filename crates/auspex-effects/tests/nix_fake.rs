//! The evaluation and the machine list against fake `nix-eval-jobs` and `nix` scripts, for
//! what the real tools cannot show cheaply: results arriving over time, a crash midway,
//! cancellation killing the process group, a name that is not a bare attribute.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _, symlink};
use std::path::Path;
use std::time::{Duration, Instant};

use auspex_domain::eval::EvalOutcome;
use auspex_domain::fleet::MachineClass;
use auspex_effects::nix::{
    EVAL_EXPR, EvalItem, EvalMachine, EvalParams, FlakeRef, evaluate, inventory,
};
use auspex_effects::{CancellationToken, Cancelled};
use common::{cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const DRV: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-a.drv";
const OUT: &str = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-nixos-system-a";

fn ok_line(attr_json: &str, path_json: &str) -> String {
    format!(
        r#"{{"attr":{attr_json},"attrPath":[{path_json}],"drvPath":"{DRV}","outputs":{{"out":"{OUT}"}}}}"#
    )
}

fn machines(names: &[&str]) -> Vec<EvalMachine> {
    names
        .iter()
        .map(|n| EvalMachine {
            name: (*n).to_owned(),
            class: MachineClass::Nixos,
        })
        .collect()
}

fn params(program: &Path, timeout: Duration) -> EvalParams<'_> {
    let mut p = EvalParams::new();
    p.runtime_dir = Some(common::runtime_dir());
    p.program = program.as_os_str();
    p.timeout = timeout;
    p
}

async fn recv(rx: &mut UnboundedReceiver<EvalItem>) -> EvalItem {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an item within 5 s")
        .expect("the stream ended early")
}

async fn collect(rx: &mut UnboundedReceiver<EvalItem>) -> Vec<EvalItem> {
    let mut v = Vec::new();
    while let Some(i) = rx.recv().await {
        v.push(i);
    }
    v
}

#[tokio::test]
async fn results_arrive_as_each_machine_finishes() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf '%s\\n' '{}'\nwhile [ ! -e '@dir@/go' ]; do @sleep@ 0.02; done\nprintf '%s\\n' '{}'",
            ok_line(r#""a""#, r#""a""#),
            ok_line(r#""b""#, r#""b""#)
        ),
    );
    let flake = FlakeRef::new("path:/nonexistent").unwrap();
    let (tx, mut rx) = unbounded_channel();
    let p = params(&prog, Duration::from_secs(10));
    let ms = machines(&["a", "b"]);
    let cancel = CancellationToken::new();
    let run = evaluate(&flake, &ms, &p, &cancel, tx);
    let check = async {
        // The first result is here while the evaluator is still blocked on `go`.
        let first = recv(&mut rx).await;
        assert!(
            matches!(&first, EvalItem::Machine { machine, from_cache: false, .. } if machine == "a"),
            "{first:?}"
        );
        assert!(!dir.path().join("go").exists());
        fs::write(dir.path().join("go"), "").unwrap();
        let second = recv(&mut rx).await;
        assert!(
            matches!(&second, EvalItem::Machine { machine, .. } if machine == "b"),
            "{second:?}"
        );
        assert_eq!(recv(&mut rx).await, EvalItem::Finished { error: None });
    };
    let (r, ()) = tokio::join!(run, check);
    r.unwrap();
}

#[tokio::test]
async fn a_crash_midway_keeps_what_arrived_and_reports_the_crash() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf '%s\\n' '{}'\nprintf 'error: out of memory\\n' >&2\nkill -9 $$",
            ok_line(r#""a""#, r#""a""#)
        ),
    );
    let flake = FlakeRef::new("path:/x").unwrap();
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &flake,
        &machines(&["a", "b"]),
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(matches!(&items[0], EvalItem::Machine { machine, .. } if machine == "a"));
    assert_eq!(
        items[1],
        EvalItem::Finished {
            error: Some("nix-eval-jobs killed by signal 9: error: out of memory".to_owned())
        }
    );
}

#[tokio::test]
async fn a_non_zero_exit_carries_the_last_stderr_lines() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        "printf 'warning: a\\nerror: getting status of /x: No such file\\n' >&2\nexit 1",
    );
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    assert_eq!(
        collect(&mut rx).await,
        [EvalItem::Finished {
            error: Some(
                "nix-eval-jobs exited 1: warning: a\nerror: getting status of /x: No such file"
                    .to_owned()
            )
        }]
    );
}

#[tokio::test]
async fn cancelling_kills_the_evaluator_and_its_workers() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    // Two "workers", forked like nix-eval-jobs's, and the evaluator waiting on them.
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\n@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
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
    let start = Instant::now();
    let r = evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(60)),
        &cancel,
        tx,
    )
    .await;
    canceller.await.unwrap();
    let pids = wait_for_pids(&pids_file, 3);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(r, Err(Cancelled));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(left.is_empty(), "still running: {left:?}");
    assert_eq!(collect(&mut rx).await, [], "no Finished after a cancel");
}

#[tokio::test]
async fn dropping_the_receiver_cancels_too() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let pids_file = dir.path().join("pids");
    let (tx, rx) = unbounded_channel();
    let dropper = {
        let pids_file = pids_file.clone();
        tokio::task::spawn_blocking(move || {
            wait_for_pids(&pids_file, 2);
            drop(rx);
        })
    };
    let r = evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(60)),
        &CancellationToken::new(),
        tx,
    )
    .await;
    dropper.await.unwrap();
    let pids = wait_for_pids(&pids_file, 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(r, Err(Cancelled));
    assert!(left.is_empty(), "still running: {left:?}");
}

#[tokio::test]
async fn the_timeout_kills_the_group_and_keeps_earlier_results() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf '%s\\n' '{}'\n@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
            ok_line(r#""a""#, r#""a""#)
        ),
    );
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a", "b"]),
        &params(&prog, Duration::from_millis(700)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let pids = wait_for_pids(&dir.path().join("pids"), 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    let items = collect(&mut rx).await;
    assert!(matches!(&items[0], EvalItem::Machine { machine, .. } if machine == "a"));
    assert_eq!(
        items[1],
        EvalItem::Finished {
            error: Some("evaluation timed out after 700ms".to_owned())
        }
    );
    assert!(left.is_empty(), "still running: {left:?}");
}

/// nix-eval-jobs quotes an attribute name that is not a bare Nix identifier (`"1odd"`);
/// the result is still that machine's. And the name reaches the evaluator only as data.
#[tokio::test]
async fn a_name_that_is_not_a_bare_attribute() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let nasty = r#"x"; builtins.abort "pwned"#;
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf '%s\\0' \"$@\" > '@dir@/argv'\nfor a in \"$@\"; do if [ \"$p\" = machines ] && [ \"$pp\" = --arg-from-file ]; then cp_from=\"$a\"; fi; pp=\"$p\"; p=\"$a\"; done\nIFS= read -r m < \"$cp_from\" || true\nprintf '%s' \"$m\" > '@dir@/list'\nprintf '%s\\n' '{}'\nprintf '%s\\n' '{}'",
            ok_line(r#""\"1odd\"""#, r#""1odd""#),
            r#"{"attr":"\"x\\\"; builtins.abort \\\"pwned\"","attrPath":["x\"; builtins.abort \"pwned"],"error":"error: attribute missing"}"#
        ),
    );
    let flake = FlakeRef::new("path:/the flake").unwrap();
    let ms = [
        EvalMachine {
            name: "1odd".to_owned(),
            class: MachineClass::Nixos,
        },
        EvalMachine {
            name: nasty.to_owned(),
            class: MachineClass::Darwin,
        },
    ];
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &flake,
        &ms,
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    let EvalItem::Machine {
        machine,
        outcome: EvalOutcome::Success(s),
        ..
    } = &items[0]
    else {
        panic!("{items:?}")
    };
    assert_eq!(machine, "1odd");
    assert_eq!(s.attr, "1odd", "renamed to the machine");
    let EvalItem::Machine {
        machine,
        outcome: EvalOutcome::Error(_),
        ..
    } = &items[1]
    else {
        panic!("{items:?}")
    };
    assert_eq!(machine, nasty);
    assert_eq!(items[2], EvalItem::Finished { error: None });

    let argv = fs::read(dir.path().join("argv")).unwrap();
    let argv: Vec<String> = argv
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8(a.to_vec()).unwrap())
        .collect();
    let json = serde_json::json!([
        {"name": "1odd", "class": "nixos"},
        {"name": nasty, "class": "darwin"},
    ])
    .to_string();
    assert_eq!(
        argv,
        [
            "--impure",
            "--workers",
            "4",
            "--max-memory-size",
            "4096",
            "--argstr",
            "flake",
            "path:/the flake",
            "--arg-from-file",
            "machines",
            &argv[10],
            "--expr",
            EVAL_EXPR,
        ]
    );
    // The names arrive only in the list file, as JSON, and the file is gone afterwards.
    assert!(
        !Path::new(&argv[10]).exists(),
        "the list file was left behind"
    );
    let list: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("list")).unwrap()).unwrap();
    assert_eq!(
        list,
        serde_json::from_str::<serde_json::Value>(&json).unwrap()
    );
}

#[tokio::test]
async fn unusable_lines_are_notes_and_strangers_are_ignored() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf 'not json\\n'\nprintf '%s\\n' '{}'\nprintf '%s\\n' '{}'\nprintf '%s\\n' '{}'",
            ok_line(r#""stranger""#, r#""stranger""#),
            ok_line(r#""a""#, r#""a""#),
            ok_line(r#""a""#, r#""a""#),
        ),
    );
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    let notes = items
        .iter()
        .filter(|i| matches!(i, EvalItem::Note(_)))
        .count();
    let results = items
        .iter()
        .filter(|i| matches!(i, EvalItem::Machine { .. }))
        .count();
    assert_eq!((notes, results), (3, 1), "{items:?}");
    assert_eq!(items.last(), Some(&EvalItem::Finished { error: None }));
}

#[tokio::test]
async fn a_missing_evaluator_is_a_finished_error() {
    let (tx, mut rx) = unbounded_channel();
    let mut p = EvalParams::new();
    p.runtime_dir = Some(common::runtime_dir());
    p.program = std::ffi::OsStr::new("auspex-test-no-such-nix-eval-jobs");
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &p,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    assert_eq!(
        collect(&mut rx).await,
        [EvalItem::Finished {
            error: Some(
                "cannot evaluate: auspex-test-no-such-nix-eval-jobs not found on PATH".to_owned()
            )
        }]
    );
}

#[tokio::test]
async fn nothing_to_evaluate_runs_nothing() {
    let (tx, mut rx) = unbounded_channel();
    let mut p = EvalParams::new();
    p.runtime_dir = Some(common::runtime_dir());
    p.program = std::ffi::OsStr::new("/nonexistent");
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &[],
        &p,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    assert_eq!(collect(&mut rx).await, [EvalItem::Finished { error: None }]);
}

#[tokio::test]
async fn the_machine_list_from_a_fake_nix() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix",
        r#"printf '%s\0' "$@" > '@dir@/argv'
printf '%s' '{"terra":{"deploy":{"targetHost":"root@10.0.0.1"}},"genetor":{"machineClass":"darwin"}}'"#,
    );
    let flake = FlakeRef::new("path:/f").unwrap();
    let list = inventory(
        prog.as_os_str(),
        &flake,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    let names: Vec<_> = list.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["genetor", "terra"]);
    let argv = fs::read_to_string(dir.path().join("argv")).unwrap();
    assert_eq!(
        argv,
        "eval\0--json\0--impure\0--no-write-lock-file\0--\0path:/f#clanInternals.inventoryClass.inventory.machines\0"
    );

    // A failing nix: its last stderr line is the reason.
    let bad = t.script(
        dir.path(),
        "nix-bad",
        "printf 'error: flake has no attribute clanInternals\\n' >&2\nexit 1",
    );
    let r = inventory(
        bad.as_os_str(),
        &flake,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        r,
        Err(
            "listing machines: nix eval exited 1: error: flake has no attribute clanInternals"
                .to_owned()
        )
    );
}

#[tokio::test]
async fn the_machine_list_times_out_and_cancels() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix",
        "echo $$ >> '@dir@/pids'\nexec @sleep@ 30",
    );
    let flake = FlakeRef::new("path:/f").unwrap();
    let r = inventory(
        prog.as_os_str(),
        &flake,
        Duration::from_millis(300),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(r, Err("listing machines timed out after 300ms".to_owned()));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let r = inventory(prog.as_os_str(), &flake, Duration::from_secs(30), &cancel).await;
    assert_eq!(r, Err(Cancelled));
    let pids = wait_for_pids(&dir.path().join("pids"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "still running: {left:?}");
}

// ---------------------------------------------------------------------------------------
// Review round 1: F3 (leftover children), F12 (one bad name), notes cap, reaping
// ---------------------------------------------------------------------------------------

/// F3: an evaluator that crashes on its own (the OOM killer, a segfault) must not leave
/// its workers running.
#[tokio::test]
async fn a_crashed_evaluator_leaves_no_workers() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\nkill -SEGV $$",
    );
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let pids = wait_for_pids(&dir.path().join("pids"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    let items = collect(&mut rx).await;
    assert!(
        matches!(items.last(), Some(EvalItem::Finished { error: Some(e) }) if e.contains("signal 11")),
        "{items:?}"
    );
    assert!(
        left.is_empty(),
        "the worker outlived its evaluator: {left:?}"
    );
}

/// F3, for `nix` (through the same runner as `git`): a background child left by a `nix`
/// that exited 0 is killed too.
#[tokio::test]
async fn a_finished_nix_leaves_no_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix",
        "@sleep@ 30 > /dev/null 2>&1 &\necho $! >> '@dir@/pids'\nprintf '{}'",
    );
    let r = inventory(
        prog.as_os_str(),
        &FlakeRef::new("path:/f").unwrap(),
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let pids = wait_for_pids(&dir.path().join("pids"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(r, Ok(vec![]));
    assert!(left.is_empty(), "nix's child outlived it: {left:?}");
}

/// F12: one machine whose name nix cannot take (a NUL) gets its own error; the others are
/// evaluated.
#[tokio::test]
async fn one_unusable_name_does_not_fail_the_others() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!("printf '%s\\n' '{}'", ok_line(r#""a""#, r#""a""#)),
    );
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a", "b\0c"]),
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    let mut by: Vec<(String, bool)> = items
        .iter()
        .filter_map(|i| match i {
            EvalItem::Machine {
                machine, outcome, ..
            } => Some((machine.clone(), matches!(outcome, EvalOutcome::Success(_)))),
            _ => None,
        })
        .collect();
    by.sort();
    assert_eq!(
        by,
        [("a".to_owned(), true), ("b\0c".to_owned(), false)],
        "{items:?}"
    );
    assert_eq!(items.last(), Some(&EvalItem::Finished { error: None }));
}

/// The fake's prelude that finds the `--arg-from-file machines <path>` argument.
const FIND_LIST: &str = "for a in \"$@\"; do if [ \"$p\" = machines ] && [ \"$pp\" = --arg-from-file ]; then list=\"$a\"; fi; pp=\"$p\"; p=\"$a\"; done\n";

/// The machine list goes to the evaluator in a file, so its size is not bounded by
/// `MAX_ARG_STRLEN` (128 KiB per argument): 3000 machines of 63 characters, all of them
/// read back by the fake.
#[tokio::test]
async fn a_large_fleet_fits() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "{FIND_LIST}IFS= read -r m < \"$list\" || true\nprintf '%s' \"$m\" > '@dir@/list'"
        ),
    );
    let names: Vec<String> = (0..3000).map(|i| format!("{i:063}")).collect();
    let ms: Vec<EvalMachine> = names
        .iter()
        .map(|n| EvalMachine {
            name: n.clone(),
            class: MachineClass::Nixos,
        })
        .collect();
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &ms,
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    assert_eq!(collect(&mut rx).await, [EvalItem::Finished { error: None }]);
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(dir.path().join("list")).unwrap()).unwrap();
    let got: Vec<&str> = list.iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert_eq!(got, names);
}

/// Review mutations R18 and R19: an empty name and one over 1 KiB get their own error.
#[tokio::test]
async fn empty_and_oversized_names_get_their_own_error() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(dir.path(), "nix-eval-jobs", "exit 0");
    let long = "x".repeat(auspex_effects::nix::MAX_NAME_BYTES + 1);
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["", &long]),
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    let errors: Vec<&str> = items
        .iter()
        .filter_map(|i| match i {
            EvalItem::Machine {
                machine,
                outcome: EvalOutcome::Error(_),
                ..
            } => Some(machine.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors, ["", long.as_str()], "{items:?}");
}

/// Review findings N7 and mutation R20: the list file is 0600, in the private runtime
/// directory (0700), and a stale one left by a process that is gone is removed.
#[tokio::test]
async fn the_list_file_is_private_and_stale_ones_are_removed() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let rt = dir.path().join("rt");
    fs::DirBuilder::new().mode(0o700).create(&rt).unwrap();
    // pid_max is at most 2^22 on Linux, so this pid cannot exist.
    let stale = rt.join("machines-4194305-0.json");
    fs::write(&stale, "[]").unwrap();
    // One of ours (a live pid) must be kept.
    let live = rt.join(format!("machines-{}-999999.json", std::process::id()));
    fs::write(&live, "[]").unwrap();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "{FIND_LIST}printf '%s' \"$list\" > '@dir@/path'\nwhile [ ! -e '@dir@/go' ]; do @sleep@ 0.02; done"
        ),
    );
    let mut p = params(&prog, Duration::from_secs(10));
    p.runtime_dir = Some(rt.clone());
    let (tx, _rx) = unbounded_channel();
    let (flake, ms, cancel) = (
        FlakeRef::new("path:/x").unwrap(),
        machines(&["a"]),
        CancellationToken::new(),
    );
    let run = evaluate(&flake, &ms, &p, &cancel, tx);
    let check = async {
        let path_file = dir.path().join("path");
        let start = std::time::Instant::now();
        while !path_file.exists() {
            assert!(start.elapsed() < Duration::from_secs(5));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let list = std::path::PathBuf::from(fs::read_to_string(&path_file).unwrap());
        let mode = fs::metadata(&list).unwrap().permissions().mode() & 0o777;
        fs::write(dir.path().join("go"), "").unwrap();
        (list, mode)
    };
    let (r, (list, mode)) = tokio::join!(run, check);
    r.unwrap();
    assert_eq!(list.parent(), Some(rt.as_path()), "{list:?}");
    assert_eq!(mode, 0o600);
    assert!(!stale.exists(), "the stale list file was kept");
    assert!(live.exists(), "a live process's list file was removed");
    assert!(!list.exists(), "this call's list file was left behind");
}

#[tokio::test]
async fn notes_about_garbage_are_capped() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        "i=0\nwhile [ $i -lt 100 ]; do printf 'junk %s\\n' $i; i=$((i+1)); done",
    );
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let notes = collect(&mut rx)
        .await
        .iter()
        .filter(|i| matches!(i, EvalItem::Note(_)))
        .count();
    assert_eq!(notes, auspex_effects::nix::MAX_LINE_NOTES);
}

/// A cancelled call returns only after reaping its child: at that moment the leader's pid
/// is gone entirely, not a zombie waiting to be collected.
#[tokio::test]
async fn a_cancelled_evaluation_has_reaped_its_leader_on_return() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        "echo $$ >> '@dir@/pids'\nexec @sleep@ 30",
    );
    let cancel = CancellationToken::new();
    let pids_file = dir.path().join("pids");
    let canceller = {
        let cancel = cancel.clone();
        let pids_file = pids_file.clone();
        tokio::task::spawn_blocking(move || {
            wait_for_pids(&pids_file, 1);
            cancel.cancel();
        })
    };
    let (tx, _rx) = unbounded_channel();
    let r = evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(60)),
        &cancel,
        tx,
    )
    .await;
    let leader = wait_for_pids(&pids_file, 1)[0];
    let present = common::pid_exists(leader);
    canceller.await.unwrap();
    cleanup(&[leader]);
    assert_eq!(r, Err(Cancelled));
    assert!(
        !present,
        "the leader {leader} was not reaped before returning"
    );
}

/// Review mutation R21: the list file is created exclusively, so a symlink planted at its
/// name is never followed. Run in a child process, whose list-file counter starts at 0, so
/// the names are known: `machines-<pid>-0.json` and on.
#[test]
fn a_planted_symlink_at_the_list_name_is_not_followed() {
    if std::env::var_os("AUSPEX_TEST_LIST_INNER").is_some() {
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "planted_list_inner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AUSPEX_TEST_LIST_INNER", "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
}

#[tokio::test]
async fn planted_list_inner() {
    if std::env::var_os("AUSPEX_TEST_LIST_INNER").is_none() {
        return;
    }
    let dir = tempdir();
    let rt = dir.path().join("rt");
    fs::DirBuilder::new().mode(0o700).create(&rt).unwrap();
    let target = dir.path().join("victim");
    fs::write(&target, "untouched").unwrap();
    for n in 0..16 {
        std::os::unix::fs::symlink(
            &target,
            rt.join(format!("machines-{}-{n}.json", std::process::id())),
        )
        .unwrap();
    }
    let mut p = EvalParams::new();
    p.runtime_dir = Some(rt);
    p.program = std::ffi::OsStr::new("/nonexistent");
    let (tx, _rx) = unbounded_channel();
    let _ = evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &p,
        &CancellationToken::new(),
        tx,
    )
    .await;
    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
}

#[test]
fn test_runtime_directories_are_removed_when_their_thread_exits() {
    let rt = std::thread::spawn(|| {
        let rt = common::runtime_dir();
        fs::create_dir(&rt).unwrap();
        assert!(rt.is_dir());
        rt
    })
    .join()
    .unwrap();
    assert!(!rt.parent().unwrap().exists(), "{rt:?} was left behind");
}

#[tokio::test]
async fn evaluator_runtime_directories_are_not_limited_by_socket_paths() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let rt = dir.path().join("r".repeat(80));
    let ssh_error = auspex_effects::ssh::ControlDir::prepare(&rt).unwrap_err();
    assert!(ssh_error.0.contains("unix socket path"), "{ssh_error}");
    assert!(
        !rt.exists(),
        "ssh must refuse before creating the directory"
    );
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "{FIND_LIST}IFS= read -r m < \"$list\" || true\nprintf '%s\\n' '{}'",
            ok_line(r#""a""#, r#""a""#)
        ),
    );
    let mut p = params(&prog, Duration::from_secs(10));
    p.runtime_dir = Some(rt.clone());
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &p,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    assert!(
        matches!(&items[..], [EvalItem::Machine { machine, .. }, EvalItem::Finished { error: None }] if machine == "a"),
        "{items:?}"
    );
    let meta = fs::symlink_metadata(&rt).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o700);
    assert_eq!(meta.uid(), rustix::process::getuid().as_raw());
    assert_eq!(fs::read_dir(&rt).unwrap().count(), 0, "list file leaked");
}

#[tokio::test]
async fn unsafe_evaluator_runtime_directories_are_refused_without_changing_them() {
    let dir = tempdir();
    let public = dir.path().join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "untouched").unwrap();
    let private = dir.path().join("private");
    fs::DirBuilder::new().mode(0o700).create(&private).unwrap();
    let link = dir.path().join("link");
    symlink(&private, &link).unwrap();

    for (rt, reason) in [
        (&public, "has mode 0755, expected 0700"),
        (&file, "is not a directory"),
        (&link, "is a symlink"),
    ] {
        let mut p = EvalParams::new();
        p.runtime_dir = Some(rt.clone());
        p.program = std::ffi::OsStr::new("/nonexistent");
        let (tx, mut rx) = unbounded_channel();
        evaluate(
            &FlakeRef::new("path:/x").unwrap(),
            &machines(&["a"]),
            &p,
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
        let items = collect(&mut rx).await;
        let [EvalItem::Finished { error: Some(error) }] = &items[..] else {
            panic!("expected a runtime-directory error, got {items:?}");
        };
        assert!(error.contains("runtime directory"), "{error}");
        assert!(error.contains(reason), "{error}");
        assert!(!error.contains("ssh"), "{error}");
        assert!(!error.contains("control directory"), "{error}");
    }
    assert_eq!(
        fs::metadata(&public).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "untouched");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_dir(&private).unwrap().count(), 0);
}

#[tokio::test]
async fn evaluator_runtime_directories_owned_by_another_user_are_refused() {
    let me = rustix::process::getuid().as_raw();
    let Some(theirs) = ["/", "/proc", "/dev", "/nix/store"]
        .iter()
        .map(Path::new)
        .find(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && m.uid() != me))
    else {
        eprintln!("SKIPPED: no directory owned by another user to test with (running as root?)");
        return;
    };
    let mut p = EvalParams::new();
    p.runtime_dir = Some(theirs.to_path_buf());
    p.program = std::ffi::OsStr::new("/nonexistent");
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a"]),
        &p,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    let [EvalItem::Finished { error: Some(error) }] = &items[..] else {
        panic!("expected an ownership error, got {items:?}");
    };
    assert!(error.contains("runtime directory"), "{error}");
    assert!(error.contains("not by this user"), "{error}");
}

// ---------------------------------------------------------------------------------------
// PR #9 fix round 1: output accepted only when read to its end
// ---------------------------------------------------------------------------------------

fn kill_holder(dir: &Path) {
    if let Ok(pids) = fs::read_to_string(dir.join("holder")) {
        let pids: Vec<i32> = pids.lines().filter_map(|l| l.trim().parse().ok()).collect();
        cleanup(&pids);
    }
}

/// `nix eval` (the machine list) prints a valid list and exits 0, but its stdout is still
/// held open by a process outside its group: that is not the whole output. The holder
/// writes its pid once it is in its own session, and only then does the fake exit, so the
/// group kill at the exit cannot reach it.
#[tokio::test]
async fn a_machine_list_not_read_to_its_end_is_refused() {
    let Some(t) = tools(&["setsid", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix",
        "printf '{}'\n@setsid@ @sh@ -c 'echo $$ > \"$1\"; exec @sleep@ 30' sh '@dir@/holder' &\nwhile [ ! -s '@dir@/holder' ]; do :; done",
    );
    let r = inventory(
        prog.as_os_str(),
        &FlakeRef::new("path:/f").unwrap(),
        Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    kill_holder(dir.path());
    let e = r.expect_err("a list not read to its end was accepted");
    assert!(e.contains("not read to the end"), "{e}");
}

/// nix-eval-jobs prints one result and exits 0 with its stdout still held open: the result
/// that arrived is delivered, and the run ends with an error, not as a complete one.
#[tokio::test]
async fn evaluator_output_not_read_to_its_end_is_an_error() {
    let Some(t) = tools(&["setsid", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf '%s\\n' '{}'\n@setsid@ @sh@ -c 'echo $$ > \"$1\"; exec @sleep@ 30' sh '@dir@/holder' &\nwhile [ ! -s '@dir@/holder' ]; do :; done",
            ok_line(r#""a""#, r#""a""#)
        ),
    );
    let (tx, mut rx) = unbounded_channel();
    evaluate(
        &FlakeRef::new("path:/x").unwrap(),
        &machines(&["a", "b"]),
        &params(&prog, Duration::from_secs(1)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    kill_holder(dir.path());
    let items = collect(&mut rx).await;
    assert!(
        items
            .iter()
            .any(|i| matches!(i, EvalItem::Machine { machine, .. } if machine == "a")),
        "{items:?}"
    );
    assert!(
        matches!(items.last(), Some(EvalItem::Finished { error: Some(e) }) if e.contains("not read to the end")),
        "{items:?}"
    );
}

/// The repository of the N6 test: `modules/tracked.nix` is in the index; `modules/new.nix`,
/// `modules/other.nix` and `outside-the-store.nix` are not, and `secret.nix` is outside it.
fn untracked_repository(dir: &Path) -> std::path::PathBuf {
    let repo = fs::canonicalize(dir).unwrap().join("repo");
    fs::create_dir_all(repo.join("modules")).unwrap();
    fs::write(repo.join("modules/tracked.nix"), "{}").unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "modules/tracked.nix"]);
    // Not added: nix cannot see these.
    fs::write(repo.join("modules/new.nix"), "{}").unwrap();
    fs::write(repo.join("modules/other.nix"), "{}").unwrap();
    fs::write(repo.join("outside-the-store.nix"), "{}").unwrap();
    fs::write(dir.join("secret.nix"), "{}").unwrap();
    repo
}

/// Review of PR #8, N6: a configuration that imports a new, untracked module fails with nix's
/// "path '/nix/store/…-source/new.nix' does not exist", which says nothing about git. When
/// the named file exists in the worktree, the operator gets one note saying why nix cannot
/// see it. Nothing is said for a file that is not there, for a path that leaves the
/// repository, or for a path that is not below a store path.
#[tokio::test]
async fn an_untracked_file_nix_cannot_find_is_explained_once() {
    let Some(t) = tools(&["git"]) else { return };
    let dir = tempdir();
    let repo = untracked_repository(dir.path());
    let store = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source";
    let err = |name: &str, text: String| {
        format!(
            r#"{{"attr":"{name}","attrPath":["{name}"],"error":{}}}"#,
            serde_json::Value::String(text)
        )
    };
    let lines = [
        // The same missing module, from two machines: one note.
        err(
            "a",
            format!("error: path '{store}/modules/new.nix' does not exist"),
        ),
        err(
            "b",
            format!("error: getting status of '{store}/modules/new.nix': No such file or directory"),
        ),
        // Not in the worktree, an escape attempt, and no store path before `-source/`.
        err(
            "c",
            format!("error: path '{store}/modules/gone.nix' does not exist"),
        ),
        err(
            "d",
            format!("error: path '{store}/../secret.nix' does not exist"),
        ),
        err(
            "e",
            "error: path '/home/u/0123456789abcdfghijklmnpqrsvwxyz-source/outside-the-store.nix' does not exist".to_owned(),
        ),
        err(
            "f",
            format!("error: path '{store}//modules/new.nix' does not exist"),
        ),
        // A tracked file that nix reports on is not an untracked one: a missing-file line
        // naming it, and the module system's "In `…'" line (which says nothing is missing)
        // naming a tracked file and an untracked one.
        err(
            "g",
            format!("error: path '{store}/modules/tracked.nix' does not exist"),
        ),
        err(
            "h",
            format!("error: attribute 'x' missing\n- In `{store}/modules/tracked.nix': true"),
        ),
        err(
            "i",
            format!(
                "error: The option `x' has conflicting definitions\n- In `{store}/modules/other.nix': true"
            ),
        ),
    ];
    let script = lines
        .iter()
        .map(|l| format!("printf '%s\\n' '{}'", l.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join("\n");
    let prog = t.script(dir.path(), "nix-eval-jobs", &script);
    let flake = FlakeRef::new(repo.to_str().unwrap()).unwrap();
    assert!(flake.is_git(), "{flake:?}");
    let (tx, mut rx) = unbounded_channel();
    let ms = machines(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
    evaluate(
        &flake,
        &ms,
        &params(&prog, Duration::from_secs(10)),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    let notes: Vec<&str> = items
        .iter()
        .filter_map(|i| match i {
            EvalItem::Note(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    let note = notes[0];
    for part in [
        "modules/new.nix",
        "not tracked by git",
        "git+file",
        "git add",
    ] {
        assert!(note.contains(part), "{part:?} missing from: {note}");
    }
    // Every machine still has its own error, untouched.
    let errors = items
        .iter()
        .filter(|i| {
            matches!(
                i,
                EvalItem::Machine {
                    outcome: EvalOutcome::Error(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(errors, 9);
}

/// Review of PR #37, re-review A: the untracked-file check asks the git the cache parameters
/// name (`AUSPEX_GIT` in the shell), not whatever `git` is on `PATH`.
#[tokio::test]
async fn the_untracked_check_uses_the_configured_git() {
    let Some(t) = tools(&["git"]) else { return };
    let dir = tempdir();
    let repo = untracked_repository(dir.path());
    let store = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source";
    let line = format!(
        r#"{{"attr":"a","attrPath":["a"],"error":"error: path '{store}/modules/new.nix' does not exist"}}"#
    );
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!("printf '%s\\n' '{line}'"),
    );
    let git = t.script(
        dir.path(),
        "git-recorder",
        "echo \"$*\" >> '@dir@/git-calls'\nulimit -v >> '@dir@/git-limits'\nexec @git@ \"$@\"",
    );
    let flake = FlakeRef::new(repo.to_str().unwrap()).unwrap();
    let (tx, mut rx) = unbounded_channel();
    let mut cache = auspex_effects::cache::CacheParams {
        cache_dir: None,
        flake_dir: &repo,
        git: auspex_effects::cache::GitParams::new(),
    };
    cache.git.program = git.as_os_str();
    auspex_effects::cache::evaluate_cached(
        &flake,
        &machines(&["a"]),
        &params(&prog, Duration::from_secs(10)),
        &cache,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    assert!(
        items
            .iter()
            .any(|i| matches!(i, EvalItem::Note(n) if n.contains("not tracked by git"))),
        "{items:?}"
    );
    // Where a launcher exists (Linux) the check runs under the 512 MiB limit like every git.
    if cfg!(target_os = "linux") {
        let limits = fs::read_to_string(dir.path().join("git-limits")).unwrap();
        assert!(
            limits.lines().all(|l| l == "524288"),
            "the check ran without its address-space limit: {limits}"
        );
    }
    let calls = fs::read_to_string(dir.path().join("git-calls")).unwrap();
    assert!(
        calls.lines().any(|l| l.contains(" ls-files ")),
        "the configured git was not asked: {calls}"
    );
}
