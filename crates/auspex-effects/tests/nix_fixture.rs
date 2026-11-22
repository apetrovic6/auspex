//! The plan's step-1 effects integration test: the real `nix eval` and `nix-eval-jobs`
//! against a fixture flake with no inputs (`tests/fixtures/flake`).
//!
//! The fixture is copied into a fresh git repository in a temporary directory and committed
//! and referred to by a plain path, which `FlakeRef` normalises to `git+file`.
//! The CLI and `builtins.getFlake` must read the same tracked source. The cache key
//! comes from a clean tree of known state.
//!
//! Skipped, with a message, when `nix`, `nix-eval-jobs` or `git` is not on `PATH`, which is
//! the case inside the `ci-pure` sandbox: there is no Nix daemon to evaluate against there.
//!
//! The edit-and-undo guard test uses unique loopback HTTP fetches as worker barriers.
//! Its impure absolute read of a tracked file observes the live edit, not the flake's
//! immutable store snapshot; the server releases each worker only after the edit/undo.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use auspex_domain::eval::EvalOutcome;
use auspex_domain::fleet::MachineClass;
use auspex_effects::CancellationToken;
use auspex_effects::cache::{CacheParams, GitParams, evaluate_cached};
use auspex_effects::nix::{DEFAULT_NIX, EvalItem, EvalMachine, EvalParams, FlakeRef, inventory};
use common::{PATIENT, find_on_path, tempdir};
use tokio::sync::mpsc::unbounded_channel;

const NASTY: &str = r#"x"; builtins.abort "pwned"#;

/// The bound on a real `nix` or `nix-eval-jobs` run here (the evaluation's own deadline, and the
/// machine list's): ten minutes where it was two. It is a bound on a hang, not on speed, and real
/// nix is not the fake programs: on a core shared with twelve busy loops it needs several minutes
/// to evaluate a four-machine fixture, and a deadline inside that is a test that fails on the
/// load (AUS-47: `evaluation timed out after 120s` with nothing hung).
const REAL_NIX: Duration = Duration::from_secs(600);

/// The cache's git, with [`PATIENT`] for each command and for the worktree read. The product's
/// 2 s per git command is right for an operator, and a loaded machine misses it: the run then
/// prints "eval cache off for this run: git rev-parse timed out after 2s", which is a
/// different run from the one these tests look at (AUS-47).
fn patient_git() -> GitParams<'static> {
    let mut git = GitParams::new();
    git.timeout = PATIENT;
    git.read_timeout = PATIENT;
    git
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        // The developer's own config (commit signing, hooks) must not take part.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}: {ok:?}");
}

async fn run(flake: &FlakeRef, machines: &[EvalMachine], cache: &CacheParams<'_>) -> Vec<EvalItem> {
    let mut eval = EvalParams::new();
    eval.runtime_dir = Some(common::runtime_dir());
    eval.workers = 2;
    eval.max_memory_mib = 1024;
    eval.timeout = REAL_NIX;
    let (tx, mut rx) = unbounded_channel();
    evaluate_cached(flake, machines, &eval, cache, &CancellationToken::new(), tx)
        .await
        .unwrap();
    let mut items = Vec::new();
    while let Some(i) = rx.recv().await {
        items.push(i);
    }
    items
}

/// machine -> (`from_cache`, outcome); and the final error.
fn split(items: &[EvalItem]) -> (BTreeMap<String, (bool, EvalOutcome)>, Option<String>) {
    let mut by = BTreeMap::new();
    let mut fin = None;
    for i in items {
        match i {
            EvalItem::Machine {
                machine,
                outcome,
                from_cache,
            } => {
                assert!(
                    by.insert(machine.clone(), (*from_cache, outcome.clone()))
                        .is_none(),
                    "two results for {machine}"
                );
            }
            EvalItem::Note(n) => eprintln!("note: {n}"),
            EvalItem::Finished { error } => fin = Some(error.clone()),
        }
    }
    (by, fin.expect("a Finished item"))
}

