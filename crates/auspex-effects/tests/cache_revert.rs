//! Review finding N2: a change made and undone while the evaluator runs. The key after the
//! evaluation equals the key before it, so only the run guard (the `ctime` and identity
//! of everything the key looked at, and of `HEAD` and the index) can tell that the tree
//! the evaluator read was not the keyed one.
//!
//! Every test runs a fake nix-eval-jobs that prints machine `a`, waits until the test has
//! made the change, prints `b`, waits until the test has undone it, and prints `c`. The
//! results must all reach the caller, none may be stored, the note must say why, and the
//! next run, in the tree the first one started from, must not be served from the cache.
//! The repositories are temporary, with git's global and system config switched off.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use auspex_domain::fleet::MachineClass;
use auspex_effects::CancellationToken;
use auspex_effects::cache::{CacheParams, GitParams, entry_path, evaluate_cached, key_for};
use auspex_effects::nix::{EvalItem, EvalMachine, EvalParams, FlakeRef};
use common::{Tools, find_on_path, in_nix_build, tempdir, tools};
use tokio::sync::mpsc::unbounded_channel;

const MACHINES: [&str; 3] = ["a", "b", "c"];
const NOTE: &str = "eval results not cached: the worktree changed during evaluation";

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

struct Lab {
    _tools: Tools,
    repo: tempfile::TempDir,
    side: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Lab {
    /// A committed repository with `flake.nix`, `flake.lock`, `host.nix` and
    /// `sub/host.nix`, and the fake evaluator beside it.
    fn new() -> Option<Self> {
        let t = tools(&["sleep"])?;
        if find_on_path("git").is_none() {
            assert!(!in_nix_build(), "git is not on PATH inside the Nix build");
            eprintln!("SKIPPED: git is not on PATH");
            return None;
        }
        let repo = tempdir();
        let r = repo.path();
        fs::write(r.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        fs::write(r.join("flake.lock"), "{}\n").unwrap();
        fs::write(r.join("host.nix"), "1\n").unwrap();
        fs::create_dir(r.join("sub")).unwrap();
        fs::write(r.join("sub/host.nix"), "2\n").unwrap();
        git(r, &["init", "-q"]);
        git(r, &["add", "."]);
        git(r, &["commit", "-q", "-m", "one"]);
        let side = tempdir();
        let line = |m: &str| {
            format!(
                r#"{{"attr":"{m}","attrPath":["{m}"],"drvPath":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{m}.drv","outputs":{{"out":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{m}"}}}}"#
            )
        };
        let wait = |f: &str| format!("while [ ! -e '@dir@/{f}' ]; do @sleep@ 0.02; done");
        t.script(
            side.path(),
            "nix-eval-jobs",
            &format!(
                "printf '%s\\n' '{}'\n{}\nprintf '%s\\n' '{}'\n{}\nprintf '%s\\n' '{}'",
                line("a"),
                wait("changed"),
                line("b"),
                wait("undone"),
                line("c"),
            ),
        );
        Some(Self {
            _tools: t,
            repo,
            side,
            cache: tempdir(),
        })
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.repo.path().join(rel)
    }

    fn git(&self, args: &[&str]) {
        git(self.repo.path(), args);
    }

    /// One `evaluate_cached` run: `change` at the first result, `undo` at the second.
    async fn run(&self, change: impl FnOnce(), undo: impl FnOnce()) -> Vec<EvalItem> {
        let prog = self.side.path().join("nix-eval-jobs");
        let mut eval = EvalParams::new();
        eval.runtime_dir = Some(common::runtime_dir());
        eval.program = prog.as_os_str();
        eval.timeout = Duration::from_secs(20);
        let params = CacheParams {
            cache_dir: Some(self.cache.path()),
            flake_dir: self.repo.path(),
            git: GitParams::new(),
        };
        let flake = FlakeRef::new(self.repo.path().to_str().unwrap()).unwrap();
        let machines: Vec<_> = MACHINES
            .iter()
            .map(|n| EvalMachine {
                name: (*n).to_owned(),
                class: MachineClass::Nixos,
            })
            .collect();
        let (tx, mut rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let run = evaluate_cached(&flake, &machines, &eval, &params, &cancel, tx);
        let side = self.side.path();
        for marker in ["changed", "undone"] {
            let _ = fs::remove_file(side.join(marker));
        }
        let watch = async {
            let (mut change, mut undo) = (Some(change), Some(undo));
            let mut items = Vec::new();
            while let Some(i) = rx.recv().await {
                if let EvalItem::Machine { machine, .. } = &i {
                    if machine == "a" {
                        if let Some(f) = change.take() {
                            f();
                        }
                        fs::write(side.join("changed"), "").unwrap();
                    } else if machine == "b" {
                        if let Some(f) = undo.take() {
                            f();
                        }
                        fs::write(side.join("undone"), "").unwrap();
                    }
                }
                items.push(i);
            }
            items
        };
        let (res, items) = tokio::join!(run, watch);
        res.unwrap();
        items
    }

    async fn quiet_run(&self) -> Vec<EvalItem> {
        self.run(|| {}, || {}).await
    }

    /// How many machines have an entry under the tree's current key.
    async fn stored(&self) -> usize {
        let key = key_for(
            self.repo.path(),
            &GitParams::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        MACHINES
            .iter()
            .filter(|m| entry_path(self.cache.path(), &key, m).exists())
            .count()
    }

    /// The scenario: `change` and `undo` during one run store nothing, with the note, and
    /// the next run evaluates everything again.
    async fn refuses(&self, change: impl FnOnce(), undo: impl FnOnce()) {
        let key = key_for(
            self.repo.path(),
            &GitParams::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        let items = self.run(change, undo).await;
        assert_eq!(
            evaluated(&items),
            3,
            "every result reaches the caller: {items:?}"
        );
        assert!(has_note(&items), "{items:?}");
        assert_eq!(items.last(), Some(&EvalItem::Finished { error: None }));
        let after = key_for(
            self.repo.path(),
            &GitParams::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(after, key, "the test must undo its change exactly");
        assert_eq!(
            self.stored().await,
            0,
            "a result of the changed run was stored"
        );
        let next = self.quiet_run().await;
        assert_eq!(hits(&next), 0, "served from the cache: {next:?}");
    }
}

fn hits(items: &[EvalItem]) -> usize {
    items
        .iter()
        .filter(|i| {
            matches!(
                i,
                EvalItem::Machine {
                    from_cache: true,
                    ..
                }
            )
        })
        .count()
}

fn evaluated(items: &[EvalItem]) -> usize {
    items
        .iter()
        .filter(|i| {
            matches!(
                i,
                EvalItem::Machine {
                    from_cache: false,
                    ..
                }
            )
        })
        .count()
}

fn has_note(items: &[EvalItem]) -> bool {
    items
        .iter()
        .any(|i| matches!(i, EvalItem::Note(n) if n == NOTE))
}

/// The review's N2 scenario: a tracked file edited at the first result and its original
/// bytes written back at the second.
#[tokio::test]
async fn an_edit_undone_during_the_evaluation_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let f = lab.path("flake.nix");
    let original = fs::read(&f).unwrap();
    lab.refuses(
        || fs::write(&f, "{ outputs = _: { edited = 1; }; }\n").unwrap(),
        || fs::write(&f, &original).unwrap(),
    )
    .await;
}

/// `git stash` of a dirty tree, then `git stash pop`.
#[tokio::test]
async fn a_stash_and_pop_during_the_evaluation_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    fs::write(lab.path("host.nix"), "dirty\n").unwrap();
    lab.refuses(
        || lab.git(&["stash", "-q"]),
        || lab.git(&["stash", "pop", "-q"]),
    )
    .await;
}

/// `git checkout other`, then `git checkout -`.
#[tokio::test]
async fn a_checkout_there_and_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.git(&["checkout", "-q", "-b", "other"]);
    fs::write(lab.path("host.nix"), "other\n").unwrap();
    lab.git(&["commit", "-q", "-am", "other"]);
    lab.git(&["checkout", "-q", "-"]);
    lab.refuses(
        || lab.git(&["checkout", "-q", "other"]),
        || lab.git(&["checkout", "-q", "-"]),
    )
    .await;
}

/// `HEAD` alone moves: to a commit with the same tree (nix's `self.rev` differs) and back,
/// with neither the index nor any file written.
#[tokio::test]
async fn head_moved_and_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let head = fs::read_to_string(lab.path(".git/HEAD")).unwrap();
    let branch = head.trim().strip_prefix("ref: ").unwrap().to_owned();
    lab.git(&["checkout", "-q", "-b", "same-tree"]);
    lab.git(&["commit", "-q", "--allow-empty", "-m", "same tree"]);
    lab.git(&["checkout", "-q", "-"]);
    lab.refuses(
        || lab.git(&["symbolic-ref", "HEAD", "refs/heads/same-tree"]),
        || lab.git(&["symbolic-ref", "HEAD", &branch]),
    )
    .await;
}

/// The index alone changes: an untracked file is added (so nix reads it) and reset.
#[tokio::test]
async fn an_add_and_reset_of_an_untracked_file_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    fs::write(lab.path("extra.nix"), "extra\n").unwrap();
    lab.refuses(
        || lab.git(&["add", "extra.nix"]),
        || lab.git(&["reset", "-q", "--", "extra.nix"]),
    )
    .await;
}

/// A tracked file deleted and written again with the same bytes (a new inode, or the same
/// one reused, with a new `ctime` either way).
#[tokio::test]
async fn a_file_deleted_and_recreated_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let f = lab.path("host.nix");
    let original = fs::read(&f).unwrap();
    lab.refuses(
        || fs::remove_file(&f).unwrap(),
        || fs::write(&f, &original).unwrap(),
    )
    .await;
}

