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
use std::path::{Path, PathBuf};
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
    _repo: tempfile::TempDir,
    /// The flake, and where the test's git commands run: the repository, or a linked
    /// worktree of it.
    dir: PathBuf,
    side: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Lab {
    /// A committed repository with `flake.nix`, `flake.lock`, `host.nix`, `sub/host.nix`
    /// and `a/b/host.nix`, and the fake evaluator beside it.
    fn new() -> Option<Self> {
        Self::with(&[], false)
    }

    /// [`Lab::new`] with extra `git init` arguments, and with the flake in a linked
    /// worktree (on its own branch `linked`) when `linked`.
    fn with(init: &[&str], linked: bool) -> Option<Self> {
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
        fs::create_dir_all(r.join("a/b")).unwrap();
        fs::write(r.join("a/b/host.nix"), "3\n").unwrap();
        git(r, &[&["init", "-q"][..], init].concat());
        git(r, &["add", "."]);
        git(r, &["commit", "-q", "-m", "one"]);
        let side = tempdir();
        let dir = if linked {
            let wt = side.path().join("wt");
            git(
                r,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "linked",
                    wt.to_str().unwrap(),
                ],
            );
            wt
        } else {
            r.to_path_buf()
        };
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
            _repo: repo,
            dir,
            side,
            cache: tempdir(),
        })
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn git(&self, args: &[&str]) {
        git(&self.dir, args);
    }

    async fn key(&self) -> auspex_domain::eval::CacheKey {
        key_for(&self.dir, &GitParams::new(), &CancellationToken::new())
            .await
            .unwrap()
            .unwrap()
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
            flake_dir: &self.dir,
            git: GitParams::new(),
        };
        let flake = FlakeRef::new(self.dir.to_str().unwrap()).unwrap();
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
        let key = key_for(&self.dir, &GitParams::new(), &CancellationToken::new())
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
        let key = key_for(&self.dir, &GitParams::new(), &CancellationToken::new())
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
        let after = key_for(&self.dir, &GitParams::new(), &CancellationToken::new())
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

// ---------------------------------------------------------------------------------------
// PR #9 fix round 2: the review of `ca2cc3f` (B1, F3, F4, F6)
// ---------------------------------------------------------------------------------------

fn rev(dir: &Path, what: &str) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["rev-parse", "--verify", what])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

impl Lab {
    /// A second commit on top of the first, writing `host.nix` (or nothing, `None`).
    fn second_commit(&self, host: Option<&str>) {
        match host {
            Some(text) => {
                fs::write(self.path("host.nix"), text).unwrap();
                self.git(&["commit", "-q", "-am", "two"]);
            }
            None => self.git(&["commit", "-q", "--allow-empty", "-m", "two"]),
        }
    }
}

/// Review B1, row 1: `reset --soft HEAD~` then `reset --soft ORIG_HEAD` moves only the
/// branch: neither `HEAD`, the index nor any file is written, yet nix's `self.rev` and
/// its dirty state differ in between.
#[tokio::test]
async fn a_soft_reset_and_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.second_commit(Some("two\n"));
    lab.refuses(
        || lab.git(&["reset", "-q", "--soft", "HEAD~"]),
        || lab.git(&["reset", "-q", "--soft", "ORIG_HEAD"]),
    )
    .await;
}

/// B1, row 2: the same with a parent of the same tree (only `self.rev` differs).
#[tokio::test]
async fn a_soft_reset_to_a_same_tree_parent_and_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.second_commit(None);
    lab.refuses(
        || lab.git(&["reset", "-q", "--soft", "HEAD~"]),
        || lab.git(&["reset", "-q", "--soft", "ORIG_HEAD"]),
    )
    .await;
}

/// B1, row 3: `commit --amend -m` then `reset --soft HEAD@{1}`.
#[tokio::test]
async fn an_amend_and_reset_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.refuses(
        || lab.git(&["commit", "-q", "--amend", "-m", "better message"]),
        || lab.git(&["reset", "-q", "--soft", "HEAD@{1}"]),
    )
    .await;
}

