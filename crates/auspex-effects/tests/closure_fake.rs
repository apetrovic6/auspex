//! The package preview's two closure queries against fake `ssh`, `nix` and `nix-store`
//! programs (scripts passed as the program). Nothing here contacts a machine, a cache or the
//! real store.
//!
//! The fakes record their argv and stdin, so the tests pin what is run, not only what comes
//! back: the deployed query is the probe's ssh argv plus the domain's script on stdin, the
//! local query is `nix-store --query --requisites <path>`, and the cache query is
//! `nix path-info --recursive --store <url> -- <path>`.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use auspex_domain::closure::{DEPLOYED_QUERY_SCRIPT, parse_closure};
use auspex_domain::fleet::Target;
use auspex_domain::observe::StorePath;
use auspex_effects::closure::{
    ClosureAnswer, PendingParams, STDOUT_CAP, cache_argv, deployed, local_argv, pending,
};
use auspex_effects::ssh::{ControlDir, ProbeParams, probe_argv};
use auspex_effects::{CancellationToken, Cancelled};
use common::{cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};

const GLIBC: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-glibc-2.40-66";
const SYSTEMD: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-systemd-256.10";
const SYSTEM: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-25.11";

fn control(dir: &Path) -> ControlDir {
    ControlDir::prepare(&dir.join("c")).unwrap()
}

/// A fake ssh over an already-authenticated master (`-O check` succeeds). `body` is what the
/// query run does; it first records its argv and stdin.
fn warm_ssh(t: &common::Tools, dir: &Path, body: &str) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh-warm",
        &format!(
            r#"for arg; do [ "$arg" = '-O' ] && exit 0; done
printf '%s\n' "$@" > '@dir@/argv'
@cat@ > '@dir@/stdin'
{body}"#
        ),
    )
}

fn paths(closure: &ClosureAnswer) -> Vec<&str> {
    closure
        .as_ref()
        .unwrap()
        .paths
        .iter()
        .map(StorePath::as_str)
        .collect()
}

#[tokio::test]
async fn the_deployed_query_is_the_probes_argv_with_the_domains_script_on_stdin() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = warm_ssh(
        &t,
        dir.path(),
        &format!("printf '%s\\n' '{GLIBC}' '{SYSTEMD}' '{SYSTEM}'"),
    );
    let cd = control(dir.path());
    let target = Target::parse("root@192.0.2.7:2222?Compression=yes").unwrap();
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let answer = deployed(&target, &params, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(paths(&answer), [GLIBC, SYSTEMD, SYSTEM]);
    assert_eq!(answer.as_ref().unwrap().dropped, 0);
    // Exactly the probe's argv: the same options, port, `--`, destination, and the remote
    // command, with nothing added by this query.
    let argv = fs::read_to_string(dir.path().join("argv")).unwrap();
    let expected = probe_argv(&target, &params).unwrap().join("\n") + "\n";
    assert_eq!(argv, expected);
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        DEPLOYED_QUERY_SCRIPT
    );
}

