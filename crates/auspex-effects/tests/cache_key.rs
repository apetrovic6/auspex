//! The eval cache key against review round 2: what git may read (N1), what the key must
//! tell apart (N2a to N2e), and the phases a cancel can arrive in (N6).
//!
//! Every test that plants a large file runs git through a wrapper under `ulimit -v 2000000`
//! (2 GB of address space): a safety net, so a regression fails the test (git runs out of
//! memory, or the test sees the wrong error) instead of eating the machine. The planted
//! files are sparse and cost nothing on disk.

#![cfg(unix)]
#![allow(
    clippy::many_single_char_names,
    reason = "t (tools), r (repo), g (git params), k (key), w (wrapper), as in the other test files"
)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use auspex_domain::eval::CacheKey;
use auspex_domain::fleet::MachineClass;
use auspex_effects::cache::{CacheParams, GitParams, evaluate_cached, key_for};
use auspex_effects::nix::{EvalItem, EvalMachine, EvalParams, FlakeRef};
use auspex_effects::{CancellationToken, Cancelled};
use common::{Tools, find_on_path, in_nix_build, still_alive_after_wait, tempdir, tools};
use tokio::sync::mpsc::unbounded_channel;

fn have_git() -> bool {
    if find_on_path("git").is_some() {
        return true;
    }
    assert!(!in_nix_build(), "git is not on PATH inside the Nix build");
    eprintln!("SKIPPED: git is not on PATH");
    false
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

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

async fn key(dir: &Path) -> Result<CacheKey, String> {
    key_for(dir, &GitParams::new(), &CancellationToken::new())
        .await
        .unwrap()
}

/// git under a 2 GB address-space limit, recording each invocation's pid in `<dir>/pids`.
fn wrapped_git(t: &Tools, dir: &Path) -> PathBuf {
    t.script(
        dir,
        "git",
        "echo $$ >> '@dir@/pids'\nulimit -v 2000000\nexec @git@ \"$@\"",
    )
}

async fn key_wrapped(
    t: &Tools,
    r: &Path,
    timeout: Duration,
) -> (Result<CacheKey, String>, Duration) {
    let side = tempdir();
    let w = wrapped_git(t, side.path());
    let mut g = GitParams::new();
    g.program = w.as_os_str();
    g.timeout = timeout;
    let start = Instant::now();
    let k = key_for(r, &g, &CancellationToken::new()).await.unwrap();
    (k, start.elapsed())
}

fn sparse(path: &Path, len: u64) {
    fs::File::create(path).unwrap().set_len(len).unwrap();
}

fn fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------------------
// N1: nothing git reads may cost memory or time
// ---------------------------------------------------------------------------------------

/// Untracked and ignored files never reach a git flake, so the key does not list them, and
/// no `.gitignore` or exclude file is read at all: a 600 MiB sparse one, at the root, in a
/// subdirectory, or as `.git/info/exclude`, changes nothing and costs nothing.
#[tokio::test]
async fn exclude_files_are_never_read() {
    let Some(t) = tools(&["git"]) else { return };
    let Some(r) = repo() else { return };
    let (before, _) = key_wrapped(&t, r.path(), Duration::from_secs(5)).await;
    let before = before.unwrap();
    sparse(&r.path().join(".gitignore"), 600 << 20);
    fs::create_dir(r.path().join("sub")).unwrap();
    sparse(&r.path().join("sub/.gitignore"), 600 << 20);
    sparse(&r.path().join(".git/info/exclude"), 600 << 20);
    let (k, took) = key_wrapped(&t, r.path(), Duration::from_secs(5)).await;
    assert_eq!(k, Ok(before), "untracked files are not in the key");
    assert!(took < Duration::from_secs(2), "{took:?}");
}

#[tokio::test]
async fn fifo_exclude_files_do_not_stall() {
    let Some(t) = tools(&["git"]) else { return };
    let Some(r) = repo() else { return };
    fs::create_dir(r.path().join("sub")).unwrap();
    fifo(&r.path().join("sub/.gitignore"));
    fs::remove_file(r.path().join(".git/info/exclude")).ok();
    fs::create_dir_all(r.path().join(".git/info")).unwrap();
    fifo(&r.path().join(".git/info/exclude"));
    let (k, took) = key_wrapped(&t, r.path(), Duration::from_secs(3)).await;
    assert!(k.is_ok(), "{k:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// The files git does read (the index, `HEAD`, `packed-refs`) are bounded by the memory
/// limit auspex puts on git: an oversized one is "no caching", saying so, not gigabytes.
#[tokio::test]
async fn oversized_git_files_hit_the_memory_limit() {
    let Some(t) = tools(&["git"]) else { return };
    // A 400 MiB index whose header claims 2^24 entries; the rest is zero, and a zero
    // trailer skips git's checksum. Unbounded, git needs about 1.2 GB for it.
    let Some(r) = repo() else { return };
    let index = r.path().join(".git/index");
    let mut header = b"DIRC".to_vec();
    header.extend_from_slice(&2u32.to_be_bytes());
    header.extend_from_slice(&(1u32 << 24).to_be_bytes());
    fs::write(&index, &header).unwrap();
    fs::File::options()
        .write(true)
        .open(&index)
        .unwrap()
        .set_len(400 << 20)
        .unwrap();
    let (k, _) = key_wrapped(&t, r.path(), Duration::from_secs(10)).await;
    let e = k.unwrap_err();
    assert!(e.contains("memory limit"), "index: {e}");

    // HEAD keeps its valid first line, so repository discovery (which reads only the
    // start of HEAD) accepts it, and `rev-parse --verify HEAD` then reads it whole.
    for name in ["HEAD", "packed-refs"] {
        let Some(r) = repo() else { return };
        let path = r.path().join(".git").join(name);
        let first = fs::read_to_string(&path).unwrap_or_default();
        let first = first.lines().next().unwrap_or("").to_owned();
        fs::write(&path, format!("{first}\n")).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(800 << 20)
            .unwrap();
        let (k, _) = key_wrapped(&t, r.path(), Duration::from_secs(10)).await;
        let e = k.unwrap_err();
        assert!(e.contains("memory limit"), "{name}: {e}");
    }
}

/// An `include.path` that is a FIFO stalls every git command: it costs one short timeout
/// (the first command), not one per command, and is "no caching" with a note.
#[tokio::test]
async fn a_fifo_include_costs_one_short_timeout() {
    let Some(t) = tools(&["git"]) else { return };
    let Some(r) = repo() else { return };
    let side = tempdir();
    let f = side.path().join("inc");
    fifo(&f);
    git(r.path(), &["config", "include.path", f.to_str().unwrap()]);
    let (k, took) = key_wrapped(&t, r.path(), Duration::from_millis(800)).await;
    let e = k.unwrap_err();
    assert!(e.contains("timed out"), "{e}");
    assert!(
        took < Duration::from_millis(1500),
        "more than one timeout: {took:?}"
    );
}

/// The limit is on git from its first instruction: it is set by util-linux `prlimit`, which
/// then execs git, so no window exists in which git runs unbounded (review finding F4).
/// A wrapper reports the soft and hard limits before anything else, then execs git.
#[tokio::test]
async fn git_observes_both_limits_at_its_first_instruction() {
    let Some(t) = tools(&["git", "prlimit"]) else {
        return;
    };
    let Some(r) = repo() else { return };
    let side = tempdir();
    let program = t.script(
        side.path(),
        "git",
        "ulimit -Sv >> '@dir@/limits'\nulimit -Hv >> '@dir@/limits'\nexec @git@ \"$@\"",
    );
    let mut params = GitParams::new();
    params.program = program.as_os_str();
    key_for(r.path(), &params, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    let observed = fs::read_to_string(side.path().join("limits")).unwrap();
    let observed: Vec<u64> = observed.lines().map(|line| line.parse().unwrap()).collect();
    assert!(observed.len() >= 2, "{observed:?}");
    assert!(
        observed
            .iter()
            .all(|&kib| kib * 1024 == auspex_effects::cache::GIT_MEMORY_LIMIT),
        "{observed:?}"
    );
}

/// git's leftover children are killed when it exits, as for nix (review mutation R26).
#[tokio::test]
async fn a_finished_git_leaves_no_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let Some(r) = repo() else { return };
    let dir = tempdir();
    let fake = t.script(
        dir.path(),
        "git",
        "@sleep@ 30 > /dev/null 2>&1 &\necho $! >> '@dir@/pids'\nexit 1",
    );
    let mut g = GitParams::new();
    g.program = fake.as_os_str();
    let _ = key_for(r.path(), &g, &CancellationToken::new()).await;
    let pids = common::wait_for_pids(&dir.path().join("pids"), 1);
    let left = still_alive_after_wait(&pids);
    common::cleanup(&pids);
    assert!(left.is_empty(), "git's child outlived it: {left:?}");
}

// ---------------------------------------------------------------------------------------
// N2: what the key must tell apart
// ---------------------------------------------------------------------------------------

/// N2a: git and nix use the owner exec bit only.
#[tokio::test]
async fn the_owner_exec_bit_is_keyed() {
    let Some(r) = repo() else { return };
    let f = r.path().join("tool.sh");
    fs::write(&f, "echo hi\n").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
    git(r.path(), &["add", "tool.sh"]);
    git(r.path(), &["commit", "-q", "-m", "tool"]);
    let before = key(r.path()).await.unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o655)).unwrap();
    assert_ne!(before, key(r.path()).await.unwrap(), "u-x with g+x, o+x");
    // Group and other bits alone do not count.
    fs::set_permissions(&f, fs::Permissions::from_mode(0o744)).unwrap();
    assert_eq!(before, key(r.path()).await.unwrap());
}

/// N2b: a dirty tree whose bytes equal the commit (`MM`) evaluates with `self.dirtyRev`,
/// so it must not share the clean tree's key.
#[tokio::test]
async fn a_dirty_tree_with_the_committed_bytes_is_not_the_clean_tree() {
    let Some(r) = repo() else { return };
    let clean = key(r.path()).await.unwrap();
    let f = r.path().join("flake.nix");
    let committed = fs::read(&f).unwrap();
    fs::write(&f, "{ outputs = _: { x = 1; }; }\n").unwrap();
    git(r.path(), &["add", "flake.nix"]);
    fs::write(&f, committed).unwrap();
    assert_ne!(clean, key(r.path()).await.unwrap());
}

/// N2c: a submodule's worktree is outside the key, so a tracked gitlink means no caching.
#[tokio::test]
async fn a_tracked_gitlink_means_no_caching() {
    let Some(r) = repo() else { return };
    let head = git(r.path(), &["rev-parse", "HEAD"]);
    git(
        r.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendored", head.trim()),
        ],
    );
    let e = key(r.path()).await.unwrap_err();
    assert!(e.contains("submodule"), "{e}");
}