/// B1, row 4: `commit --allow-empty` then `reset --soft HEAD~`.
#[tokio::test]
async fn an_empty_commit_and_reset_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.refuses(
        || lab.git(&["commit", "-q", "--allow-empty", "-m", "empty"]),
        || lab.git(&["reset", "-q", "--soft", "HEAD~"]),
    )
    .await;
}

/// B1, row 5: `update-ref` of the branch there and back.
#[tokio::test]
async fn an_update_ref_there_and_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.second_commit(Some("two\n"));
    let (now, parent) = (rev(&lab.dir, "HEAD"), rev(&lab.dir, "HEAD~"));
    let branch = rev_name(&lab.dir);
    lab.refuses(
        || lab.git(&["update-ref", &branch, &parent]),
        || lab.git(&["update-ref", &branch, &now]),
    )
    .await;
}

fn rev_name(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["symbolic-ref", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// B1 with the branch packed (`pack-refs --all`, as `git gc` does): no loose ref file at
/// the first key, so its directory and `packed-refs` are what change.
#[tokio::test]
async fn a_soft_reset_and_back_of_a_packed_branch_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.second_commit(Some("two\n"));
    lab.git(&["pack-refs", "--all"]);
    lab.refuses(
        || lab.git(&["reset", "-q", "--soft", "HEAD~"]),
        || lab.git(&["reset", "-q", "--soft", "ORIG_HEAD"]),
    )
    .await;
}

/// B1 in a linked worktree: its branch lives in the common git directory, not in the
/// worktree's own.
#[tokio::test]
async fn a_soft_reset_and_back_in_a_linked_worktree_stores_nothing() {
    let Some(lab) = Lab::with(&[], true) else {
        return;
    };
    lab.second_commit(Some("two\n"));
    lab.refuses(
        || lab.git(&["reset", "-q", "--soft", "HEAD~"]),
        || lab.git(&["reset", "-q", "--soft", "ORIG_HEAD"]),
    )
    .await;
}

/// B1 with the reftable ref backend: no loose refs at all; `reftable/tables.list` is
/// rewritten by every ref update.
#[tokio::test]
async fn a_soft_reset_and_back_with_reftable_stores_nothing() {
    let Some(lab) = Lab::with(&["--ref-format=reftable"], false) else {
        return;
    };
    lab.second_commit(Some("two\n"));
    lab.refuses(
        || lab.git(&["reset", "-q", "--soft", "HEAD~"]),
        || lab.git(&["reset", "-q", "--soft", "ORIG_HEAD"]),
    )
    .await;
}

/// B1 with a detached `HEAD`: the commit id is in the `HEAD` file itself.
#[tokio::test]
async fn a_detached_head_moved_and_back_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.second_commit(Some("two\n"));
    lab.git(&["checkout", "-q", "--detach"]);
    lab.refuses(
        || lab.git(&["reset", "-q", "--soft", "HEAD~"]),
        || lab.git(&["reset", "-q", "--soft", "ORIG_HEAD"]),
    )
    .await;
}

/// Review F4 (mutation M16): a change the key sees and that stays. HEAD moves by an empty
/// commit during the scan: the two keys differ, so nothing is stored under either.
#[tokio::test]
async fn a_head_that_moves_and_stays_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let before = lab.key().await;
    let items = lab
        .run(
            || lab.git(&["commit", "-q", "--allow-empty", "-m", "stays"]),
            || {},
        )
        .await;
    assert_eq!(evaluated(&items), 3, "{items:?}");
    assert!(has_note(&items), "{items:?}");
    let after = lab.key().await;
    assert_ne!(after, before, "the key must see the move");
    for key in [&before, &after] {
        let stored = MACHINES
            .iter()
            .filter(|m| entry_path(lab.cache.path(), key, m).exists())
            .count();
        assert_eq!(stored, 0, "stored under {key:?}");
    }
}

