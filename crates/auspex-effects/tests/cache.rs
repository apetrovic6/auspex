//! The eval cache: entries, the key from a real (temporary) git repository, and the cache
//! in front of a fake `nix-eval-jobs`.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use auspex_domain::eval::{CacheEntry, EvalOutcome, EvalSuccess};
use auspex_domain::fleet::MachineClass;
use auspex_domain::observe::StorePath;
use auspex_effects::CancellationToken;
use auspex_effects::cache::{
    CacheParams, GitParams, entry_path, evaluate_cached, key_for, lookup, store,
};
use auspex_effects::nix::{EvalItem, EvalMachine, EvalParams, FlakeRef};
use common::{find_on_path, in_nix_build, tempdir, tools};
use tokio::sync::mpsc::unbounded_channel;

fn success(machine: &str, n: u8) -> EvalSuccess {
    let h = format!("{n}123456789abcdfghijklmnpqrsvwxyz");
    let h = &h[..32];
    EvalSuccess {
        attr: machine.to_owned(),
        attr_path: vec![machine.to_owned()],
        drv_path: StorePath::new(format!("/nix/store/{h}-nixos-system-{machine}.drv")).unwrap(),
        out_path: StorePath::new(format!("/nix/store/{h}-nixos-system-{machine}")).unwrap(),
    }
}

/// A git repository with one commit, or `None` (skipped) without git.
fn repo() -> Option<tempfile::TempDir> {
    if !have_git() {
        return None;
    }
    let dir = tempdir();
    fs::write(dir.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    fs::write(dir.path().join("flake.lock"), "{}\n").unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "flake.nix", "flake.lock"]);
    git(dir.path(), &["commit", "-q", "-m", "one"]);
    Some(dir)
}

