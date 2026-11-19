//! The eval cache against a fleet repository, cache directory or `flake.lock` that someone
//! else prepared: repository-configured commands, endless attribute files, symlinks,
//! FIFOs, a dropped receiver. Review findings F1, F2, F5, F10 and F11.
//!
//! The repositories are temporary, with git's global and system config switched off for
//! the test's own git calls. Each hostile command is a script that appends to a marker
//! file, so "it ran" is a fact the test can read.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use auspex_domain::eval::{CacheEntry, CacheKey, EvalSuccess};
use auspex_domain::fleet::MachineClass;
use auspex_domain::observe::StorePath;
use auspex_effects::cache::{
    CacheParams, GitParams, entry_path, evaluate_cached, key_for, lookup, store,
};
use auspex_effects::nix::{EvalItem, EvalMachine, EvalParams, FlakeRef};
use auspex_effects::{CancellationToken, Cancelled};
use common::{
    Tools, find_on_path, in_nix_build, still_alive_after_wait, tempdir, tools, wait_for_pids,
};
use tokio::sync::mpsc::unbounded_channel;

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

/// A committed repository with `flake.nix`, `flake.lock` and `.gitattributes` putting
/// every file through filter `x`.
fn repo() -> Option<tempfile::TempDir> {
    if !have_git() {
        return None;
    }
    let dir = tempdir();
    fs::write(dir.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    fs::write(dir.path().join("flake.lock"), "{}\n").unwrap();
    fs::write(dir.path().join(".gitattributes"), "* filter=x\n").unwrap();
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &["add", "flake.nix", "flake.lock", ".gitattributes"],
    );
    git(dir.path(), &["commit", "-q", "-m", "one"]);
    Some(dir)
}

/// A script outside the repository that records that it ran, then behaves as `then`.
fn payload(t: &Tools, dir: &Path, marker: &Path, then: &str) -> PathBuf {
    t.script(
        dir,
        "payload",
        &format!("printf 'ran\\n' >> '{}'\n{then}", marker.display()),
    )
}

async fn key(dir: &Path) -> Result<CacheKey, String> {
    key_for(dir, &GitParams::new(), &CancellationToken::new())
        .await
        .unwrap()
}

/// Same bytes, a stat that no longer matches the index: the state of any copied or
/// unpacked checkout, and what makes git re-read (and filter) the file.
fn make_stat_dirty(path: &Path) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(978_307_200))
        .unwrap();
}

#[tokio::test]
async fn a_repository_clean_filter_never_runs() {
    let Some(t) = tools(&["cat"]) else { return };
    let Some(r) = repo() else { return };
    let side = tempdir();
    let marker = side.path().join("marker");
    let p = payload(&t, side.path(), &marker, "exec @cat@");
    git(r.path(), &["config", "filter.x.clean", p.to_str().unwrap()]);
    fs::write(r.path().join("flake.nix"), "{ outputs = _: { x = 1; }; }\n").unwrap();
    key(r.path()).await.unwrap();
    assert!(
        fs::read_to_string(&marker).is_err(),
        "the repository's filter ran"
    );
}

#[tokio::test]
async fn a_repository_process_filter_never_runs_on_a_stat_dirty_tree() {
    let Some(t) = tools(&[]) else { return };
    let Some(r) = repo() else { return };
    let side = tempdir();
    let marker = side.path().join("marker");
    let p = payload(&t, side.path(), &marker, "exit 1");
    git(
        r.path(),
        &["config", "filter.x.process", p.to_str().unwrap()],
    );
    make_stat_dirty(&r.path().join("flake.nix"));
    let _ = key(r.path()).await;
    assert!(
        fs::read_to_string(&marker).is_err(),
        "the repository's process filter ran"
    );
}