/// An editor's save: the same bytes written to a new file and renamed over the old one.
#[tokio::test]
async fn a_file_replaced_by_rename_with_the_same_bytes_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let f = lab.path("host.nix");
    let original = fs::read(&f).unwrap();
    let tmp = lab.path(".host.nix.tmp");
    lab.refuses(
        || {
            fs::write(&tmp, &original).unwrap();
            fs::rename(&tmp, &f).unwrap();
        },
        || {},
    )
    .await;
}

/// A tracked file that is missing from the worktree appears and goes again: there is no
/// file to stamp, so the directory that holds the name is stamped instead.
#[tokio::test]
async fn a_missing_file_that_appears_and_goes_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let f = lab.path("host.nix");
    fs::remove_file(&f).unwrap();
    lab.refuses(
        || fs::write(&f, "back\n").unwrap(),
        || fs::remove_file(&f).unwrap(),
    )
    .await;
}

/// The same with a missing directory on the way: the deepest directory reached.
#[tokio::test]
async fn a_missing_directory_that_appears_and_goes_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let d = lab.path("sub");
    fs::remove_dir_all(&d).unwrap();
    lab.refuses(
        || {
            fs::create_dir(&d).unwrap();
            fs::write(d.join("host.nix"), "back\n").unwrap();
        },
        || fs::remove_dir_all(&d).unwrap(),
    )
    .await;
}