#[tokio::test]
async fn the_fixture_flake_with_real_nix() {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH (as inside the ci-pure sandbox)");
            return;
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/flake/flake.nix");
    let repo = tempdir();
    fs::copy(&src, repo.path().join("flake.nix")).unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["add", "flake.nix"]);
    git(repo.path(), &["commit", "-q", "-m", "fixture"]);
    // FlakeRef makes the git source explicit for both Nix entry points.
    let flake = FlakeRef::new(repo.path().to_str().unwrap()).unwrap();

    // The machine list.
    let list = inventory(
        OsStr::new(DEFAULT_NIX),
        &flake,
        REAL_NIX,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    eprintln!("machine list: {list:#?}");
    let names: Vec<_> = list.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["1odd", "broken", "genetor", "luna", "terra"]);
    let genetor = &list[2];
    assert_eq!(genetor.class, MachineClass::Darwin);
    assert_eq!(genetor.target_host.as_deref(), Some("admin@192.0.2.20"));
    assert_eq!(list[4].tags, ["server", "k8s"]);
    assert_eq!(list[3].build_host.as_deref(), Some("localhost"));

    // Every listed machine, plus one whose name is an attempt at Nix injection.
    let mut machines: Vec<EvalMachine> = list
        .iter()
        .map(|m| EvalMachine {
            name: m.name.clone(),
            class: m.class,
        })
        .collect();
    machines.push(EvalMachine {
        name: NASTY.to_owned(),
        class: MachineClass::Nixos,
    });

    let cache_dir = tempdir();
    let cache = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: repo.path(),
        git: patient_git(),
    };

    let first = run(&flake, &machines, &cache).await;
    eprintln!("first run: {first:#?}");
    let (by, error) = split(&first);
    assert_eq!(error, None, "nix-eval-jobs exited 0");
    assert_eq!(by.len(), 6, "a result per machine: {by:#?}");
    for ok in ["terra", "luna", "genetor", "1odd"] {
        let (from_cache, outcome) = &by[ok];
        assert!(!from_cache);
        let EvalOutcome::Success(s) = outcome else {
            panic!("{ok}: {outcome:?}")
        };
        assert_eq!(s.attr, ok);
        let prefix = if ok == "genetor" {
            "darwin-system-"
        } else {
            "nixos-system-"
        };
        assert_eq!(s.out_path.name(), format!("{prefix}{ok}"));
    }
    let EvalOutcome::Error(e) = &by["broken"].1 else {
        panic!()
    };
    assert_eq!(
        e.summary(),
        Some("error: broken fails to evaluate, on purpose")
    );
    let EvalOutcome::Error(e) = &by[NASTY].1 else {
        panic!()
    };
    let summary = e.summary().unwrap();
    assert!(summary.contains("missing"), "{summary}");
    assert!(!summary.contains("evaluation aborted"), "{summary}");

    // A clean tree, unchanged: every success comes from the cache. Errors are never
    // cached, so those two are evaluated again.
    let second = run(&flake, &machines, &cache).await;
    eprintln!("second run: {second:#?}");
    let (by2, error) = split(&second);
    assert_eq!(error, None);
    for ok in ["terra", "luna", "genetor", "1odd"] {
        assert!(by2[ok].0, "{ok} read from the cache");
        assert_eq!(by2[ok].1, by[ok].1);
    }
    assert!(!by2["broken"].0);
    assert!(!by2[NASTY].0);
}

