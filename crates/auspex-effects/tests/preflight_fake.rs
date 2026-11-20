//! What the checks before an update look up: the fleet repository's state and the pending
//! closure's size, against fake `git` and `nix` programs (scripts passed as the program), and
//! against a real `git` in a temporary repository for what only a real one can show (that
//! `status` runs no filter, no hook, and writes nothing). Nothing here touches a real fleet,
//! cache or store.
//!
//! The fakes record their argv, working directory and environment, so the tests pin **what
//! is run**, not only what comes back.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use auspex_domain::observe::StorePath;
use auspex_domain::preflight::{TreeState, Upstream, status_args};
use auspex_effects::cache::GitParams;
use auspex_effects::closure::PendingParams;
use auspex_effects::nix::FlakeRef;
use auspex_effects::preflight::{
    cache_size_argv, closure_size, fleet_tree, local_size_argv, tree_state,
};
use auspex_effects::{CancellationToken, Cancelled};
use common::{
    Tools, find_on_path, in_nix_build, still_alive_after_wait, tempdir, tools, wait_for_pids,
};

const A: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-glibc-2.40-66";
const B: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-systemd-256.10";
const SYSTEM: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-25.11";

fn store_path() -> StorePath {
    StorePath::new(SYSTEM).unwrap()
}

// ---------------------------------------------------------------------------------------
// git status
// ---------------------------------------------------------------------------------------

/// A fake git: records every call's argv (each followed by a `--` line), its working directory
/// and two environment variables; answers the config listing with `listing` (a `printf` format)
/// and runs `body` for anything else (the status).
fn fake_git_listing(t: &Tools, dir: &Path, listing: &str, body: &str) -> PathBuf {
    t.script(
        dir,
        "git",
        &format!(
            "printf '%s\\n' \"$@\" >> '@dir@/git-argv'\n\
             printf -- '--\\n' >> '@dir@/git-argv'\n\
             pwd > '@dir@/git-cwd'\n\
             printf '%s|%s\\n' \"$GIT_OPTIONAL_LOCKS\" \"${{GIT_DIR-unset}}\" > '@dir@/git-env'\n\
             case \" $* \" in *\" config \"*) printf '{listing}'; exit 0;; esac\n\
             {body}"
        ),
    )
}

/// [`fake_git_listing`] for a repository that defines no filter drivers.
fn fake_git(t: &Tools, dir: &Path, body: &str) -> PathBuf {
    fake_git_listing(t, dir, "core.bare\\0", body)
}

fn git_params(program: &Path, timeout: Duration) -> GitParams<'_> {
    let mut p = GitParams::new();
    p.program = program.as_os_str();
    p.timeout = timeout;
    p
}

fn tracking(ahead: u64, behind: u64) -> Upstream {
    Upstream::Tracking {
        name: "origin/main".to_owned(),
        ahead,
        behind,
    }
}

#[tokio::test]
async fn the_tree_check_lists_the_config_then_runs_one_read_only_hardened_git_status() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let repo = dir.path().join("fleet");
    fs::create_dir(&repo).unwrap();
    let program = fake_git_listing(
        &t,
        dir.path(),
        "core.bare\\0filter.evil.clean\\0filter.my.driver.process\\0",
        "printf '## main...origin/main [ahead 2]\\0 M flake.nix\\0M  a.nix\\0'",
    );
    let answer = tree_state(
        &git_params(&program, Duration::from_secs(10)),
        &repo,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        Ok(TreeState {
            changed_files: 2,
            upstream: tracking(2, 0),
        })
    );
    // Two commands, both with the cache module's hardening first: the read-only listing of
    // config keys, then the status with every filter driver the listing named blanked.
    let argv = fs::read_to_string(dir.path().join("git-argv")).unwrap();
    assert_eq!(
        argv,
        "-c\ncore.fsmonitor=false\nconfig\n--list\n--name-only\n-z\n--\n\
         -c\ncore.fsmonitor=false\n--no-optional-locks\n-c\ncore.hooksPath=/dev/null\n\
         -c\ncore.attributesFile=/dev/null\n\
         -c\nfilter.evil.clean=\n-c\nfilter.evil.smudge=\n-c\nfilter.evil.process=\n\
         -c\nfilter.evil.required=false\n\
         -c\nfilter.my.driver.clean=\n-c\nfilter.my.driver.smudge=\n\
         -c\nfilter.my.driver.process=\n-c\nfilter.my.driver.required=false\n\
         status\n--porcelain=v1\n-b\n-z\n--untracked-files=no\n--ignore-submodules=all\n\
         --no-renames\n--\n"
    );
    // The same, from the domain's own function.
    let mut want = vec!["-c".to_owned(), "core.fsmonitor=false".to_owned()];
    want.extend(status_args(&["evil".to_owned(), "my.driver".to_owned()]));
    assert!(
        argv.contains(&format!("{}\n--\n", want.join("\n"))),
        "{argv}"
    );
    // In the fleet's directory, with locks off.
    let cwd = fs::read_to_string(dir.path().join("git-cwd")).unwrap();
    assert_eq!(
        Path::new(cwd.trim()).canonicalize().unwrap(),
        repo.canonicalize().unwrap()
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("git-env")).unwrap(),
        "0|unset\n"
    );
}