#[tokio::test]
async fn repository_hooks_and_fsmonitor_never_run() {
    let Some(t) = tools(&[]) else { return };
    let Some(r) = repo() else { return };
    let side = tempdir();
    let marker = side.path().join("marker");
    let p = payload(&t, side.path(), &marker, "exit 0");
    let hooks = side.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    for h in [
        "post-index-change",
        "post-checkout",
        "reference-transaction",
    ] {
        symlink(&p, hooks.join(h)).unwrap();
    }
    git(r.path(), &["config", "core.fsmonitor", p.to_str().unwrap()]);
    git(
        r.path(),
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    make_stat_dirty(&r.path().join("flake.nix"));
    let index = r.path().join(".git/index");
    let before = fs::metadata(&index).unwrap();
    key(r.path()).await.unwrap();
    let after = fs::metadata(&index).unwrap();
    assert!(
        fs::read_to_string(&marker).is_err(),
        "a hook or fsmonitor ran"
    );
    assert_eq!(
        (before.len(), before.modified().unwrap()),
        (after.len(), after.modified().unwrap()),
        "the index was rewritten"
    );
}

/// The key reads the worktree's bytes: an edit of the same length, with the mtime put back,
/// still changes it.
#[tokio::test]
async fn the_key_follows_worktree_bytes_not_stat_data() {
    let Some(r) = repo() else { return };
    let f = r.path().join("flake.nix");
    let before = key(r.path()).await.unwrap();
    let mtime = fs::metadata(&f).unwrap().modified().unwrap();
    // Same length as the committed "{ outputs = _: {}; }\n".
    fs::write(&f, "{ outputs = _: []; }\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert_ne!(key(r.path()).await.unwrap(), before);
}

/// A tracked file replaced by a symlink to `/dev/zero`, or by a FIFO, is not read: the key
/// comes back at once.
#[tokio::test]
async fn tracked_paths_that_are_not_files_are_not_read() {
    let Some(r) = repo() else { return };
    let lock = r.path().join("flake.nix");
    fs::remove_file(&lock).unwrap();
    symlink("/dev/zero", &lock).unwrap();
    let start = Instant::now();
    let k1 = key(r.path()).await;
    assert!(start.elapsed() < Duration::from_secs(5));
    fs::remove_file(&lock).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &lock,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let k2 = tokio::time::timeout(Duration::from_secs(5), key(r.path()))
        .await
        .expect("a FIFO in the worktree hung the key");
    assert!(k1.is_ok() && k2.is_ok(), "{k1:?} {k2:?}");
    assert_ne!(k1, k2);
}

/// `flake.lock` as a symlink to `/dev/zero`: refused, not read. Run in a child process
/// that limits its own address space, so a regression fails instead of eating memory.
#[test]
fn a_flake_lock_symlink_to_dev_zero_is_not_read() {
    if std::env::var_os("AUSPEX_TEST_LOCK_INNER").is_some() {
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lock_dev_zero_inner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AUSPEX_TEST_LOCK_INNER", "1")
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
async fn lock_dev_zero_inner() {
    if std::env::var_os("AUSPEX_TEST_LOCK_INNER").is_none() {
        return;
    }
    let gib = 1024 * 1024 * 1024;
    rustix::process::setrlimit(
        rustix::process::Resource::As,
        rustix::process::Rlimit {
            current: Some(gib),
            maximum: None,
        },
    )
    .unwrap();
    let Some(r) = repo() else { return };
    let lock = r.path().join("flake.lock");
    fs::remove_file(&lock).unwrap();
    symlink("/dev/zero", &lock).unwrap();
    let start = Instant::now();
    let e = key(r.path()).await.unwrap_err();
    assert!(e.contains("flake.lock"), "{e}");
    assert!(start.elapsed() < Duration::from_secs(5));
    // Peak memory: reading /dev/zero would have grown to the limit before failing.
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    if let Some(kb) = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())
    {
        assert!(kb < 200 * 1024, "peak RSS {kb} kB: /dev/zero was read");
    }
    assert!(!e.contains("out of memory"), "{e}");
}

/// `flake.lock` as a FIFO: no hang, and a cancel is answered.
#[tokio::test]
async fn a_flake_lock_fifo_neither_hangs_nor_ignores_cancel() {
    let Some(r) = repo() else { return };
    let lock = r.path().join("flake.lock");
    fs::remove_file(&lock).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &lock,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c.cancel();
    });
    let r2 = tokio::time::timeout(
        Duration::from_secs(4),
        key_for(r.path(), &GitParams::new(), &cancel),
    )
    .await;
    // Release a reader stuck in open(2), whatever happened, so the runtime can shut down.
    let _ = rustix::fs::open(
        &lock,
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    );
    let r2 = r2.expect("key_for hung on a FIFO flake.lock");
    assert!(
        matches!(r2, Err(Cancelled) | Ok(Err(_))),
        "a FIFO is not a lock file: {r2:?}"
    );

    // And without a cancel: refused at once, not after the timeout.
    let start = Instant::now();
    let r3 = tokio::time::timeout(Duration::from_secs(4), key(r.path())).await;
    let _ = rustix::fs::open(
        &lock,
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    );
    let e = r3.expect("key_for hung on a FIFO flake.lock").unwrap_err();
    assert!(e.contains("not a regular file"), "{e}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

fn success(machine: &str) -> EvalSuccess {
    EvalSuccess {
        attr: machine.to_owned(),
        attr_path: vec![machine.to_owned()],
        drv_path: StorePath::new(format!(
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-{machine}.drv"
        ))
        .unwrap(),
        out_path: StorePath::new(format!(
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-{machine}"
        ))
        .unwrap(),
        system: Some("x86_64-linux".to_owned()),
    }
}

#[tokio::test]
async fn a_symlink_inside_the_cache_is_not_followed() {
    let Some(r) = repo() else { return };
    let k = key(r.path()).await.unwrap();
    let cache = tempdir();
    let elsewhere = tempdir();
    symlink(elsewhere.path(), cache.path().join("eval")).unwrap();
    let _ = store(cache.path(), &k, "terra", &success("terra")).await;
    assert_eq!(
        fs::read_dir(elsewhere.path()).unwrap().count(),
        0,
        "a write landed outside the cache"
    );

    // An entry reached through a planted link is not read either.
    let real = tempdir();
    store(real.path(), &k, "terra", &success("terra"))
        .await
        .unwrap();
    fs::remove_file(cache.path().join("eval")).unwrap();
    symlink(real.path().join("eval"), cache.path().join("eval")).unwrap();
    assert_eq!(lookup(cache.path(), &k, "terra").await, None);

    // Nor an entry file that is itself a link to a good entry.
    let plain = tempdir();
    let target = entry_path(real.path(), &k, "terra");
    let path = entry_path(plain.path(), &k, "terra");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(lookup(plain.path(), &k, "terra").await, None);
}

#[tokio::test]
async fn a_planted_entry_is_bounded_and_normalised() {
    let Some(r) = repo() else { return };
    let k = key(r.path()).await.unwrap();
    let cache = tempdir();
    let path = entry_path(cache.path(), &k, "terra");
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    // Escape sequences in the display-only `attr` do not survive a hit.
    let mut planted = success("terra");
    planted.attr = "\u{1b}]0;pwned\u{7}\u{1b}[2J".to_owned();
    fs::write(
        &path,
        CacheEntry::new(k.clone(), "terra".to_owned(), planted).to_json(),
    )
    .unwrap();
    let hit = lookup(cache.path(), &k, "terra").await.unwrap();
    assert_eq!(hit, success("terra"));

    // A valid entry padded past the size cap is not read.
    let mut big = CacheEntry::new(k.clone(), "terra".to_owned(), success("terra")).to_json();
    big.push_str(&" ".repeat(2 * 1024 * 1024));
    fs::write(&path, big).unwrap();
    assert_eq!(lookup(cache.path(), &k, "terra").await, None);
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

/// F2: the shell is told to call `evaluate_cached`; dropping its receiver must cancel it
/// and kill the evaluator, as for `evaluate`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_receiver_of_evaluate_cached_cancels() {
    let Some(t) = tools(&["sleep"]) else { return };
    let Some(r) = repo() else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        "@sleep@ 30 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait",
    );
    let cache = tempdir();
    let (tx, mut rx) = unbounded_channel();
    let (repo_path, cache_path, prog_path) = (
        r.path().to_path_buf(),
        cache.path().to_path_buf(),
        prog.clone(),
    );
    let task = tokio::spawn(async move {
        let mut eval = EvalParams::new();
        eval.runtime_dir = Some(common::runtime_dir());
        eval.program = prog_path.as_os_str();
        eval.timeout = Duration::from_secs(60);
        let cache = CacheParams {
            cache_dir: Some(&cache_path),
            flake_dir: &repo_path,
            git: GitParams::new(),
        };
        let flake = FlakeRef::new(repo_path.to_str().unwrap()).unwrap();
        evaluate_cached(
            &flake,
            &machines(&["a"]),
            &eval,
            &cache,
            &CancellationToken::new(),
            tx,
        )
        .await
    });
    let pids_file = dir.path().join("pids");
    let pids = tokio::task::spawn_blocking(move || wait_for_pids(&pids_file, 2))
        .await
        .unwrap();
    // Caching was on (else this would only test plain `evaluate`): no "cache off" note.
    while let Ok(i) = rx.try_recv() {
        assert!(
            !matches!(&i, EvalItem::Note(n) if n.contains("cache off")),
            "caching was off: {i:?}"
        );
    }
    drop(rx);
    let r = tokio::time::timeout(Duration::from_secs(3), task).await;
    let left = still_alive_after_wait(&pids);
    common::cleanup(&pids);
    assert_eq!(
        r.expect("evaluate_cached ignored the dropped receiver")
            .unwrap(),
        Err(Cancelled)
    );
    assert!(left.is_empty(), "still running: {left:?}");
}

/// F11: a `path:` flake sees untracked and ignored files, whose contents are not in the
/// key, so it gets no caching (and a note saying why).
#[tokio::test]
async fn a_path_flake_reference_gets_no_caching() {
    let Some(t) = tools(&[]) else { return };
    let Some(r) = repo() else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        r#"printf '%s\n' '{"attr":"a","attrPath":["a"],"drvPath":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-a.drv","outputs":{"out":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-a"}}'"#,
    );
    let cache = tempdir();
    let mut eval = EvalParams::new();
    eval.runtime_dir = Some(common::runtime_dir());
    eval.program = prog.as_os_str();
    let params = CacheParams {
        cache_dir: Some(cache.path()),
        flake_dir: r.path(),
        git: GitParams::new(),
    };
    let flake = FlakeRef::new(&format!("path:{}", r.path().display())).unwrap();
    let (tx, mut rx) = unbounded_channel();
    evaluate_cached(
        &flake,
        &machines(&["a"]),
        &eval,
        &params,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();
    let mut items = Vec::new();
    while let Some(i) = rx.recv().await {
        items.push(i);
    }
    assert!(
        matches!(&items[0], EvalItem::Note(n) if n.contains("path:")),
        "{items:?}"
    );
    assert!(!cache.path().join("eval").exists(), "nothing was cached");
}

#[tokio::test]
async fn an_oversized_flake_lock_gets_no_caching() {
    let Some(r) = repo() else { return };
    let lock = r.path().join("flake.lock");
    let f = fs::File::create(&lock).unwrap();
    f.set_len(auspex_effects::cache::MAX_LOCK_BYTES + 1)
        .unwrap();
    let e = key(r.path()).await.unwrap_err();
    assert!(e.contains("flake.lock is larger than"), "{e}");
}

/// A tracked symlink is keyed by its target text, and a directory replaced by a symlink is
/// not read through: the key sees what nix would, not what the link points to.
#[tokio::test]
async fn tracked_symlinks_are_keyed_by_target_and_not_followed() {
    let Some(r) = repo() else { return };
    let d = r.path();
    symlink("one", d.join("link")).unwrap();
    fs::create_dir(d.join("sub")).unwrap();
    fs::write(d.join("sub/host.nix"), "1\n").unwrap();
    git(d, &["add", "link", "sub/host.nix"]);
    git(d, &["commit", "-q", "-m", "links"]);
    let base = key(d).await.unwrap();

    fs::remove_file(d.join("link")).unwrap();
    symlink("two", d.join("link")).unwrap();
    assert_ne!(
        key(d).await.unwrap(),
        base,
        "the link's target is part of the key"
    );
    fs::remove_file(d.join("link")).unwrap();
    symlink("one", d.join("link")).unwrap();
    assert_eq!(key(d).await.unwrap(), base);

    // `sub` becomes a symlink to a directory elsewhere: what is there must not matter.
    let elsewhere = tempdir();
    fs::remove_dir_all(d.join("sub")).unwrap();
    symlink(elsewhere.path(), d.join("sub")).unwrap();
    fs::write(elsewhere.path().join("host.nix"), "2\n").unwrap();
    let k1 = key(d).await.unwrap();
    fs::write(elsewhere.path().join("host.nix"), "3\n").unwrap();
    assert_eq!(
        key(d).await.unwrap(),
        k1,
        "a file was read through a symlinked directory"
    );
    assert_ne!(k1, base);
}

/// Review finding B2, without nix: the tree is edited while the evaluator runs. The
/// results still reach the caller, but none is stored (the second key differs), a note
/// says so, and after the edit is undone nothing is served from the cache.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario: the edited run, the undo, the clean run"
)]
async fn an_edit_during_the_evaluation_stores_nothing() {
    let Some(t) = tools(&["sleep"]) else { return };
    let Some(r) = repo() else { return };
    let dir = tempdir();
    let line = |m: &str| {
        format!(
            r#"{{"attr":"{m}","attrPath":["{m}"],"drvPath":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{m}.drv","outputs":{{"out":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{m}"}}}}"#
        )
    };
    let prog = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf '%s\\n' '{}'\nwhile [ ! -e '@dir@/go' ]; do @sleep@ 0.02; done\nprintf '%s\\n' '{}'",
            line("a"),
            line("b")
        ),
    );
    let cache = tempdir();
    let mut eval = EvalParams::new();
    eval.runtime_dir = Some(common::runtime_dir());
    eval.program = prog.as_os_str();
    eval.timeout = Duration::from_secs(10);
    let params = CacheParams {
        cache_dir: Some(cache.path()),
        flake_dir: r.path(),
        git: GitParams::new(),
    };
    let flake = FlakeRef::new(r.path().to_str().unwrap()).unwrap();
    let ms = machines(&["a", "b"]);
    let committed = fs::read(r.path().join("flake.nix")).unwrap();

    let (tx, mut rx) = unbounded_channel();
    let cancel = CancellationToken::new();
    let run = evaluate_cached(&flake, &ms, &eval, &params, &cancel, tx);
    let edit = async {
        let mut items = Vec::new();
        while let Some(i) = rx.recv().await {
            if matches!(&i, EvalItem::Machine { machine, .. } if machine == "a") {
                // An operator saving a file mid-scan.
                fs::write(
                    r.path().join("flake.nix"),
                    "{ outputs = _: { edited = 1; }; }\n",
                )
                .unwrap();
                fs::write(dir.path().join("go"), "").unwrap();
            }
            items.push(i);
        }
        items
    };
    let (res, items) = tokio::join!(run, edit);
    res.unwrap();
    let results = items
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
        .count();
    assert_eq!(results, 2, "both results reach the caller: {items:?}");
    assert!(
        items
            .iter()
            .any(|i| matches!(i, EvalItem::Note(n) if n.contains("changed during evaluation"))),
        "{items:?}"
    );
    assert_eq!(items.last(), Some(&EvalItem::Finished { error: None }));

    // Undo the edit: the tree is the one the run started from, and nothing was stored.
    fs::write(r.path().join("flake.nix"), committed).unwrap();
    fs::remove_file(dir.path().join("go")).unwrap();
    fs::write(dir.path().join("go"), "").unwrap();
    let (tx, mut rx) = unbounded_channel();
    evaluate_cached(&flake, &ms, &eval, &params, &CancellationToken::new(), tx)
        .await
        .unwrap();
    let mut again = Vec::new();
    while let Some(i) = rx.recv().await {
        again.push(i);
    }
    assert!(
        !again.iter().any(|i| matches!(
            i,
            EvalItem::Machine {
                from_cache: true,
                ..
            }
        )),
        "a result of the edited run was served from the cache: {again:?}"
    );
    // And an undisturbed run is stored: the third run hits.
    let (tx, mut rx) = unbounded_channel();
    evaluate_cached(&flake, &ms, &eval, &params, &CancellationToken::new(), tx)
        .await
        .unwrap();
    let mut third = Vec::new();
    while let Some(i) = rx.recv().await {
        third.push(i);
    }
    let hits = third
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
        .count();
    assert_eq!(hits, 2, "{third:?}");
}