/// Plain paths, including nested flakes and URI-sensitive directory names, must
/// not leak ignored/untracked files or .git into getFlake while inventory uses git.
#[tokio::test]
async fn plain_paths_use_the_same_git_source_for_inventory_and_evaluation() {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let temp = tempdir();
    let root = temp.path().join("repo with + % &");
    let nested = root.join("fleet with + % &");
    fs::create_dir_all(&nested).unwrap();
    let source = r#"{
      outputs = { self }: let
        visible = builtins.pathExists ./untracked || builtins.pathExists ./ignored;
        metadata = builtins.pathExists ./.git;
        revision = if self ? rev then self.rev else "dirty";
      in {
        clanInternals.inventoryClass.inventory.machines =
          if visible || metadata then { wrong = {}; } else { terra = {}; };
        nixosConfigurations.terra.config.system.build.toplevel = derivation {
          name = "source-consistency";
          system = "x86_64-linux";
          builder = "/auspex-fixture-never-built";
          inherit visible metadata revision;
        };
      };
    }"#;
    for dir in [&root, &nested] {
        fs::write(dir.join("flake.nix"), source).unwrap();
    }
    fs::write(root.join(".gitignore"), "ignored\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "source consistency"]);
    let cache_dir = tempdir();
    let machines = [EvalMachine {
        name: "terra".to_owned(),
        class: MachineClass::Nixos,
    }];
    for dir in [&root, &nested] {
        let flake = FlakeRef::new(dir.to_str().unwrap()).unwrap();
        let cache = CacheParams {
            cache_dir: Some(cache_dir.path()),
            flake_dir: dir,
            git: patient_git(),
        };
        let (before, error) = split(&run(&flake, &machines, &cache).await);
        assert_eq!(error, None);
        assert!(matches!(before["terra"], (false, EvalOutcome::Success(_))));
        fs::write(dir.join("untracked"), "not in a git source").unwrap();
        fs::write(dir.join("ignored"), "not in a git source either").unwrap();
        git(
            &root,
            &[
                "checkout",
                "-q",
                "-b",
                if dir == &root { "other" } else { "nested" },
            ],
        );
        let list = inventory(
            OsStr::new(DEFAULT_NIX),
            &flake,
            REAL_NIX,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            list.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["terra"]
        );
        let (warm, error) = split(&run(&flake, &machines, &cache).await);
        assert_eq!(error, None);
        assert!(warm["terra"].0);
        let uncached = CacheParams {
            cache_dir: None,
            ..cache
        };
        let (fresh, error) = split(&run(&flake, &machines, &uncached).await);
        assert_eq!(error, None);
        assert!(!fresh["terra"].0);
        assert_eq!(before["terra"].1, warm["terra"].1);
        assert_eq!(before["terra"].1, fresh["terra"].1);
    }
}

/// The precondition of the scenario was not met: the evaluation ended before a second worker
/// success, so there was no moment to undo the change in.
const NO_SECOND_SUCCESS: &str = "fewer than two successes";

/// A change at the first success and its undo at the second, with the real nix-eval-jobs:
/// `workers = 1` and `max_memory_mib = 1`, so every job's worker reads the tree again.
///
/// Checks that the key after the run equals the key before it (so only the run guard
/// can have refused), that the note was sent, that the next run on the restored tree
/// serves nothing from the cache, and that some worker really read the changed tree (a
/// result differs from the restored run's). `None` when a tool is missing.
///
/// Under load, nix-eval-jobs with `max_memory_mib = 1` can kill a worker before it reports
/// ("memory budget of 1 MiB exceeded ... evaluation worker exited with 0") and exit 1; that
/// is its own race, not the guard's, and the scenario is then run again in a fresh
/// repository, at most three times. An evaluation that ends with fewer than two
/// successes (the undo has no moment to happen in) is retried the same way. If all three
/// attempts are lost to these races the test prints SKIPPED and returns `None`: an explicit skip. Any other failure fails at once.
async fn real_change_and_undo(
    setup: impl Fn(&Path) + Sync,
    change: impl Fn(&Path) + Sync,
    undo: impl Fn(&Path) + Sync,
) -> Option<()> {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return None;
        }
    }
    let mut last = String::new();
    for attempt in 1..=3 {
        match real_attempt(&setup, &change, &undo).await {
            Ok(()) => return Some(()),
            Err(e) => {
                eprintln!("attempt {attempt}: nix-eval-jobs's own race: {e}");
                last = e;
            }
        }
    }
    // Three runs in a row never reached the scenario (a worker killed on the 1 MiB budget, or
    // fewer than two successes, so no moment for the undo): nothing was tested. An explicit
    // skip, not a pass and not a failure.
    eprintln!("SKIPPED: three attempts under load never reached the scenario; last: {last}");
    None
}