/// A tracked symlink retargeted and put back.
#[tokio::test]
async fn a_symlink_retargeted_and_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let l = lab.path("link");
    symlink("host.nix", &l).unwrap();
    lab.git(&["add", "link"]);
    lab.git(&["commit", "-q", "-m", "link"]);
    lab.refuses(
        || {
            fs::remove_file(&l).unwrap();
            symlink("sub/host.nix", &l).unwrap();
        },
        || {
            fs::remove_file(&l).unwrap();
            symlink("host.nix", &l).unwrap();
        },
    )
    .await;
}

/// A `touch` (same bytes, a new mtime) moves `ctime` too, so that run is not cached: the
/// guard cannot tell it from a write and its revert. It is one run only: the next run is
/// stored and the one after hits, since `ctime` is not in the key.
#[tokio::test]
async fn a_touch_skips_one_store_and_no_more() {
    let Some(lab) = Lab::new() else { return };
    let f = lab.path("host.nix");
    let before = fs::symlink_metadata(&f).unwrap();
    lab.refuses(
        || {
            fs::File::options()
                .write(true)
                .open(&f)
                .unwrap()
                .set_modified(SystemTime::now() + Duration::from_secs(60))
                .unwrap();
        },
        || {},
    )
    .await;
    {
        use std::os::unix::fs::MetadataExt as _;
        let after = fs::symlink_metadata(&f).unwrap();
        assert_ne!(
            (after.ctime(), after.ctime_nsec()),
            (before.ctime(), before.ctime_nsec()),
            "touch moved ctime"
        );
    }
    // `refuses` ran one quiet run after the touched one: it stored everything.
    assert_eq!(lab.stored().await, 3);
    let third = lab.quiet_run().await;
    assert_eq!(hits(&third), 3, "{third:?}");
}

/// A tree nobody touches during the evaluation is stored, and the next run hits.
#[tokio::test]
async fn an_untouched_tree_is_stored_and_hits() {
    let Some(lab) = Lab::new() else { return };
    let first = lab.quiet_run().await;
    assert_eq!(evaluated(&first), 3, "{first:?}");
    assert!(!has_note(&first), "{first:?}");
    assert_eq!(lab.stored().await, 3);
    let second = lab.quiet_run().await;
    assert_eq!(hits(&second), 3, "{second:?}");
    assert_eq!(second.last(), Some(&EvalItem::Finished { error: None }));
}