#[tokio::test]
async fn lines_that_are_not_store_paths_are_dropped_and_counted() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = warm_ssh(
        &t,
        dir.path(),
        &format!(
            "printf '%s\\n' 'Last login: never' '{GLIBC}' '' '/etc/passwd' '{SYSTEM}\u{1b}[31m'"
        ),
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let answer = deployed(
        &Target::parse("h").unwrap(),
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(paths(&answer), [GLIBC]);
    assert_eq!(answer.unwrap().dropped, 3);
}

#[tokio::test]
async fn an_answer_with_no_store_path_is_an_error_not_an_empty_closure() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = warm_ssh(&t, dir.path(), "printf 'hello\\n'");
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let answer = deployed(
        &Target::parse("h").unwrap(),
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let why = answer.unwrap_err();
    assert!(
        why.contains("printed no store paths (1 lines dropped)"),
        "{why}"
    );
}

#[tokio::test]
async fn a_failing_query_names_its_exit_and_the_last_stderr_line() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = warm_ssh(
        &t,
        dir.path(),
        "printf 'error: path /run/current-system is not valid\\n' >&2\nexit 1",
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let answer = deployed(
        &Target::parse("h").unwrap(),
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        answer.unwrap_err(),
        "deployed closure query exited 1: error: path /run/current-system is not valid"
    );
}

#[tokio::test]
async fn output_past_the_cap_kills_the_query_and_is_not_compared() {
    let Some(t) = tools(&["cat", "yes"]) else {
        return;
    };
    let dir = tempdir();
    let program = warm_ssh(
        &t,
        dir.path(),
        &format!("echo $$ > '@dir@/pid'\nexec @yes@ '{GLIBC}'"),
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let started = Instant::now();
    let answer = deployed(
        &Target::parse("h").unwrap(),
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let pids = wait_for_pids(&dir.path().join("pid"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(
        answer.unwrap_err(),
        "deployed closure query printed more than 4 MiB; not compared"
    );
    assert!(left.is_empty(), "the capped query is killed: {left:?}");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "stopped at the cap, not at the timeout"
    );
    assert_eq!(STDOUT_CAP, 4 * 1024 * 1024);
}

#[tokio::test]
async fn a_query_past_its_deadline_is_killed_and_reaped() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let program = warm_ssh(&t, dir.path(), "echo $$ > '@dir@/pid'\nexec @sleep@ 30");
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.deadline = Duration::from_millis(700);
    let started = Instant::now();
    let answer = deployed(
        &Target::parse("h").unwrap(),
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let pids = wait_for_pids(&dir.path().join("pid"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(
        answer
            .unwrap_err()
            .starts_with("deployed closure query timed out after 700ms"),
    );
    assert!(left.is_empty(), "{left:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancelling_kills_and_reaps_the_deployed_query() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    // The ssh child forks a grandchild, as `ProxyJump` would: the whole group must go.
    let program = warm_ssh(
        &t,
        dir.path(),
        "echo $$ > '@dir@/pid'\n@sleep@ 30 &\necho $! >> '@dir@/pid'\nwait",
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let cancel = CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let path = dir.path().join("pid");
        tokio::task::spawn_blocking(move || {
            wait_for_pids(&path, 2);
            cancel.cancel();
        })
    };
    let answer = deployed(&Target::parse("h").unwrap(), &params, &cancel).await;
    canceller.await.unwrap();
    let pids = wait_for_pids(&dir.path().join("pid"), 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(answer, Err(Cancelled));
    assert!(left.is_empty(), "the group is killed: {left:?}");
}

/// A fake master that appears only after the simulated approval, as `ssh.rs`'s tests do.
fn cold_ssh(t: &common::Tools, dir: &Path) -> std::path::PathBuf {
    t.script(
        dir,
        "ssh-cold",
        &format!(
            r#"for arg; do
  case "$arg" in
    -O) [ -f '@dir@/master' ]; exit $?;;
    true)
      echo $$ >> '@dir@/connections'
      @sleep@ 1
      : > '@dir@/master'
      exit 0;;
  esac
done
[ -f '@dir@/master' ] || exit 255
@cat@ > /dev/null
printf '%s\n' '{GLIBC}'
"#
        ),
    )
}

#[tokio::test]
async fn a_warm_master_is_reused_and_a_cold_one_is_approved_once_for_both_queries() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let program = cold_ssh(&t, dir.path());
    let cd = control(dir.path());
    let target = Target::parse("h").unwrap();
    let phases = std::sync::Mutex::new(Vec::new());
    let signal = |waiting| phases.lock().unwrap().push(waiting);
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    params.on_approval = Some(&signal);
    for _ in 0..2 {
        let answer = deployed(&target, &params, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(paths(&answer), [GLIBC]);
    }
    // One approval phase, entered and left; the second query found the master the first one
    // opened and never connected.
    assert_eq!(*phases.lock().unwrap(), [true, false]);
    assert_eq!(
        fs::read_to_string(dir.path().join("connections"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn a_dead_host_is_the_connection_failure_not_a_closure() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = t.script(
        dir.path(),
        "ssh-dead",
        "printf 'ssh: connect to host h port 22: Connection refused\\n' >&2\nexit 255",
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let answer = deployed(
        &Target::parse("h").unwrap(),
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let why = answer.unwrap_err();
    assert!(why.starts_with("deployed closure: "), "{why}");
    assert!(why.contains("Connection refused"), "{why}");
}

// ---------------------------------------------------------------------------------------
// The pending closure
// ---------------------------------------------------------------------------------------

fn store_path() -> StorePath {
    StorePath::new(SYSTEM).unwrap()
}

struct Fakes {
    nix: std::path::PathBuf,
    nix_store: std::path::PathBuf,
}

/// A fake `nix-store` (`store_body`) and `nix` (`nix_body`), each recording its argv, and
/// writing a marker file when it runs at all.
fn fakes(t: &common::Tools, dir: &Path, store_body: &str, nix_body: &str) -> Fakes {
    Fakes {
        nix_store: t.script(
            dir,
            "nix-store",
            &format!("printf '%s\\n' \"$@\" > '@dir@/store-argv'\n{store_body}"),
        ),
        nix: t.script(
            dir,
            "nix",
            &format!("printf '%s\\n' \"$@\" > '@dir@/nix-argv'\n{nix_body}"),
        ),
    }
}

fn params<'a>(f: &'a Fakes, cache_url: Option<&'a str>, timeout: Duration) -> PendingParams<'a> {
    PendingParams {
        nix: f.nix.as_os_str(),
        nix_store: f.nix_store.as_os_str(),
        cache_url,
        timeout,
    }
}

#[tokio::test]
async fn a_path_valid_in_the_local_store_is_queried_locally_and_the_cache_is_not_asked() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let f = fakes(
        &t,
        dir.path(),
        &format!("printf '%s\\n' '{GLIBC}' '{SYSTEM}'"),
        "exit 99",
    );
    let answer = pending(
        &store_path(),
        &params(&f, Some("https://cache.example"), Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(paths(&answer), [GLIBC, SYSTEM]);
    assert_eq!(
        fs::read_to_string(dir.path().join("store-argv")).unwrap(),
        format!("--query\n--requisites\n{SYSTEM}\n")
    );
    assert!(
        !dir.path().join("nix-argv").exists(),
        "no network query when the local store has it"
    );
    assert_eq!(
        local_argv(&store_path()),
        ["--query", "--requisites", SYSTEM]
    );
}

#[tokio::test]
async fn otherwise_the_cache_is_queried_read_only_with_exactly_this_argv() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let f = fakes(
        &t,
        dir.path(),
        &format!("printf 'error: path %s is not valid\\n' '{SYSTEM}' >&2\nexit 1"),
        &format!("printf '%s\\n' '{GLIBC}' '{SYSTEM}'"),
    );
    let answer = pending(
        &store_path(),
        &params(&f, Some("https://cache.example"), Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(paths(&answer), [GLIBC, SYSTEM]);
    // `--store` names the cache, so nothing is written to the local store (nix does keep its
    // own narinfo cache under `~/.cache/nix`, without negative entries: ttl 0); the path
    // follows `--`, so it cannot be read as an option.
    assert_eq!(
        fs::read_to_string(dir.path().join("nix-argv")).unwrap(),
        format!(
            "path-info\n--recursive\n--option\nnarinfo-cache-negative-ttl\n0\n--store\nhttps://cache.example/\n--\n{SYSTEM}\n"
        )
    );
    assert_eq!(
        cache_argv("https://cache.example/", &store_path()),
        [
            "path-info",
            "--recursive",
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
async fn both_failing_says_so_and_names_both() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let f = fakes(
        &t,
        dir.path(),
        "printf 'error: path is not valid\\n' >&2\nexit 1",
        "printf 'error: unable to download narinfo\\n' >&2\nexit 1",
    );
    let why = pending(
        &store_path(),
        &params(&f, Some("https://cache.example"), Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("not in the local store"), "{why}");
    assert!(why.contains("path is not valid"), "{why}");
    assert!(why.contains("cache query exited 1"), "{why}");
    assert!(why.contains("unable to download narinfo"), "{why}");
}

#[tokio::test]
async fn no_cache_configured_never_runs_nix() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let f = fakes(&t, dir.path(), "exit 1", "exit 99");
    let why = pending(
        &store_path(),
        &params(&f, None, Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("no binary cache is configured"), "{why}");
    assert!(!dir.path().join("nix-argv").exists());
}

#[tokio::test]
async fn an_unusable_cache_url_is_refused_before_anything_runs() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let f = fakes(&t, dir.path(), "exit 1", "exit 99");
    for url in [
        "file:///etc",
        "ssh://host/x",
        "https://user:pw@cache.example",
        "-oProxyCommand=x",
        "",
    ] {
        let why = pending(
            &store_path(),
            &params(&f, Some(url), Duration::from_secs(10)),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(why.contains("the cache URL is unusable"), "{url}: {why}");
    }
    assert!(!dir.path().join("nix-argv").exists());
}

#[tokio::test]
async fn pending_output_past_the_cap_is_killed_and_not_compared() {
    let Some(t) = tools(&["yes"]) else { return };
    let dir = tempdir();
    let f = fakes(
        &t,
        dir.path(),
        "exit 1",
        &format!("echo $$ > '@dir@/pid'\nexec @yes@ '{GLIBC}'"),
    );
    let why = pending(
        &store_path(),
        &params(&f, Some("https://cache.example"), Duration::from_secs(20)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    let pids = wait_for_pids(&dir.path().join("pid"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(why.contains("cache query printed more than 4 MiB"), "{why}");
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_pending_query_past_its_deadline_is_killed_and_reaped() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let f = fakes(
        &t,
        dir.path(),
        "exit 1",
        "echo $$ > '@dir@/pid'\nexec @sleep@ 30",
    );
    let started = Instant::now();
    let why = pending(
        &store_path(),
        &params(
            &f,
            Some("https://cache.example"),
            Duration::from_millis(800),
        ),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    let pids = wait_for_pids(&dir.path().join("pid"), 1);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(why.contains("cache query timed out after 800ms"), "{why}");
    assert!(left.is_empty(), "{left:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_slow_local_query_spends_the_budget_and_the_cache_is_not_asked() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let f = fakes(&t, dir.path(), "exec @sleep@ 30", "exit 99");
    let why = pending(
        &store_path(),
        &params(
            &f,
            Some("https://cache.example"),
            Duration::from_millis(600),
        ),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("timed out after 600ms"), "{why}");
    assert!(!dir.path().join("nix-argv").exists());
}

#[tokio::test]
async fn cancelling_kills_and_reaps_the_pending_query_and_its_children() {
    let Some(t) = tools(&["sleep"]) else { return };
    let dir = tempdir();
    let f = fakes(
        &t,
        dir.path(),
        "exit 1",
        "echo $$ > '@dir@/pid'\n@sleep@ 30 &\necho $! >> '@dir@/pid'\nwait",
    );
    let cancel = CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let path = dir.path().join("pid");
        tokio::task::spawn_blocking(move || {
            wait_for_pids(&path, 2);
            cancel.cancel();
        })
    };
    let answer = pending(
        &store_path(),
        &params(&f, Some("https://cache.example"), Duration::from_secs(60)),
        &cancel,
    )
    .await;
    canceller.await.unwrap();
    let pids = wait_for_pids(&dir.path().join("pid"), 2);
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert_eq!(answer, Err(Cancelled));
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn what_the_fakes_print_is_what_the_domain_reads() {
    // The fakes print one path per line; so do `nix-store -qR` and `nix path-info -r`.
    let closure = parse_closure(&format!("{GLIBC}\n{SYSTEMD}\n"));
    assert_eq!(closure.paths.len(), 2);
}

#[tokio::test]
async fn the_deployed_query_never_prompts_and_has_no_controlling_terminal() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = warm_ssh(
        &t,
        dir.path(),
        &format!(
            "printf '%s|%s\\n' \"${{SSH_ASKPASS_REQUIRE-unset}}\" \"${{SSH_ASKPASS-unset}}\" > '@dir@/env'\n\
             if (exec 3</dev/tty) 2>/dev/null; then echo tty > '@dir@/tty'; fi\n\
             printf '%s\\n' '{GLIBC}'"
        ),
    );
    let cd = control(dir.path());
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = program.as_os_str();
    let answer = deployed(
        &Target::parse("h").unwrap(),
        &params,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(paths(&answer), [GLIBC]);
    assert_eq!(
        fs::read_to_string(dir.path().join("env")).unwrap(),
        "never|unset\n",
        "SSH_ASKPASS_REQUIRE=never, SSH_ASKPASS removed"
    );
    assert!(!dir.path().join("tty").exists(), "a session of its own");
}

#[tokio::test]
async fn a_local_store_closure_over_the_cap_fails_and_does_not_fall_through_to_the_cache() {
    let Some(t) = tools(&["yes"]) else { return };
    let dir = tempdir();
    let f = fakes(&t, dir.path(), &format!("exec @yes@ '{GLIBC}'"), "exit 99");
    let why = pending(
        &store_path(),
        &params(&f, Some("https://cache.example"), Duration::from_secs(20)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("printed more than 4 MiB"), "{why}");
    assert!(
        !dir.path().join("nix-argv").exists(),
        "an oversized local closure is not retried over the network"
    );
}

/// Review of PR #37, re-review B: the deployed-closure query starts an ssh in the control
/// directory like the probe does, and refuses a directory that is no longer the prepared one. A
/// fake ssh whose master check replaces the directory and then answers "master up" leaves only
/// the query's own spawn to refuse it; the query must not run.
#[tokio::test]
async fn the_deployed_closure_query_rechecks_the_control_directory() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let prog = t.script(
        dir.path(),
        "ssh-replacing",
        "echo run >> '@dir@/ran'\nfor arg; do\n  if [ \"$arg\" = -O ]; then\n    mv '@dir@/c' '@dir@/c.aside' && mkdir -m 700 '@dir@/c'\n    exit 0\n  fi\ndone\necho query >> '@dir@/ran'",
    );
    let cd = control(dir.path());
    let target = Target::parse("h").unwrap();
    let mut params = ProbeParams::new(&cd);
    params.ssh_program = prog.as_os_str();
    let answer = deployed(&target, &params, &CancellationToken::new())
        .await
        .unwrap();
    let why = answer.unwrap_err();
    assert!(why.contains("deployed closure"), "{why}");
    assert!(why.contains("not the directory auspex made"), "{why}");
    let ran = fs::read_to_string(dir.path().join("ran")).unwrap();
    assert_eq!(
        ran.lines().collect::<Vec<_>>(),
        ["run"],
        "only the check may run"
    );
}