#[expect(
    clippy::too_many_lines,
    reason = "one scenario: the changed run, the checks, the restored run"
)]
async fn real_attempt(
    setup: &(impl Fn(&Path) + Sync),
    change: &(impl Fn(&Path) + Sync),
    undo: &(impl Fn(&Path) + Sync),
) -> Result<(), String> {
    let repo = tempdir();
    let r = repo.path();
    fs::write(r.join("extra.nix"), "original").unwrap();
    fs::write(
        r.join("flake.nix"),
        r#"{
      outputs = { self }: {
        nixosConfigurations = builtins.listToAttrs (map (name: {
          inherit name;
          value.config.system.build.toplevel = derivation {
            inherit name;
            system = "x86_64-linux";
            builder = "/auspex-fixture-never-built";
            source = builtins.readFile ./extra.nix;
            rev = self.rev or self.dirtyRev or "none";
            work = builtins.toString (builtins.foldl' (a: b: a + b) 0
              (builtins.genList (n: n) 200000));
          };
        }) ["a" "b" "c" "d"]);
      };
    }"#,
    )
    .unwrap();
    git(r, &["init", "-q"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "one"]);
    setup(r);
    let flake = FlakeRef::new(r.to_str().unwrap()).unwrap();
    let cache_dir = tempdir();
    let cache = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: r,
        git: patient_git(),
    };
    let key = || async {
        auspex_effects::cache::key_for(r, &patient_git(), &CancellationToken::new())
            .await
            .unwrap()
            .unwrap()
    };
    let machines: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|name| EvalMachine {
            name: name.to_owned(),
            class: MachineClass::Nixos,
        })
        .collect();
    let mut eval = EvalParams::new();
    eval.runtime_dir = Some(common::runtime_dir());
    eval.workers = 1;
    eval.max_memory_mib = 1; // Force a new source read for every worker.
    let before = key().await;
    let (tx, mut rx) = unbounded_channel();
    let collect = async {
        let (mut change, mut undo) = (Some(change), Some(undo));
        let mut items = Vec::new();
        while let Some(item) = rx.recv().await {
            if matches!(
                &item,
                EvalItem::Machine {
                    outcome: EvalOutcome::Success(_),
                    ..
                }
            ) {
                if let Some(f) = change.take() {
                    f(r);
                } else if let Some(f) = undo.take() {
                    f(r);
                }
            }
            items.push(item);
        }
        (items, undo.is_none())
    };
    let cancel = CancellationToken::new();
    let (result, (items, undone)) = tokio::join!(
        evaluate_cached(&flake, &machines, &eval, &cache, &cancel, tx),
        collect,
    );
    result.unwrap();
    if let Some(EvalItem::Finished { error: Some(e) }) = items.last()
        && e.contains("memory budget of 1 MiB exceeded")
        && e.contains("evaluation worker exited with 0")
    {
        return Err(e.clone());
    }
    if !undone {
        return Err(format!("{NO_SECOND_SUCCESS}: {items:?}"));
    }
    assert_eq!(key().await, before, "the undo must restore the key exactly");
    assert!(
        items.iter().any(|i| matches!(
            i,
            EvalItem::Note(n) if n == "eval results not cached: the worktree changed during evaluation"
        )),
        "{items:?}"
    );
    let (changed, error) = split(&items);
    assert_eq!(error, None);
    let (restored, error) = split(&run(&flake, &machines, &cache).await);
    assert_eq!(error, None);
    for name in ["a", "b", "c", "d"] {
        assert!(!restored[name].0, "{name} was cached from the changed run");
    }
    assert!(
        ["b", "c", "d"]
            .into_iter()
            .any(|name| changed[name].1 != restored[name].1),
        "the experiment did not exercise a worker reading the changed tree"
    );
    Ok(())
}