/// N2e: two flakes in one repository do not share a key.
#[tokio::test]
async fn two_flakes_in_one_repository_have_different_keys() {
    let Some(r) = repo() else { return };
    for d in ["prod", "staging"] {
        fs::create_dir(r.path().join(d)).unwrap();
        fs::write(r.path().join(d).join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        fs::write(r.path().join(d).join("flake.lock"), "{}\n").unwrap();
    }
    git(r.path(), &["add", "."]);
    git(r.path(), &["commit", "-q", "-m", "two"]);
    assert_ne!(
        key(&r.path().join("prod")).await.unwrap(),
        key(&r.path().join("staging")).await.unwrap()
    );
}

/// Review mutation R02: a rename with the same content changes the key.
#[tokio::test]
async fn a_rename_changes_the_key() {
    let Some(r) = repo() else { return };
    fs::write(r.path().join("a.nix"), "1\n").unwrap();
    git(r.path(), &["add", "a.nix"]);
    git(r.path(), &["commit", "-q", "-m", "a"]);
    let before = key(r.path()).await.unwrap();
    git(r.path(), &["mv", "a.nix", "b.nix"]);
    assert_ne!(before, key(r.path()).await.unwrap());
}

/// Review mutation R30: a deleted tracked file is not the same as an emptied one.
#[tokio::test]
async fn deleted_and_emptied_files_differ() {
    let Some(r) = repo() else { return };
    fs::write(r.path().join("a.nix"), "1\n").unwrap();
    git(r.path(), &["add", "a.nix"]);
    git(r.path(), &["commit", "-q", "-m", "a"]);
    fs::write(r.path().join("a.nix"), "").unwrap();
    let emptied = key(r.path()).await.unwrap();
    fs::remove_file(r.path().join("a.nix")).unwrap();
    assert_ne!(emptied, key(r.path()).await.unwrap());
}

/// Review finding F6: the index listing is capped at 64 MiB, exactly. A git whose
/// `ls-files` prints one byte more is "no caching"; at the cap it is read (all NULs: no
/// records, so a key).
///
/// Whether the exit is seen before the last bytes are read depends on the machine (the
/// forge runner saw it, this machine did not); both orders must give the same answer, and
/// the unit tests in `cache.rs` force the late order. Each git command gets 60 s, not the
/// default 2 s: piping 64 MiB through a fake on a loaded runner is not what the default is
/// sized for, and a timeout here would hide which answer the cap gave.
#[tokio::test]
async fn the_index_listing_cap_is_64_mib() {
    let Some(t) = tools(&["git", "head"]) else {
        return;
    };
    let Some(r) = repo() else { return };
    let side = tempdir();
    for (len, over) in [(64 << 20, false), ((64 << 20) + 1, true)] {
        let fake = t.script(
            side.path(),
            &format!("git{len}"),
            &format!(
                "case \" $* \" in *' ls-files '*) exec @head@ -c {len} /dev/zero;; esac\nexec @git@ \"$@\""
            ),
        );
        let mut g = GitParams::new();
        g.program = fake.as_os_str();
        g.timeout = Duration::from_secs(60);
        let start = Instant::now();
        let k = key_for(r.path(), &g, &CancellationToken::new())
            .await
            .unwrap();
        eprintln!("{len} bytes: {:?}", start.elapsed());
        let printed = format!("git ls-files printed more than {} bytes", 64 << 20);
        match k {
            Err(e) => assert!(over && e == printed, "{len}: {e}"),
            Ok(_) => assert!(!over, "{len} bytes were accepted"),
        }
    }
}

/// Review mutation R09: an inherited `GIT_DIR` or `GIT_INDEX_FILE` does not redirect git.
/// Run in a child test process, since the variables must be in this crate's own
/// environment and `set_var` is `unsafe`.
#[test]
fn inherited_git_variables_do_not_redirect_git() {
    if std::env::var_os("AUSPEX_TEST_GITDIR_INNER").is_some() {
        return;
    }
    if !have_git() {
        return;
    }
    let other = repo().unwrap();
    fs::write(
        other.path().join("flake.nix"),
        "{ outputs = _: { other = 1; }; }\n",
    )
    .unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "git_dir_inner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AUSPEX_TEST_GITDIR_INNER", "1")
        .env("GIT_DIR", other.path().join(".git"))
        .env("GIT_WORK_TREE", other.path())
        .env("GIT_INDEX_FILE", other.path().join(".git/index"))
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
async fn git_dir_inner() {
    if std::env::var_os("AUSPEX_TEST_GITDIR_INNER").is_none() {
        return;
    }
    // Two repositories with the same commit content: if GIT_DIR redirected git, the key
    // of `mine` would be computed from the other repository's worktree.
    let mine = tempdir();
    fs::write(mine.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    fs::write(mine.path().join("flake.lock"), "{}\n").unwrap();
    let clean_env = |c: &mut Command| {
        c.env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
    };
    for args in [
        &["init", "-q"][..],
        &["add", "flake.nix", "flake.lock"],
        &["commit", "-q", "-m", "one"],
    ] {
        let mut c = Command::new("git");
        clean_env(&mut c);
        let ok = c
            .current_dir(mine.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=a", "-c", "user.email=a@invalid"])
            .args(args)
            .status()
            .unwrap();
        assert!(ok.success());
    }
    let with_env = key(mine.path()).await.unwrap();
    // Change `mine`; the key must follow it, not the other repository.
    fs::write(
        mine.path().join("flake.nix"),
        "{ outputs = _: { mine = 2; }; }\n",
    )
    .unwrap();
    assert_ne!(with_env, key(mine.path()).await.unwrap());
    let top = key_for(mine.path(), &GitParams::new(), &CancellationToken::new()).await;
    assert!(matches!(top, Ok(Ok(_))));
}

// ---------------------------------------------------------------------------------------
// N2d and N6: references and cancellation in the key phase
// ---------------------------------------------------------------------------------------

async fn collect(rx: &mut tokio::sync::mpsc::UnboundedReceiver<EvalItem>) -> Vec<EvalItem> {
    let mut v = Vec::new();
    while let Some(i) = rx.recv().await {
        v.push(i);
    }
    v
}

/// N2d: a `git+file:` reference with a query (`?ref=`, `?rev=`, `?dir=`) reads something
/// other than the worktree, so it is not cached.
#[tokio::test]
async fn a_git_reference_with_a_query_gets_no_caching() {
    let Some(r) = repo() else { return };
    let cache = tempdir();
    let mut eval = EvalParams::new();
    eval.runtime_dir = Some(common::runtime_dir());
    eval.program = std::ffi::OsStr::new("/nonexistent");
    let params = CacheParams {
        cache_dir: Some(cache.path()),
        flake_dir: r.path(),
        git: GitParams::new(),
    };
    for q in [
        "?ref=main",
        "?rev=0000000000000000000000000000000000000000",
        "?dir=sub",
    ] {
        let flake = FlakeRef::new(&format!("git+file://{}{q}", r.path().display())).unwrap();
        let (tx, mut rx) = unbounded_channel();
        evaluate_cached(
            &flake,
            &[EvalMachine {
                name: "a".to_owned(),
                class: MachineClass::Nixos,
            }],
            &eval,
            &params,
            &CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
        let items = collect(&mut rx).await;
        assert!(
            matches!(&items[0], EvalItem::Note(n) if n.contains("query")),
            "{q}: {items:?}"
        );
    }
    let plain = FlakeRef::new(&format!("git+file://{}", r.path().display())).unwrap();
    assert!(plain.is_git());
}

/// N6: the receiver dropped while git is computing the key: the call returns only once
/// git is reaped, and nothing more runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_receiver_during_the_key_kills_git_first() {
    let Some(t) = tools(&["sleep"]) else { return };
    let Some(r) = repo() else { return };
    let side = tempdir();
    let w = t.script(
        side.path(),
        "git",
        "echo $$ >> '@dir@/pids'\nexec @sleep@ 30",
    );
    let cache = tempdir();
    let (tx, rx) = unbounded_channel();
    let (rp, cp, wp) = (
        r.path().to_path_buf(),
        cache.path().to_path_buf(),
        w.clone(),
    );
    let task = tokio::spawn(async move {
        let mut g = GitParams::new();
        g.program = wp.as_os_str();
        g.timeout = Duration::from_secs(30);
        let params = CacheParams {
            cache_dir: Some(&cp),
            flake_dir: &rp,
            git: g,
        };
        let flake = FlakeRef::new(rp.to_str().unwrap()).unwrap();
        let mut eval = EvalParams::new();
        eval.runtime_dir = Some(common::runtime_dir());
        eval.program = std::ffi::OsStr::new("/nonexistent");
        evaluate_cached(
            &flake,
            &[EvalMachine {
                name: "a".to_owned(),
                class: MachineClass::Nixos,
            }],
            &eval,
            &params,
            &CancellationToken::new(),
            tx,
        )
        .await
    });
    let pids_file = side.path().join("pids");
    let pids = tokio::task::spawn_blocking(move || common::wait_for_pids(&pids_file, 1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(rx);
    let r = tokio::time::timeout(Duration::from_secs(3), task).await;
    let present = common::pid_exists(pids[0]);
    common::cleanup(&pids);
    assert_eq!(
        r.expect("the dropped receiver was ignored").unwrap(),
        Err(Cancelled)
    );
    assert!(!present, "git was not reaped before the call returned");
}

/// Nix can change its dirty-tree decision when these flags change, independently
/// of the blob ids and worktree bytes.
#[tokio::test]
async fn index_stat_flags_are_part_of_the_key() {
    let Some(r) = repo() else { return };
    fs::write(
        r.path().join("flake.nix"),
        "{ outputs = _: { changed = true; }; }\n",
    )
    .unwrap();
    let ordinary = key(r.path()).await.unwrap();
    git(
        r.path(),
        &["update-index", "--assume-unchanged", "flake.nix"],
    );
    let assumed = key(r.path()).await.unwrap();
    assert_ne!(ordinary, assumed);
    git(
        r.path(),
        &["update-index", "--no-assume-unchanged", "flake.nix"],
    );
    assert_eq!(key(r.path()).await.unwrap(), ordinary);
    git(r.path(), &["update-index", "--skip-worktree", "flake.nix"]);
    assert_ne!(key(r.path()).await.unwrap(), ordinary);
    git(
        r.path(),
        &["update-index", "--no-skip-worktree", "flake.nix"],
    );
    assert_eq!(key(r.path()).await.unwrap(), ordinary);
}

#[tokio::test]
async fn linked_worktrees_are_keyed_from_their_own_index_and_files() {
    let Some(r) = repo() else { return };
    let side = tempdir();
    let tree = side.path().join("checkout");
    git(
        r.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            tree.to_str().unwrap(),
            "HEAD",
        ],
    );
    let original = key(r.path()).await.unwrap();
    assert_eq!(key(&tree).await.unwrap(), original);
    fs::write(
        tree.join("flake.nix"),
        "{ outputs = _: { changed = true; }; }\n",
    )
    .unwrap();
    assert_ne!(key(&tree).await.unwrap(), original);
    assert_eq!(key(r.path()).await.unwrap(), original);
}

/// Review finding B1 and F9: the reference nix evaluates (built from the nearest ancestor
/// with a `.git`) and the key (from git's own discovery) must name the same repository. A
/// `sub/.git` that is not a repository makes them differ: git looks past it, `FlakeRef`
/// does not. That is no caching, never a key for one tree serving the other.
#[tokio::test]
async fn a_reference_and_a_key_for_different_repositories_get_no_caching() {
    let Some(r) = repo() else { return };
    let sub = r.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    git(r.path(), &["add", "sub/flake.nix"]);
    git(r.path(), &["commit", "-q", "-m", "sub"]);
    fs::create_dir(sub.join(".git")).unwrap();
    let flake = FlakeRef::new(sub.to_str().unwrap()).unwrap();
    assert!(flake.is_git(), "{flake:?}");
    // git itself still finds the outer repository.
    assert!(key(&sub).await.is_ok());
    let cache = tempdir();
    let mut eval = EvalParams::new();
    eval.runtime_dir = Some(common::runtime_dir());
    eval.program = std::ffi::OsStr::new("/nonexistent");
    let params = CacheParams {
        cache_dir: Some(cache.path()),
        flake_dir: &sub,
        git: GitParams::new(),
    };
    let (tx, mut rx) = unbounded_channel();
    evaluate_cached(
        &flake,
        &[EvalMachine {
            name: "a".to_owned(),
            class: MachineClass::Nixos,
        }],
        &eval,
        &params,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let items = collect(&mut rx).await;
    assert!(
        matches!(&items[0], EvalItem::Note(n) if n.contains("top level")),
        "{items:?}"
    );
}

/// Without util-linux `prlimit` on `PATH`, git is never run (it would run unbounded): no
/// caching, with a note naming the launcher. In a child process whose `PATH` holds only a
/// recording `git`.
#[test]
fn without_prlimit_git_never_runs() {
    if std::env::var_os("AUSPEX_TEST_NO_PRLIMIT").is_some() {
        return;
    }
    let Some(t) = tools(&["git"]) else { return };
    let bin = tempdir();
    t.script(
        bin.path(),
        "git",
        "echo ran >> '@dir@/ran'\nexec @git@ \"$@\"",
    );
    let Some(r) = repo() else { return };
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "no_prlimit_inner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AUSPEX_TEST_NO_PRLIMIT", r.path())
        .env("PATH", bin.path())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
    assert!(
        !bin.path().join("ran").exists(),
        "git ran without its memory limit"
    );
}

#[tokio::test]
async fn no_prlimit_inner() {
    let Some(repo) = std::env::var_os("AUSPEX_TEST_NO_PRLIMIT") else {
        return;
    };
    let e = key(Path::new(&repo)).await.unwrap_err();
    assert!(e.contains("prlimit"), "{e}");
}

/// Review F4 (mutation M17): a git directory whose path contains a newline. `rev-parse`
/// prints it before HEAD's id, so only the last line is the id.
#[tokio::test]
async fn a_git_directory_path_with_a_newline_is_keyed() {
    if !have_git() {
        return;
    }
    let base = tempdir();
    let r = base.path().join("fleet\nrepo");
    fs::create_dir(&r).unwrap();
    fs::write(r.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    git(&r, &["init", "-q"]);
    git(&r, &["add", "flake.nix"]);
    git(&r, &["commit", "-q", "-m", "one"]);
    let k = key(&r).await;
    assert!(k.is_ok(), "{k:?}");
}

/// Review F4 (mutation M18): the key of a fixed repository, pinned. The run guard must
/// never change the key, so entries written by `master` keep hitting; this value was
/// computed by `master` (`a818479`) and by this branch alike.
#[tokio::test]
async fn the_key_of_a_fixed_repository_is_pinned() {
    if !have_git() {
        return;
    }
    let r = tempdir();
    fs::write(r.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    fs::write(r.path().join("flake.lock"), "{}\n").unwrap();
    git(r.path(), &["init", "-q"]);
    git(r.path(), &["add", "flake.nix", "flake.lock"]);
    let ok = Command::new("git")
        .current_dir(r.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(["commit", "-q", "-m", "pinned"])
        .status()
        .unwrap();
    assert!(ok.success());
    assert_eq!(
        git(r.path(), &["rev-parse", "HEAD"]).trim(),
        "579811899085928f2cda55b0b05dae39e78bffad",
        "the fixture commit is not the pinned one"
    );
    assert_eq!(
        key(r.path()).await.unwrap().as_str(),
        "v2-71451c8e29e86e59a2bd3be004df05e8a725aee5481e3a9ee3454b16b6bf2ab9"
    );
}