/// Review F3: a tracked file that cannot be opened at either key (`chmod 000`) is made
/// readable, edited, reverted and made unreadable again during the scan. Its own `lstat`
/// stamp moves; its directory's does not.
#[tokio::test]
async fn an_unreadable_file_opened_and_closed_again_stores_nothing() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(lab) = Lab::new() else { return };
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: root can read a mode 000 file");
        return;
    }
    let f = lab.path("host.nix");
    let original = fs::read(&f).unwrap();
    let mode = |m: u32| fs::set_permissions(&f, fs::Permissions::from_mode(m)).unwrap();
    mode(0o000);
    lab.refuses(
        || {
            mode(0o644);
            fs::write(&f, "edited\n").unwrap();
        },
        || {
            fs::write(&f, &original).unwrap();
            mode(0o000);
        },
    )
    .await;
    mode(0o644);
}

/// Review F4 (mutation M06): the guard rests on `ctime`, which no unprivileged call sets
/// back. An edit in place reverted with its old mtime put back (`touch -r`, or what
/// `cp --preserve=all` does) leaves bytes, size and mtime as they were.
#[tokio::test]
async fn an_edit_reverted_with_its_old_mtime_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let f = lab.path("host.nix");
    let original = fs::read(&f).unwrap();
    let mtime = fs::metadata(&f).unwrap().modified().unwrap();
    lab.refuses(
        || fs::write(&f, "9\n").unwrap(),
        || {
            fs::write(&f, &original).unwrap();
            fs::File::options()
                .write(true)
                .open(&f)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        },
    )
    .await;
}

/// Review F4 (mutation M11): a missing directory below an existing one. The deepest
/// directory reached is `a`, not the top.
#[tokio::test]
async fn a_deeper_missing_directory_that_appears_and_goes_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let d = lab.path("a/b");
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

/// Review F6: git needs only search permission on its directory, and so does the guard.
/// With `.git` at mode 0300 the key is computed and an untouched run is stored.
#[tokio::test]
async fn an_unlistable_git_directory_still_caches() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(lab) = Lab::new() else { return };
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: root ignores directory modes");
        return;
    }
    let g = lab.path(".git");
    fs::set_permissions(&g, fs::Permissions::from_mode(0o300)).unwrap();
    let first = lab.quiet_run().await;
    let stored = lab.stored().await;
    fs::set_permissions(&g, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !first.iter().any(|i| matches!(i, EvalItem::Note(_))),
        "{first:?}"
    );
    assert_eq!(stored, 3, "{first:?}");
}

/// B1 with the ref packed at both keys: moved and packed again, then moved back and
/// packed again. No loose ref file exists at either key; `packed-refs` was rewritten.
#[tokio::test]
async fn a_packed_ref_moved_and_back_and_repacked_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.second_commit(Some("two\n"));
    lab.git(&["pack-refs", "--all"]);
    let (now, parent) = (rev(&lab.dir, "HEAD"), rev(&lab.dir, "HEAD~"));
    let branch = rev_name(&lab.dir);
    lab.refuses(
        || {
            lab.git(&["update-ref", &branch, &parent]);
            lab.git(&["pack-refs", "--all"]);
        },
        || {
            lab.git(&["update-ref", &branch, &now]);
            lab.git(&["pack-refs", "--all"]);
        },
    )
    .await;
}

/// Review of PR #8, N1: a git setting that the key covers (`core.fileMode`) changed at the
/// first result and put back at the second leaves two equal keys; the repository's `config`
/// file is in the run guard, so nothing is stored.
#[tokio::test]
async fn a_git_setting_changed_and_put_back_during_the_evaluation_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    lab.refuses(
        || lab.git(&["config", "core.fileMode", "false"]),
        || lab.git(&["config", "--unset", "core.fileMode"]),
    )
    .await;
}

/// Review of PR #37, finding 1: an attributes file outside the tree written at the first
/// result and removed at the second leaves two equal keys; its stamp is in the run guard.
#[tokio::test]
async fn an_attributes_file_added_and_removed_during_the_evaluation_stores_nothing() {
    let Some(lab) = Lab::new() else { return };
    let info = lab.path(".git/info/attributes");
    fs::create_dir_all(info.parent().unwrap()).unwrap();
    lab.refuses(
        || fs::write(&info, "* text=auto\n").unwrap(),
        || fs::remove_file(&info).unwrap(),
    )
    .await;
}
