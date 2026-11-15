//! The evaluation and the machine list against fake `nix-eval-jobs` and `nix` scripts, for
//! what the real tools cannot show cheaply: results arriving over time, a crash midway,
//! cancellation killing the process group, a name that is not a bare attribute.

#![cfg(unix)]

mod common;

use std::fs;
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

/// The machine list goes to the evaluator on stdin, so its size is not bounded by
/// `MAX_ARG_STRLEN` (128 KiB per argument): 3000 machines of 63 characters.
#[tokio::test]
async fn a_large_fleet_fits() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let prog = t.script(dir.path(), "nix-eval-jobs", "exit 0");
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