/// Review finding N2: hold real workers at HTTP fetches while editing, and undoing if `undo`.
///
/// `--impure` permits loopback fetches; unlike a FIFO read, a fetch can block and
/// still yield a successful derivation. Unique URLs prevent fetcher-cache hits.
/// No memory-pressure restarts are needed, and none are asked for: nix-eval-jobs 2.35 enforces
/// `workers * max-memory-size` as one budget for all its workers *and its own process*, so a
/// budget small enough to restart a worker after every job (the `1` this suite used) is one
/// that is exceeded while a job runs, and whether it then restarts the worker after the job or
/// kills it in the middle ("memory budget of 1 MiB exceeded ... evaluation worker exited with
/// 0", "evaluation exceeded the memory budget ... even when run alone") is a race the
/// evaluator wins on an idle machine and loses on a loaded one (AUS-47: four of four runs lost
/// at one core with twelve busy loops, none of fifteen idle ones). No budget makes a restart
/// certain and a kill impossible, so the scenario does not rest on one: a gate decides, to the
/// request, when the tree changes while workers evaluate.
#[expect(
    clippy::too_many_lines,
    reason = "one synchronized real-evaluator scenario"
)]
#[expect(
    clippy::future_not_send,
    reason = "a test helper, polled on the test's own task (it holds a `Cell`)"
)]
async fn gated_edit(undo: bool) {
    use std::cell::Cell;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let repo = tempdir();
    let r = repo.path();
    let nonce = r.file_name().unwrap().to_str().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/{nonce}", listener.local_addr().unwrap());
    fs::write(r.join("extra.nix"), "original").unwrap();
    fs::write(
        r.join("flake.nix"),
        format!(
            r#"{{
      outputs = _: {{
        nixosConfigurations = builtins.listToAttrs (map (name: {{
          inherit name;
          value.config.system.build.toplevel =
            builtins.seq (builtins.fetchurl ("{url}/gate-" + name)) (derivation {{
              inherit name;
              system = "x86_64-linux";
              builder = "/auspex-fixture-never-built";
              source = builtins.readFile "{repo}/extra.nix";
            }});
        }}) ["a" "b"]);
      }};
    }}"#,
            repo = r.display(),
        ),
    )
    .unwrap();
    git(r, &["init", "-q"]);
    git(r, &["add", "flake.nix", "extra.nix"]);
    git(r, &["commit", "-q", "-m", "HTTP gates"]);
    let flake = FlakeRef::new(r.to_str().unwrap()).unwrap();
    let cache_dir = tempdir();
    let cache = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: r,
        git: patient_git(),
    };
    let machines: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| EvalMachine {
            name: name.to_owned(),
            class: MachineClass::Nixos,
        })
        .collect();
    let key = || async {
        auspex_effects::cache::key_for(r, &patient_git(), &CancellationToken::new())
            .await
            .unwrap()
            .unwrap()
    };
    let requests = Cell::new(0);
    let serve = async {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            match requests.get() {
                0 => {
                    assert!(line.starts_with(&format!("GET /{nonce}/gate-a ")), "{line}");
                    fs::write(r.join("extra.nix"), "edited").unwrap();
                }
                1 => {
                    assert!(line.starts_with(&format!("GET /{nonce}/gate-b ")), "{line}");
                    if undo {
                        fs::write(r.join("extra.nix"), "original").unwrap();
                    }
                }
                _ => {}
            }
            requests.set(requests.get() + 1);
            loop {
                line.clear();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
            }
            socket
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        }
    };
    let scenario = async {
        let before = key().await;
        let mut eval = EvalParams::new();
        eval.runtime_dir = Some(common::runtime_dir());
        eval.workers = 1;
        eval.max_memory_mib = 1024;
        eval.timeout = REAL_NIX;
        let (tx, mut rx) = unbounded_channel();
        evaluate_cached(
            &flake,
            &machines,
            &eval,
            &cache,
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
        let mut items = Vec::new();
        while let Some(item) = rx.recv().await {
            items.push(item);
        }
        assert_eq!(requests.get(), 2, "both gates must be reached");
        if undo {
            assert_eq!(key().await, before, "the undo must restore the key exactly");
        } else {
            assert_ne!(key().await, before, "the edit must change the key");
            // Back to the committed tree, so the next run's key is the one this run began with.
            git(r, &["checkout", "--", "extra.nix"]);
            assert_eq!(key().await, before, "the checkout must restore the key");
        }
        assert!(
            items.iter().any(|item| matches!(
                item,
                EvalItem::Note(note) if note == "eval results not cached: the worktree changed during evaluation"
            )),
            "{items:?}"
        );
        let (changed, error) = split(&items);
        assert_eq!(error, None);
        let (restored, error) = split(&run(&flake, &machines, &cache).await);
        assert_eq!(error, None);
        for name in ["a", "b"] {
            assert!(
                matches!(changed[name].1, EvalOutcome::Success(_)),
                "{items:?}"
            );
            assert!(matches!(restored[name].1, EvalOutcome::Success(_)));
            assert!(!restored[name].0, "{name} was cached from the changed run");
        }
        assert_ne!(
            changed["a"].1, restored["a"].1,
            "machine a must read the edit"
        );
        if !undo {
            // The restored run was cached under the restored tree's key, and nothing else is.
            let (warm, error) = split(&run(&flake, &machines, &cache).await);
            assert_eq!(error, None);
            for name in ["a", "b"] {
                assert!(warm[name].0, "{name} was not served from the cache");
                assert_eq!(warm[name].1, restored[name].1);
            }
        }
    };
    tokio::select! {
        () = scenario => {}
        () = serve => unreachable!(),
    }
}

