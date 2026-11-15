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
use common::{find_on_path, tempdir};
use tokio::sync::mpsc::unbounded_channel;

const NASTY: &str = r#"x"; builtins.abort "pwned"#;

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
    eval.timeout = Duration::from_secs(120);
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
        Duration::from_secs(120),
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
        git: GitParams::new(),
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
            git: GitParams::new(),
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
            Duration::from_secs(120),
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

/// A worker restart reads a changed source. None of that run's successes may be
/// published under the key gathered before the edit, even if results stream early.
#[tokio::test]
async fn edits_during_real_worker_restarts_do_not_poison_the_cache() {
    for tool in ["nix", "nix-eval-jobs", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let repo = tempdir();
    fs::write(repo.path().join("extra.nix"), "original").unwrap();
    fs::write(
        repo.path().join("flake.nix"),
        r#"{
      outputs = _: {
        nixosConfigurations = builtins.listToAttrs (map (name: {
          inherit name;
          value.config.system.build.toplevel = derivation {
            inherit name;
            system = "x86_64-linux";
            builder = "/auspex-fixture-never-built";
            source = builtins.readFile ./extra.nix;
            work = builtins.toString (builtins.foldl' (a: b: a + b) 0
              (builtins.genList (n: n) 200000));
          };
        }) ["a" "b" "c" "d"]);
      };
    }"#,
    )
    .unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "worker restarts"]);
    let flake = FlakeRef::new(repo.path().to_str().unwrap()).unwrap();
    let cache_dir = tempdir();
    let cache = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: repo.path(),
        git: GitParams::new(),
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
    let (tx, mut rx) = unbounded_channel();
    let collect = async {
        let mut items = Vec::new();
        let mut edited = false;
        while let Some(item) = rx.recv().await {
            if matches!(
                &item,
                EvalItem::Machine {
                    outcome: EvalOutcome::Success(_),
                    ..
                }
            ) && !edited
            {
                fs::write(repo.path().join("extra.nix"), "edited").unwrap();
                edited = true;
            }
            items.push(item);
        }
        assert!(edited, "no success arrived: {items:?}");
        items
    };
    let cancel = CancellationToken::new();
    let (result, edited) = tokio::join!(
        evaluate_cached(&flake, &machines, &eval, &cache, &cancel, tx),
        collect,
    );
    result.unwrap();
    let (changed, error) = split(&edited);
    assert_eq!(error, None);
    git(repo.path(), &["checkout", "--", "extra.nix"]);
    let (restored, error) = split(&run(&flake, &machines, &cache).await);
    assert_eq!(error, None);
    for name in ["a", "b", "c", "d"] {
        assert!(
            !restored[name].0,
            "{name} was cached under the pre-edit key"
        );
        assert!(matches!(restored[name].1, EvalOutcome::Success(_)));
    }
    assert!(
        ["b", "c", "d"]
            .into_iter()
            .any(|name| changed[name].1 != restored[name].1),
        "the experiment did not exercise a worker reading the edited tree"
    );
    let (warm, error) = split(&run(&flake, &machines, &cache).await);
    assert_eq!(error, None);
    for name in ["a", "b", "c", "d"] {
        assert!(warm[name].0);
        assert_eq!(warm[name].1, restored[name].1);
    }
}