/// Whether git is here. `ci-test` puts git in the sandbox (`nativeCheckInputs`), so there a
/// missing git fails rather than skips: a skip would be a false green.
fn have_git() -> bool {
    if find_on_path("git").is_some() {
        return true;
    }
    assert!(!in_nix_build(), "git is not on PATH inside the Nix build");
    eprintln!("SKIPPED: git is not on PATH");
    false
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

async fn key(dir: &Path) -> Result<auspex_domain::eval::CacheKey, String> {
    key_for(dir, &GitParams::new(), &CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn hit_miss_corrupt_and_wrong_machine() {
    let Some(r) = repo() else { return };
    let k = key(r.path()).await.unwrap();
    let cache = tempdir();
    let c = cache.path();

    assert_eq!(
        lookup(c, &k, "terra").await,
        None,
        "a miss before anything is stored"
    );
    store(c, &k, "terra", &success("terra", 1)).await.unwrap();
    assert_eq!(lookup(c, &k, "terra").await, Some(success("terra", 1)));
    assert_eq!(lookup(c, &k, "luna").await, None, "another machine misses");

    // Corrupt: truncated JSON.
    let path = entry_path(c, &k, "terra");
    let good = fs::read(&path).unwrap();
    fs::write(&path, &good[..good.len() / 2]).unwrap();
    assert_eq!(lookup(c, &k, "terra").await, None);
    fs::write(&path, b"\xff\xfe not json").unwrap();
    assert_eq!(lookup(c, &k, "terra").await, None);

    // Wrong machine: luna's entry copied to terra's place.
    store(c, &k, "luna", &success("luna", 2)).await.unwrap();
    fs::copy(entry_path(c, &k, "luna"), &path).unwrap();
    assert_eq!(lookup(c, &k, "terra").await, None);

    // A future format version.
    let mut entry: serde_json::Value = serde_json::from_str(
        &CacheEntry::new(k.clone(), "terra".to_owned(), success("terra", 1)).to_json(),
    )
    .unwrap();
    entry["version"] = serde_json::json!(99);
    fs::write(&path, entry.to_string()).unwrap();
    assert_eq!(lookup(c, &k, "terra").await, None);

    // A machine name is never a path component.
    let p = entry_path(c, &k, "../../etc/passwd");
    assert!(p.starts_with(c.join("eval").join(k.as_str())));
    assert_eq!(p.file_name().unwrap().len(), 64 + ".json".len());
}

#[tokio::test]
async fn dirty_and_clean_trees_give_different_keys() {
    let Some(r) = repo() else { return };
    let d = r.path();
    let clean = key(d).await.unwrap();
    assert_eq!(key(d).await.unwrap(), clean, "stable");

    fs::write(d.join("flake.nix"), "{ outputs = _: { x = 1; }; }\n").unwrap();
    let modified = key(d).await.unwrap();
    assert_ne!(modified, clean);

    // An untracked file changes nothing: a git flake never sees it (round 2 dropped the
    // untracked listing, which read exclude files whole).
    fs::write(d.join("new.nix"), "{}\n").unwrap();
    assert_eq!(key(d).await.unwrap(), modified);

    // Back to the commit: the clean key again.
    fs::remove_file(d.join("new.nix")).unwrap();
    git(d, &["checkout", "-q", "--", "flake.nix"]);
    assert_eq!(key(d).await.unwrap(), clean);

    // flake.lock is an input.
    fs::write(d.join("flake.lock"), "{\"nodes\":{}}\n").unwrap();
    git(d, &["commit", "-q", "-am", "lock"]);
    let relocked = key(d).await.unwrap();
    assert_ne!(relocked, clean);

    // An ignored file changes nothing.
    fs::write(d.join(".gitignore"), "result\n").unwrap();
    git(d, &["add", ".gitignore"]);
    git(d, &["commit", "-q", "-m", "ignore"]);
    let with_ignore = key(d).await.unwrap();
    fs::write(d.join("result"), "x").unwrap();
    assert_eq!(key(d).await.unwrap(), with_ignore);
}

#[tokio::test]
async fn a_directory_that_is_not_a_git_repository_gets_no_caching() {
    if !have_git() {
        return;
    }
    // The system temporary directory is not inside a repository, here or in the sandbox.
    let dir = tempdir();
    let e = key_for(dir.path(), &GitParams::new(), &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert!(e.starts_with("not a git repository"), "{e}");
}

#[tokio::test]
async fn a_missing_git_gets_no_caching() {
    let dir = tempdir();
    let mut g = GitParams::new();
    g.program = std::ffi::OsStr::new("auspex-test-no-such-git");
    let e = key_for(dir.path(), &g, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert!(e.contains("not found on PATH"), "{e}");
}

#[tokio::test]
async fn an_unwritable_cache_directory_is_reported_not_fatal() {
    if rustix::process::getuid().is_root() {
        eprintln!("SKIPPED: root can write anywhere");
        return;
    }
    let Some(r) = repo() else { return };
    let k = key(r.path()).await.unwrap();
    let cache = tempdir();
    fs::set_permissions(cache.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let e = store(cache.path(), &k, "terra", &success("terra", 1))
        .await
        .unwrap_err();
    assert!(e.starts_with("cannot write the eval cache"), "{e}");
    assert_eq!(lookup(cache.path(), &k, "terra").await, None);
    fs::set_permissions(cache.path(), fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn two_writers_at_once_leave_one_whole_entry() {
    let Some(r) = repo() else { return };
    let k = key(r.path()).await.unwrap();
    let cache = tempdir();
    let c = cache.path().to_path_buf();
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..32u8 {
        let c = c.clone();
        let k = k.clone();
        tasks.spawn(async move { store(&c, &k, "terra", &success("terra", i % 2)).await });
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap().unwrap();
    }
    let got = lookup(&c, &k, "terra").await.expect("a whole entry");
    assert!(got == success("terra", 0) || got == success("terra", 1));
    // No temporary files left behind.
    let dir = entry_path(&c, &k, "terra");
    let left: Vec<_> = fs::read_dir(dir.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
}

/// The cache in front of a fake evaluator: the second run on an unchanged tree reads the
/// successes from the cache and evaluates only what failed.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario, run after run against the same cache"
)]
async fn evaluate_cached_reads_hits_and_evaluates_misses() {
    let Some(t) = tools(&[]) else { return };
    let Some(r) = repo() else { return };
    let dir = tempdir();
    let drv = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-a.drv";
    let out = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-nixos-system-a";
    // Records each invocation's machine list (read from the `--arg-from-file` file) and prints a success for `a`
    // and for `1odd` (quoted in `attr`, as nix-eval-jobs prints a name that is not a bare
    // identifier) and an error for `b`, when asked for them.
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            r#"for a in "$@"; do if [ "$p" = machines ] && [ "$pp" = --arg-from-file ]; then f="$a"; fi; pp="$p"; p="$a"; done
IFS= read -r m < "$f" || true
printf '%s\n' "$m" >> '@dir@/calls'
case "$m" in *'"a"'*) printf '%s\n' '{{"attr":"a","attrPath":["a"],"drvPath":"{drv}","outputs":{{"out":"{out}"}}}}';; esac
case "$m" in *'"1odd"'*) printf '%s\n' '{{"attr":"\"1odd\"","attrPath":["1odd"],"drvPath":"{drv}","outputs":{{"out":"{out}"}}}}';; esac
case "$m" in *'"b"'*) printf '%s\n' '{{"attr":"b","attrPath":["b"],"error":"error: b is broken"}}';; esac"#
        ),
    );
    let cache_dir = tempdir();
    let cache = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: r.path(),
        git: GitParams::new(),
    };
    let mut eval = EvalParams::new();
    eval.runtime_dir = Some(common::runtime_dir());
    eval.program = prog.as_os_str();
    eval.timeout = Duration::from_secs(10);
    // A plain path: git+file, so cached (a `path:` reference is not).
    let flake = FlakeRef::new(r.path().to_str().unwrap()).unwrap();
    let machines: Vec<EvalMachine> = ["a", "1odd", "b", "a"]
        .iter()
        .map(|n| EvalMachine {
            name: (*n).to_owned(),
            class: MachineClass::Nixos,
        })
        .collect();

    let run = || async {
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
        while let Some(i) = rx.recv().await {
            items.push(i);
        }
        items
    };
    let summary = |items: &[EvalItem]| -> Vec<(String, bool, bool)> {
        items
            .iter()
            .filter_map(|i| match i {
                EvalItem::Machine {
                    machine,
                    outcome,
                    from_cache,
                } => Some((
                    machine.clone(),
                    matches!(outcome, EvalOutcome::Success(_)),
                    *from_cache,
                )),
                _ => None,
            })
            .collect()
    };

    let first = run().await;
    assert_eq!(
        summary(&first),
        [
            ("a".to_owned(), true, false),
            ("1odd".to_owned(), true, false),
            ("b".to_owned(), false, false)
        ]
    );
    assert_eq!(first.last(), Some(&EvalItem::Finished { error: None }));

    let second = run().await;
    assert_eq!(
        summary(&second),
        [
            ("a".to_owned(), true, true),
            ("1odd".to_owned(), true, true),
            ("b".to_owned(), false, false)
        ],
        "a and 1odd from the cache, b (an error, never cached) evaluated again"
    );
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    let calls: Vec<serde_json::Value> = calls
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        calls,
        [
            serde_json::json!([
                {"name": "a", "class": "nixos"},
                {"name": "1odd", "class": "nixos"},
                {"name": "b", "class": "nixos"},
            ]),
            serde_json::json!([{"name": "b", "class": "nixos"}]),
        ],
        "the second run asked the evaluator only for the miss"
    );

    // Everything cached: the evaluator is not run at all.
    let only_a = &machines[..1];
    let (tx, mut rx) = unbounded_channel();
    evaluate_cached(&flake, only_a, &eval, &cache, &CancellationToken::new(), tx)
        .await
        .unwrap();
    let mut items = Vec::new();
    while let Some(i) = rx.recv().await {
        items.push(i);
    }
    assert_eq!(summary(&items), [("a".to_owned(), true, true)]);
    assert_eq!(items.last(), Some(&EvalItem::Finished { error: None }));
    assert_eq!(
        fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );

    // An unwritable cache: results still arrive, with a note.
    if !rustix::process::getuid().is_root() {
        let ro = tempdir();
        fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let cache = CacheParams {
            cache_dir: Some(ro.path()),
            ..cache
        };
        let (tx, mut rx) = unbounded_channel();
        evaluate_cached(&flake, only_a, &eval, &cache, &CancellationToken::new(), tx)
            .await
            .unwrap();
        let mut items = Vec::new();
        while let Some(i) = rx.recv().await {
            items.push(i);
        }
        assert_eq!(summary(&items), [("a".to_owned(), true, false)]);
        assert!(
            items.iter().any(|i| matches!(i, EvalItem::Note(n) if n.starts_with("a: cannot write the eval cache"))),
            "{items:?}"
        );
        fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    // Not a git repository: a note, and everything evaluated.
    let plain = tempdir();
    let cache = CacheParams {
        cache_dir: Some(cache_dir.path()),
        flake_dir: plain.path(),
        git: GitParams::new(),
    };
    let (tx, mut rx) = unbounded_channel();
    evaluate_cached(&flake, only_a, &eval, &cache, &CancellationToken::new(), tx)
        .await
        .unwrap();
    let mut items = Vec::new();
    while let Some(i) = rx.recv().await {
        items.push(i);
    }
    assert!(
        matches!(&items[0], EvalItem::Note(n) if n.starts_with("eval cache off for this run: not a git repository")),
        "{items:?}"
    );
    assert_eq!(summary(&items), [("a".to_owned(), true, false)]);
}