/// Review finding N2: an edit at the first gate, undone at the second. The run's results are
/// not cached under the key gathered before it, though the key after it is the same.
#[tokio::test]
async fn an_edit_undone_during_a_real_evaluation_is_not_cached() {
    gated_edit(true).await;
}

/// An edit at the first gate, not undone until the run is over: none of the run's successes may
/// be published under the key gathered before the edit, even though results stream early, and
/// the next run on the restored tree serves nothing from the cache. (This was
/// `edits_during_real_worker_restarts_do_not_poison_the_cache`, which asked nix-eval-jobs to
/// restart its worker after every job with a 1 MiB budget: see [`gated_edit`] for why that is a
/// race on a loaded machine.)
#[tokio::test]
async fn an_edit_during_a_real_evaluation_does_not_poison_the_cache() {
    gated_edit(false).await;
}

/// Review finding B1 with the real nix-eval-jobs: `reset --soft HEAD~` at the first
/// result, `reset --soft ORIG_HEAD` at the second. Only the branch moves; nix's
/// `self.rev` (and its dirty state) differs in between.
///
/// What only this test covers (and `an_edit_during_a_real_evaluation_does_not_poison_the_cache`
/// above does not) is a worker that is *restarted* and re-reads the flake's own copy of a file
/// (`./extra.nix`) after the tree has moved: the gated tests edit an absolute path that one
/// worker reads. A restart needs the memory budget to cause it, which no budget does reliably
/// (see [`gated_edit`]), so this test retries and may print SKIPPED when none happened: on such a
/// run that property is not exercised at all, and a green here does not say it was.
#[tokio::test]
async fn a_soft_reset_undone_during_real_worker_restarts_is_not_cached() {
    real_change_and_undo(
        |r| {
            fs::write(r.join("extra.nix"), "two").unwrap();
            git(r, &["commit", "-q", "-am", "two"]);
        },
        |r| git(r, &["reset", "-q", "--soft", "HEAD~"]),
        |r| git(r, &["reset", "-q", "--soft", "ORIG_HEAD"]),
    )
    .await;
}

/// Review item N3: `[`, `]`, `|` and `^` in a repository path made nix refuse the reference
/// ("is not an absolute path"), for the machine list and for the evaluation alike. The
/// repository and a nested flake directory carry all four, and the evaluation is cached and
/// read back.
#[tokio::test]
async fn brackets_pipes_and_carets_in_the_repository_path_still_evaluate() {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let temp = tempdir();
    let root = temp.path().join("fleet|[1]^");
    let nested = root.join("sub]|^[");
    fs::create_dir_all(&nested).unwrap();
    let source = r#"{
      outputs = _: {
        clanInternals.inventoryClass.inventory.machines.terra = {};
        nixosConfigurations.terra.config.system.build.toplevel = derivation {
          name = "brackets";
          system = "x86_64-linux";
          builder = "/auspex-fixture-never-built";
        };
      };
    }"#;
    for dir in [&root, &nested] {
        fs::write(dir.join("flake.nix"), source).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "brackets"]);
    let cache_dir = tempdir();
    let machines = [EvalMachine {
        name: "terra".to_owned(),
        class: MachineClass::Nixos,
    }];
    for dir in [&root, &nested] {
        let flake = FlakeRef::new(dir.to_str().unwrap()).unwrap();
        let list = inventory(
            OsStr::new(DEFAULT_NIX),
            &flake,
            REAL_NIX,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{}: {e}", flake.as_str()));
        assert_eq!(
            list.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["terra"]
        );
        let cache = CacheParams {
            cache_dir: Some(cache_dir.path()),
            flake_dir: dir,
            git: patient_git(),
        };
        let (first, error) = split(&run(&flake, &machines, &cache).await);
        assert_eq!(error, None, "{}", flake.as_str());
        assert!(matches!(first["terra"], (false, EvalOutcome::Success(_))));
        let (warm, error) = split(&run(&flake, &machines, &cache).await);
        assert_eq!(error, None);
        assert!(warm["terra"].0, "the second run is served from the cache");
        assert_eq!(first["terra"].1, warm["terra"].1);
    }
}

