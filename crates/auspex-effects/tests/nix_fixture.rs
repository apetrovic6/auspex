//! The plan's step-1 effects integration test: the real `nix eval` and `nix-eval-jobs`
//! against a fixture flake with no inputs (`tests/fixtures/flake`).
//!
//! The fixture is copied into a fresh git repository in a temporary directory and referred
//! to with a `path:` flake ref, so it works untracked ("Flakes only see tracked files") and
//! the cache key comes from a clean tree of known state.
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
    // A plain path in a git repository: nix reads it as git+file (committed files only), and
    // only git references are cached. A `path:` reference would get no caching.
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