#[tokio::test]
async fn clean_dirty_ahead_and_no_upstream_are_read_from_the_branch_line_and_the_records() {
    let Some(t) = tools(&["cat"]) else { return };
    let cases: [(&str, TreeState); 4] = [
        (
            "## main...origin/main\\0",
            TreeState {
                changed_files: 0,
                upstream: tracking(0, 0),
            },
        ),
        (
            "## main...origin/main [ahead 1, behind 3]\\0",
            TreeState {
                changed_files: 0,
                upstream: tracking(1, 3),
            },
        ),
        (
            "## main\\0 M a\\0 D b\\0",
            TreeState {
                changed_files: 2,
                upstream: Upstream::None,
            },
        ),
        (
            "## main...origin/main [gone]\\0",
            TreeState {
                changed_files: 0,
                upstream: Upstream::Gone("origin/main".to_owned()),
            },
        ),
    ];
    for (output, want) in cases {
        let dir = tempdir();
        let program = fake_git(&t, dir.path(), &format!("printf '{output}'"));
        let answer = tree_state(
            &git_params(&program, Duration::from_secs(10)),
            dir.path(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(answer, Ok(want), "{output}");
    }
}

#[tokio::test]
async fn a_tree_check_that_cannot_run_is_one_line() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let ask = |program: PathBuf| {
        let dir = dir.path().to_owned();
        async move {
            tree_state(
                &git_params(&program, Duration::from_secs(10)),
                &dir,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
        }
    };
    // Not a repository.
    let program = fake_git(
        &t,
        dir.path(),
        "printf 'fatal: not a git repository (or any of the parent directories): .git\\n' >&2\nexit 128",
    );
    let why = ask(program).await.unwrap_err();
    assert!(
        why.contains("git status") && why.contains("not a git repository"),
        "{why}"
    );
    // Output that is not `git status`'s.
    let program = fake_git(&t, dir.path(), "printf 'hello\\n'");
    let why = ask(program).await.unwrap_err();
    assert!(why.contains("branch line"), "{why}");
    // Nothing at all.
    let program = fake_git(&t, dir.path(), "true");
    assert!(ask(program).await.is_err());
    // No git program.
    let missing = dir.path().join("no-such-git");
    let why = ask(missing).await.unwrap_err();
    assert!(why.contains("git"), "{why}");
    // Escape sequences in git's own error text never reach the line unsanitised.
    let program = fake_git(
        &t,
        dir.path(),
        "printf 'fatal: \\033]0;pwned\\007bad\\n' >&2\nexit 128",
    );
    let why = ask(program).await.unwrap_err();
    assert!(!why.contains('\u{1b}') && !why.contains('\u{7}'), "{why:?}");
}

#[tokio::test]
async fn a_slow_git_is_a_timeout_and_a_cancelled_one_is_killed_and_reaped() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let pids = dir.path().join("pids");
    let program = fake_git(
        &t,
        dir.path(),
        &format!(
            "printf '%s\\n' $$ >> '{}'\n@sleep@ 30 &\nprintf '%s\\n' $! >> '{}'\nwait",
            pids.display(),
            pids.display()
        ),
    );
    let started = Instant::now();
    let why = tree_state(
        &git_params(&program, Duration::from_millis(300)),
        dir.path(),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("timed out"), "{why}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let gone = still_alive_after_wait(&wait_for_pids(&pids, 2));
    assert_eq!(gone, Vec::<i32>::new(), "left running after the timeout");

    // Cancelled.
    let _ = fs::remove_file(&pids);
    let cancel = CancellationToken::new();
    let canceller = {
        let (cancel, pids) = (cancel.clone(), pids.clone());
        tokio::task::spawn_blocking(move || {
            let p = wait_for_pids(&pids, 2);
            cancel.cancel();
            p
        })
    };
    let answer = tree_state(
        &git_params(&program, Duration::from_secs(60)),
        dir.path(),
        &cancel,
    )
    .await;
    assert_eq!(answer, Err(Cancelled));
    let spawned = canceller.await.unwrap();
    assert_eq!(still_alive_after_wait(&spawned), Vec::<i32>::new());
}

#[tokio::test]
async fn a_fleet_that_is_not_a_local_directory_has_no_tree_to_check() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake_git(&t, dir.path(), "printf '## main\\0'");
    let params = git_params(&program, Duration::from_secs(10));
    let remote = FlakeRef::new("github:example/fleet").unwrap();
    let why = fleet_tree(&params, &remote, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert!(why.contains("not a local directory"), "{why}");
    assert!(
        !dir.path().join("git-argv").exists(),
        "git is not run for a remote reference"
    );
    // A local one is checked in its directory.
    let local = FlakeRef::new(dir.path().to_str().unwrap()).unwrap();
    let answer = fleet_tree(&params, &local, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.upstream, Upstream::None);
}

// ---------------------------------------------------------------------------------------
// A real git: what `status` must not do
// ---------------------------------------------------------------------------------------

fn real_git(dir: &Path, args: &[&str]) {
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

/// `Some(tools)` when a real git can be used, `None` (after saying why) when not; inside a
/// Nix build a missing git is a failure, not a skip.
fn with_real_git() -> Option<Tools> {
    let t = tools(&["touch", "cat", "sleep"])?;
    if find_on_path("git").is_none() {
        assert!(!in_nix_build(), "git is not on PATH inside the Nix build");
        eprintln!("SKIPPED: git is not on PATH");
        return None;
    }
    Some(t)
}

fn real_params(timeout: Duration) -> GitParams<'static> {
    let mut p = GitParams::new();
    p.timeout = timeout;
    p
}

async fn real_tree(dir: &Path) -> Result<TreeState, String> {
    tree_state(
        &real_params(Duration::from_secs(20)),
        dir,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_real_repository_is_clean_dirty_ahead_and_untracked_files_do_not_count() {
    let Some(_t) = with_real_git() else { return };
    let dir = tempdir();
    let remote = tempdir();
    let r = dir.path();
    real_git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
    real_git(r, &["init", "-q", "-b", "main"]);
    fs::write(r.join("flake.nix"), "{ }\n").unwrap();
    fs::write(r.join("host.nix"), "1\n").unwrap();
    real_git(r, &["add", "flake.nix", "host.nix"]);
    real_git(r, &["commit", "-q", "-m", "one"]);

    // No upstream yet.
    assert_eq!(
        real_tree(r).await,
        Ok(TreeState {
            changed_files: 0,
            upstream: Upstream::None
        })
    );

    real_git(
        r,
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    real_git(r, &["push", "-q", "-u", "origin", "main"]);
    assert_eq!(
        real_tree(r).await,
        Ok(TreeState {
            changed_files: 0,
            upstream: tracking(0, 0)
        })
    );

    // An untracked file is not a change to a tracked one.
    fs::write(r.join("untracked.nix"), "x\n").unwrap();
    assert_eq!(real_tree(r).await.unwrap().changed_files, 0);

    // A tracked file with new content, and one that is staged.
    fs::write(r.join("host.nix"), "2\n").unwrap();
    fs::write(r.join("flake.nix"), "{ a = 1; }\n").unwrap();
    real_git(r, &["add", "flake.nix"]);
    assert_eq!(real_tree(r).await.unwrap().changed_files, 2);

    // A local commit the upstream does not have.
    real_git(r, &["commit", "-q", "-a", "-m", "two"]);
    assert_eq!(
        real_tree(r).await,
        Ok(TreeState {
            changed_files: 0,
            upstream: tracking(1, 0)
        })
    );

    // Detached: no branch, no upstream.
    real_git(r, &["checkout", "-q", "--detach"]);
    assert_eq!(real_tree(r).await.unwrap().upstream, Upstream::Detached);
}

#[tokio::test]
async fn status_runs_no_filter_named_by_a_tracked_gitattributes_and_no_fsmonitor_hook() {
    let Some(t) = with_real_git() else { return };
    let dir = tempdir();
    let r = dir.path().join("repo");
    fs::create_dir(&r).unwrap();
    real_git(&r, &["init", "-q", "-b", "main"]);
    fs::write(r.join(".gitattributes"), "* filter=evil\n").unwrap();
    fs::write(r.join("host.nix"), "1\n").unwrap();
    real_git(&r, &["add", ".gitattributes", "host.nix"]);
    real_git(&r, &["commit", "-q", "-m", "one"]);
    let filter_ran = dir.path().join("FILTER-RAN");
    let hook_ran = dir.path().join("HOOK-RAN");
    let hook = t.script(
        dir.path(),
        "fsmonitor",
        &format!("@touch@ '{}'\nprintf '\\0'", hook_ran.display()),
    );
    real_git(
        &r,
        &[
            "config",
            "filter.evil.clean",
            &format!(
                "{} '{}'; {}",
                t.get("touch").display(),
                filter_ran.display(),
                t.get("cat").display()
            ),
        ],
    );
    real_git(&r, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
    // A file whose stat changed with the same content is what makes git read it, and so run
    // a clean filter, to decide whether it changed.
    std::thread::sleep(Duration::from_millis(1100));
    let now = fs::read(r.join("host.nix")).unwrap();
    fs::write(r.join("host.nix"), now).unwrap();

    let answer = real_tree(&r).await.unwrap();
    assert_eq!(answer.changed_files, 0, "same content: not a change");
    assert!(
        !filter_ran.exists(),
        "a clean filter from a tracked .gitattributes was run by the status check"
    );
    assert!(
        !hook_ran.exists(),
        "the repository's fsmonitor hook was run by the status check"
    );
}

#[tokio::test]
async fn status_writes_nothing_to_the_repository() {
    let Some(_t) = with_real_git() else { return };
    let dir = tempdir();
    let r = dir.path();
    real_git(r, &["init", "-q", "-b", "main"]);
    fs::write(r.join("host.nix"), "1\n").unwrap();
    real_git(r, &["add", "host.nix"]);
    real_git(r, &["commit", "-q", "-m", "one"]);
    // Make the index stale (a touched file), which an ordinary `git status` would refresh by
    // rewriting `.git/index`.
    std::thread::sleep(Duration::from_millis(1100));
    let now = fs::read(r.join("host.nix")).unwrap();
    fs::write(r.join("host.nix"), now).unwrap();
    let index = r.join(".git/index");
    let before = (
        fs::read(&index).unwrap(),
        fs::metadata(&index).unwrap().modified().unwrap(),
    );
    let _ = real_tree(r).await.unwrap();
    let after = (
        fs::read(&index).unwrap(),
        fs::metadata(&index).unwrap().modified().unwrap(),
    );
    assert_eq!(
        before, after,
        "the index was rewritten by a read-only check"
    );
    assert!(!r.join(".git/index.lock").exists());
}

// ---------------------------------------------------------------------------------------
// The closure's size
// ---------------------------------------------------------------------------------------

struct Fakes {
    nix: PathBuf,
}

fn fake_nix(t: &Tools, dir: &Path, body: &str) -> Fakes {
    Fakes {
        nix: t.script(
            dir,
            "nix",
            &format!(
                "printf 'RUN\\n' >> '@dir@/nix-runs'\nprintf '%s\\n' \"$@\" >> '@dir@/nix-argv'\n{body}"
            ),
        ),
    }
}

fn size_params<'a>(f: &'a Fakes, cache: Option<&'a str>, timeout: Duration) -> PendingParams<'a> {
    PendingParams {
        nix: f.nix.as_os_str(),
        // Sizes come from `nix`; this program must never be run.
        nix_store: Path::new("/nonexistent/nix-store").as_os_str(),
        cache_url: cache,
        timeout,
    }
}

#[test]
fn the_size_queries_are_exactly_these_argvs() {
    assert_eq!(
        local_size_argv(&store_path()),
        ["path-info", "--recursive", "--size", "--", SYSTEM]
    );
    assert_eq!(
        cache_size_argv("https://cache.example/", &store_path()),
        [
            "path-info",
            "--recursive",
            "--size",
            "--option",
            "narinfo-cache-negative-ttl",
            "0",
            "--store",
            "https://cache.example/",
            "--",
            SYSTEM
        ]
    );
}

#[tokio::test]
async fn a_path_valid_locally_is_sized_locally_and_the_cache_is_not_asked() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let f = fake_nix(
        &t,
        dir.path(),
        &format!("printf '%s\\t%s\\n' '{A}' 1000 '{B}' 2000 '{SYSTEM}' 3000"),
    );
    let answer = closure_size(
        &store_path(),
        &size_params(&f, Some("https://cache.example"), Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        Ok(vec![
            (StorePath::new(A).unwrap(), 1000),
            (StorePath::new(B).unwrap(), 2000),
            (StorePath::new(SYSTEM).unwrap(), 3000)
        ])
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("nix-argv")).unwrap(),
        format!("path-info\n--recursive\n--size\n--\n{SYSTEM}\n")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("nix-runs")).unwrap(),
        "RUN\n",
        "one process: no network query when the local store has it"
    );
}

#[tokio::test]
async fn otherwise_the_cache_is_asked_read_only_with_exactly_this_argv() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    // The first call (no --store) fails as nix does for a path that is not valid; the second
    // answers.
    let f = fake_nix(
        &t,
        dir.path(),
        &format!(
            "case \"$*\" in *--store*) printf '%s %s\\n' '{A}' 40 '{SYSTEM}' 2;; \
             *) printf 'error: path is not valid\\n' >&2; exit 1;; esac"
        ),
    );
    let answer = closure_size(
        &store_path(),
        &size_params(&f, Some("https://cache.example/"), Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        Ok(vec![
            (StorePath::new(A).unwrap(), 40),
            (StorePath::new(SYSTEM).unwrap(), 2)
        ])
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("nix-argv")).unwrap(),
        format!(
            "path-info\n--recursive\n--size\n--\n{SYSTEM}\n\
             path-info\n--recursive\n--size\n--option\nnarinfo-cache-negative-ttl\n0\n\
             --store\nhttps://cache.example/\n--\n{SYSTEM}\n"
        )
    );
}

#[tokio::test]
async fn both_failing_names_both_and_no_cache_says_so() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let f = fake_nix(
        &t,
        dir.path(),
        "case \"$*\" in *--store*) printf 'error: unable to download narinfo\\n' >&2;; \
         *) printf 'error: path is not valid\\n' >&2;; esac\nexit 1",
    );
    let why = closure_size(
        &store_path(),
        &size_params(&f, Some("https://cache.example"), Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        why.contains("not in the local store")
            && why.contains("path is not valid")
            && why.contains("unable to download narinfo"),
        "{why}"
    );

    let why = closure_size(
        &store_path(),
        &size_params(&f, None, Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("no binary cache is configured"), "{why}");

    let why = closure_size(
        &store_path(),
        &size_params(&f, Some("not a url"), Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("cache URL is unusable"), "{why}");
}

#[tokio::test]
async fn an_unreadable_listing_is_an_error_and_never_a_low_sum() {
    let Some(t) = tools(&["cat"]) else { return };
    for output in [
        // A line of noise among the paths: the sum would leave something out.
        format!("printf '%s\\t%s\\n' '{A}' 1000\nprintf 'warning: something\\n'"),
        format!("printf '%s\\t%s\\n' '{A}' lots"),
        "true".to_owned(),
        format!("printf '%s\\n' '{A}'"),
    ] {
        let dir = tempdir();
        let f = fake_nix(&t, dir.path(), &output);
        let answer = closure_size(
            &store_path(),
            &size_params(&f, None, Duration::from_secs(10)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(answer.is_err(), "{output}: {answer:?}");
    }
}

#[tokio::test]
async fn a_slow_nix_is_a_timeout_and_a_cancelled_one_is_killed_and_reaped() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let pids = dir.path().join("pids");
    let f = fake_nix(
        &t,
        dir.path(),
        &format!(
            "printf '%s\\n' $$ >> '{p}'\n@sleep@ 30 &\nprintf '%s\\n' $! >> '{p}'\nwait",
            p = pids.display()
        ),
    );
    let started = Instant::now();
    let why = closure_size(
        &store_path(),
        &size_params(
            &f,
            Some("https://cache.example"),
            Duration::from_millis(400),
        ),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("timed out"), "{why}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "one budget for both attempts"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("nix-runs")).unwrap(),
        "RUN\n",
        "the budget was spent on the first: the cache is not tried"
    );
    assert_eq!(
        still_alive_after_wait(&wait_for_pids(&pids, 2)),
        Vec::<i32>::new()
    );

    let _ = fs::remove_file(&pids);
    let cancel = CancellationToken::new();
    let canceller = {
        let (cancel, pids) = (cancel.clone(), pids.clone());
        tokio::task::spawn_blocking(move || {
            let p = wait_for_pids(&pids, 2);
            cancel.cancel();
            p
        })
    };
    let answer = closure_size(
        &store_path(),
        &size_params(&f, None, Duration::from_secs(60)),
        &cancel,
    )
    .await;
    assert_eq!(answer, Err(Cancelled));
    assert_eq!(
        still_alive_after_wait(&canceller.await.unwrap()),
        Vec::<i32>::new()
    );
}

// ---------------------------------------------------------------------------------------
// Fix round 1 (review 5, 6): every way an attribute can name a filter, and SHA-256
// ---------------------------------------------------------------------------------------

/// One way a repository can make `git status` run a filter, set up in a fresh repository whose
/// `host.nix` is committed. `filter_cmd` is the command line a filter driver would run (it
/// touches a marker file and then copies stdin to stdout, so a clean filter would also work).
struct Route {
    name: &'static str,
    /// Configures the route in `repo`, for a driver (named `driver`) that runs `filter_cmd`.
    /// `kind` is which of the driver's commands to set (`clean`, `process`, `smudge`).
    setup: fn(repo: &Path, driver: &str, kind: &str, filter_cmd: &str),
    driver: &'static str,
    kind: &'static str,
}

fn set_driver(repo: &Path, driver: &str, kind: &str, cmd: &str) {
    real_git(repo, &["config", &format!("filter.{driver}.{kind}"), cmd]);
}

fn attributes_in_info(repo: &Path, driver: &str) {
    fs::create_dir_all(repo.join(".git/info")).unwrap();
    fs::write(
        repo.join(".git/info/attributes"),
        format!("* filter={driver}\n"),
    )
    .unwrap();
}

const ROUTES: &[Route] = &[
    Route {
        name: "a tracked .gitattributes",
        setup: |r, d, k, c| {
            fs::write(r.join(".gitattributes"), format!("* filter={d}\n")).unwrap();
            real_git(r, &["add", ".gitattributes"]);
            real_git(r, &["commit", "-q", "-m", "attributes"]);
            set_driver(r, d, k, c);
        },
        driver: "evil",
        kind: "clean",
    },
    Route {
        name: ".git/info/attributes",
        setup: |r, d, k, c| {
            attributes_in_info(r, d);
            set_driver(r, d, k, c);
        },
        driver: "evil",
        kind: "clean",
    },
    Route {
        name: "core.attributesFile",
        setup: |r, d, k, c| {
            let file = r.join("..").join("attrs-elsewhere");
            fs::write(&file, format!("* filter={d}\n")).unwrap();
            real_git(
                r,
                &["config", "core.attributesFile", file.to_str().unwrap()],
            );
            set_driver(r, d, k, c);
        },
        driver: "evil",
        kind: "clean",
    },
    Route {
        name: "a driver defined by an included config file",
        setup: |r, d, k, c| {
            attributes_in_info(r, d);
            fs::write(
                r.join(".git/extra.cfg"),
                format!("[filter \"{d}\"]\n\t{k} = {c}\n"),
            )
            .unwrap();
            real_git(r, &["config", "include.path", "extra.cfg"]);
        },
        driver: "evil",
        kind: "clean",
    },
    Route {
        name: "a long-running process filter",
        setup: |r, d, k, c| {
            attributes_in_info(r, d);
            set_driver(r, d, k, c);
        },
        driver: "evil",
        kind: "process",
    },
    Route {
        name: "a driver whose name has a dot",
        setup: |r, d, k, c| {
            attributes_in_info(r, d);
            set_driver(r, d, k, c);
        },
        driver: "my.driver",
        kind: "clean",
    },
    Route {
        name: "a smudge filter",
        setup: |r, d, k, c| {
            attributes_in_info(r, d);
            set_driver(r, d, k, c);
        },
        driver: "evil",
        kind: "smudge",
    },
];

#[tokio::test]
async fn no_filter_runs_whichever_way_the_repository_names_it() {
    let Some(t) = with_real_git() else { return };
    for route in ROUTES {
        let dir = tempdir();
        let r = dir.path().join("repo");
        fs::create_dir(&r).unwrap();
        real_git(&r, &["init", "-q", "-b", "main"]);
        fs::write(r.join("host.nix"), "1\n").unwrap();
        real_git(&r, &["add", "host.nix"]);
        real_git(&r, &["commit", "-q", "-m", "one"]);
        let ran = dir.path().join("FILTER-RAN");
        let cmd = format!(
            "{} '{}'; {}",
            t.get("touch").display(),
            ran.display(),
            t.get("cat").display()
        );
        (route.setup)(&r, route.driver, route.kind, &cmd);
        // Sanity: without the check's blanking, this very repository does run the filter
        // (otherwise the test below would pass for a repository that never would have).
        std::thread::sleep(Duration::from_millis(1100));
        let now = fs::read(r.join("host.nix")).unwrap();
        fs::write(r.join("host.nix"), now).unwrap();
        if route.kind == "clean" {
            real_git(&r, &["status", "--porcelain"]);
            assert!(
                ran.exists(),
                "{}: an ordinary `git status` did not run the filter, so this route proves nothing",
                route.name
            );
            fs::remove_file(&ran).unwrap();
            fs::write(r.join("host.nix"), "1\n").unwrap();
            std::thread::sleep(Duration::from_millis(1100));
            let now = fs::read(r.join("host.nix")).unwrap();
            fs::write(r.join("host.nix"), now).unwrap();
        }

        let answer = real_tree(&r)
            .await
            .unwrap_or_else(|why| panic!("{}: {why}", route.name));
        assert_eq!(answer.changed_files, 0, "{}: same content", route.name);
        assert!(
            !ran.exists(),
            "{}: the status check ran the repository's filter",
            route.name
        );
        // And a real change is still seen.
        fs::write(r.join("host.nix"), "2\n").unwrap();
        let answer = real_tree(&r).await.unwrap();
        assert_eq!(answer.changed_files, 1, "{}", route.name);
        assert!(!ran.exists(), "{}: ran on a changed file", route.name);
    }
}

#[tokio::test]
async fn a_sha256_repository_is_checked_like_any_other() {
    let Some(_t) = with_real_git() else { return };
    let dir = tempdir();
    let remote = tempdir();
    let r = dir.path();
    let init = Command::new("git")
        .current_dir(r)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["init", "-q", "-b", "main", "--object-format=sha256"])
        .output()
        .unwrap();
    if !init.status.success() {
        assert!(!in_nix_build(), "git cannot make a SHA-256 repository");
        eprintln!("SKIPPED: this git has no SHA-256 repositories");
        return;
    }
    real_git(
        remote.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--object-format=sha256",
        ],
    );
    fs::write(r.join("host.nix"), "1\n").unwrap();
    real_git(r, &["add", "host.nix"]);
    real_git(r, &["commit", "-q", "-m", "one"]);
    real_git(
        r,
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    real_git(r, &["push", "-q", "-u", "origin", "main"]);
    assert_eq!(
        real_tree(r).await,
        Ok(TreeState {
            changed_files: 0,
            upstream: tracking(0, 0)
        })
    );
    fs::write(r.join("host.nix"), "2\n").unwrap();
    real_git(r, &["commit", "-q", "-a", "-m", "two"]);
    fs::write(r.join("host.nix"), "3\n").unwrap();
    assert_eq!(
        real_tree(r).await,
        Ok(TreeState {
            changed_files: 1,
            upstream: tracking(1, 0)
        })
    );
}

#[tokio::test]
async fn a_filter_driver_that_cannot_be_blanked_makes_the_check_not_run() {
    let Some(_t) = with_real_git() else { return };
    let dir = tempdir();
    let r = dir.path();
    real_git(r, &["init", "-q", "-b", "main"]);
    fs::write(r.join("host.nix"), "1\n").unwrap();
    real_git(r, &["add", "host.nix"]);
    real_git(r, &["commit", "-q", "-m", "one"]);
    // `-c key=value` splits at the first `=`, so a driver named `a=b` cannot be turned off.
    fs::write(
        r.join(".git/config"),
        format!(
            "{}\n[filter \"a=b\"]\n\tclean = cat\n",
            fs::read_to_string(r.join(".git/config")).unwrap()
        ),
    )
    .unwrap();
    let why = real_tree(r).await.unwrap_err();
    assert!(why.contains("cannot be turned off"), "{why}");
}

// ---------------------------------------------------------------------------------------
// Fix round 2 (re-review 3, 4): a driver that is not UTF-8, and a required one
// ---------------------------------------------------------------------------------------

/// A repository whose `host.nix` has had its stat changed without its content, with a filter
/// driver named by raw bytes in `.git/config` and `.git/info/attributes`.
fn repo_with_raw_driver(t: &Tools, dir: &Path, name: &[u8], extra: &str) -> (PathBuf, PathBuf) {
    let r = dir.join("repo");
    fs::create_dir(&r).unwrap();
    real_git(&r, &["init", "-q", "-b", "main"]);
    fs::write(r.join("host.nix"), "1\n").unwrap();
    real_git(&r, &["add", "host.nix"]);
    real_git(&r, &["commit", "-q", "-m", "one"]);
    let ran = dir.join("FILTER-RAN");
    let mut config = fs::read(r.join(".git/config")).unwrap();
    config.extend_from_slice(b"[filter \"");
    config.extend_from_slice(name);
    config.extend_from_slice(
        format!(
            "\"]\n\tclean = {} '{}'; {}\n{extra}",
            t.get("touch").display(),
            ran.display(),
            t.get("cat").display()
        )
        .as_bytes(),
    );
    fs::write(r.join(".git/config"), config).unwrap();
    let mut attrs = b"* filter=".to_vec();
    attrs.extend_from_slice(name);
    attrs.push(b'\n');
    fs::create_dir_all(r.join(".git/info")).unwrap();
    fs::write(r.join(".git/info/attributes"), attrs).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let now = fs::read(r.join("host.nix")).unwrap();
    fs::write(r.join("host.nix"), now).unwrap();
    (r, ran)
}

#[tokio::test]
async fn a_filter_driver_whose_name_is_not_utf8_makes_the_check_not_run_and_runs_nothing() {
    let Some(t) = with_real_git() else { return };
    for name in [&b"\xff"[..], b"ev\xc3il"] {
        let dir = tempdir();
        let (r, ran) = repo_with_raw_driver(&t, dir.path(), name, "");
        // The sanity check: an ordinary status does run this driver.
        real_git(&r, &["status", "--porcelain"]);
        assert!(ran.exists(), "{name:?}: the route proves nothing");
        fs::remove_file(&ran).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        let now = fs::read(r.join("host.nix")).unwrap();
        fs::write(r.join("host.nix"), now).unwrap();

        let why = real_tree(&r).await.unwrap_err();
        assert!(why.contains("not UTF-8"), "{name:?}: {why}");
        assert!(!ran.exists(), "{name:?}: the filter ran");
    }
}

#[tokio::test]
async fn a_required_filter_is_turned_off_and_the_check_still_answers() {
    let Some(t) = with_real_git() else { return };
    let dir = tempdir();
    // `git lfs install` writes `required = true` into the operator's global config.
    let (r, ran) = repo_with_raw_driver(&t, dir.path(), b"lfs", "\trequired = true\n");
    // Without the blanking this repository's status fails outright once the driver has no
    // command (the sanity check that `required` is what is being tested).
    let blanked = Command::new("git")
        .current_dir(&r)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "filter.lfs.clean=", "status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        !blanked.status.success(),
        "an emptied required driver must be an error without `required=false`: {blanked:?}"
    );
    fs::remove_file(&ran).ok();

    let answer = real_tree(&r).await.unwrap();
    assert_eq!(answer.changed_files, 0, "same content: not a change");
    assert!(!ran.exists(), "the filter ran");
    // And a real change is seen.
    fs::write(r.join("host.nix"), "2\n").unwrap();
    assert_eq!(real_tree(&r).await.unwrap().changed_files, 1);
}