/// Review item N1, setup A, end to end with the real nix: a committed file made executable
/// is dirty to nix under `core.fileMode=true` and clean under `false`, so `self.rev` and
/// every toplevel that records it differ. One key used to stand for both.
#[tokio::test]
async fn a_git_setting_that_changes_dirtiness_is_not_a_stale_hit() {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let repo = tempdir();
    let r = repo.path();
    fs::write(
        r.join("flake.nix"),
        r#"{ outputs = { self }: {
          nixosConfigurations.m.config.system.build.toplevel = derivation {
            name = "toplevel";
            system = "x86_64-linux";
            builder = "/auspex-fixture-never-built";
            configurationRevision = self.rev or self.dirtyRev or "none";
            v = builtins.readFile ./v.txt;
          };
        }; }"#,
    )
    .unwrap();
    fs::write(r.join("v.txt"), "one\n").unwrap();
    git(r, &["init", "-q"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "one"]);
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(r.join("v.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let flake = FlakeRef::new(r.to_str().unwrap()).unwrap();
    let machines = [EvalMachine {
        name: "m".to_owned(),
        class: MachineClass::Nixos,
    }];
    let cache_dir = tempdir();
    let cached = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: r,
        git: patient_git(),
    };
    let uncached = CacheParams {
        cache_dir: None,
        ..cached
    };
    let (first, error) = split(&run(&flake, &machines, &cached).await);
    assert_eq!(error, None);
    assert!(!first["m"].0);

    git(r, &["config", "core.fileMode", "false"]);
    let (warm, error) = split(&run(&flake, &machines, &cached).await);
    assert_eq!(error, None);
    let (fresh, error) = split(&run(&flake, &machines, &uncached).await);
    assert_eq!(error, None);
    // The experiment must be able to tell: nix does read the two settings differently.
    assert_ne!(
        first["m"].1, fresh["m"].1,
        "core.fileMode did not change nix's answer"
    );
    assert_eq!(warm["m"].1, fresh["m"].1, "a stale result was served");
    assert!(
        !warm["m"].0,
        "the second setting must not hit the first one's entry"
    );
}

/// Review item N6, with the real nix: a tracked configuration imports a module that is not
/// tracked. nix's own error names neither git nor tracking; the operator gets a note that does,
/// and once the file is added the machine evaluates.
#[tokio::test]
async fn an_untracked_import_is_explained_and_added_it_evaluates() {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let repo = tempdir();
    let r = repo.path();
    fs::write(
        r.join("flake.nix"),
        r#"{ outputs = _: {
          nixosConfigurations.m.config.system.build.toplevel = derivation {
            name = "toplevel";
            system = "x86_64-linux";
            builder = "/auspex-fixture-never-built";
            v = import ./new-module.nix;
          };
        }; }"#,
    )
    .unwrap();
    git(r, &["init", "-q"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "one"]);
    fs::write(r.join("new-module.nix"), "\"x\"").unwrap();
    let flake = FlakeRef::new(r.to_str().unwrap()).unwrap();
    let machines = [EvalMachine {
        name: "m".to_owned(),
        class: MachineClass::Nixos,
    }];
    let uncached = CacheParams {
        cache_dir: None,
        flake_dir: r,
        git: patient_git(),
    };
    let items = run(&flake, &machines, &uncached).await;
    let (by, error) = split(&items);
    assert_eq!(error, None);
    assert!(matches!(by["m"].1, EvalOutcome::Error(_)), "{:?}", by["m"]);
    let notes: Vec<&String> = items
        .iter()
        .filter_map(|i| match i {
            EvalItem::Note(n) => Some(n),
            _ => None,
        })
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("new-module.nix") && n.contains("not tracked by git")),
        "{notes:?}"
    );
    git(r, &["add", "new-module.nix"]);
    let items = run(&flake, &machines, &uncached).await;
    let (by, _) = split(&items);
    assert!(
        matches!(by["m"].1, EvalOutcome::Success(_)),
        "{:?}",
        by["m"]
    );
    assert!(
        !items
            .iter()
            .any(|i| matches!(i, EvalItem::Note(n) if n.contains("not tracked")))
    );
}

/// Review of PR #37, finding 1, end to end with the real nix: a file committed with LF endings
/// and checked out with CRLF is dirty to nix until `core.autocrlf` or an attributes file outside
/// the tree makes the CRLF filter turn it back into the committed bytes. `touch` makes
/// libgit2 look at the file instead of trusting the index's stat data. One key used to stand
/// for both answers.
#[tokio::test]
async fn crlf_settings_and_attribute_files_do_not_give_stale_hits() {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let repo = tempdir();
    let r = repo.path();
    fs::write(
        r.join("flake.nix"),
        r#"{ outputs = { self }: {
          nixosConfigurations.m.config.system.build.toplevel = derivation {
            name = "toplevel";
            system = "x86_64-linux";
            builder = "/auspex-fixture-never-built";
            configurationRevision = self.rev or self.dirtyRev or "none";
          };
        }; }"#,
    )
    .unwrap();
    fs::write(r.join("f.nix"), "a\nb\n").unwrap();
    git(r, &["init", "-q"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "one"]);
    // The worktree has CRLF, the index the LF blob (added through the filter).
    fs::write(r.join("f.nix"), "a\r\nb\r\n").unwrap();
    git(r, &["-c", "core.autocrlf=true", "add", "f.nix"]);
    let flake = FlakeRef::new(r.to_str().unwrap()).unwrap();
    let machines = [EvalMachine {
        name: "m".to_owned(),
        class: MachineClass::Nixos,
    }];
    let cache_dir = tempdir();
    let cached = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: r,
        git: patient_git(),
    };
    let uncached = CacheParams {
        cache_dir: None,
        ..cached
    };
    let touch = || {
        let f = fs::OpenOptions::new()
            .append(true)
            .open(r.join("f.nix"))
            .unwrap();
        f.set_modified(std::time::SystemTime::now()).unwrap();
    };
    touch();
    let (first, error) = split(&run(&flake, &machines, &cached).await);
    assert_eq!(error, None);
    let (dirty, _) = split(&run(&flake, &machines, &uncached).await);
    assert_eq!(first["m"].1, dirty["m"].1);

    for (what, apply) in [
        ("core.autocrlf", &["config", "core.autocrlf", "true"][..]),
        ("core.eol", &["config", "core.eol", "lf"][..]),
    ] {
        git(r, apply);
        touch();
        let (warm, error) = split(&run(&flake, &machines, &cached).await);
        assert_eq!(error, None);
        let (fresh, _) = split(&run(&flake, &machines, &uncached).await);
        assert_eq!(
            warm["m"].1, fresh["m"].1,
            "{what}: a stale result was served"
        );
        if what == "core.autocrlf" {
            // The experiment can tell: nix reads the file as clean once the filter runs.
            assert_ne!(
                first["m"].1, fresh["m"].1,
                "core.autocrlf did not change nix's answer"
            );
            assert!(!warm["m"].0, "{what}: hit the other setting's entry");
        }
        git(r, &["config", "--unset", what]);
    }

    // The same through an attributes file that is not in the tree.
    fs::create_dir_all(r.join(".git/info")).unwrap();
    fs::write(r.join(".git/info/attributes"), "* text=auto\n").unwrap();
    touch();
    let (warm, error) = split(&run(&flake, &machines, &cached).await);
    assert_eq!(error, None);
    let (fresh, _) = split(&run(&flake, &machines, &uncached).await);
    assert_eq!(
        warm["m"].1, fresh["m"].1,
        "info/attributes: a stale result was served"
    );
    assert_ne!(
        first["m"].1, fresh["m"].1,
        "info/attributes did not change nix's answer"
    );
    assert!(!warm["m"].0, "info/attributes: hit the first run's entry");
}
