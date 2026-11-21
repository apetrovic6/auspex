//! `kubectl`, against a fake: what is run, what its answers become, and that a timed-out or
//! cancelled one is killed and reaped. No real `kubectl`, kubeconfig or cluster is touched; the
//! kubeconfig path in these tests names a file that does not exist, and the fake never reads it.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use auspex_effects::kube::{KubeParams, drain, nodes, set_cordon};
use auspex_effects::{CancellationToken, Cancelled};
use common::{Tools, cleanup, still_alive_after_wait, tempdir, tools, wait_for_pids};
use tokio::sync::mpsc::unbounded_channel;

const KUBECONFIG: &str = "/nonexistent/auspex-test/kubeconfig";

/// A fake kubectl: records each call's argv (one line per argument, then `--`) and then runs
/// `body`.
fn fake(t: &Tools, dir: &Path, body: &str) -> PathBuf {
    t.script(
        dir,
        "kubectl",
        &format!(
            "printf '%s\\n' \"$@\" >> '@dir@/argv'\nprintf -- '--\\n' >> '@dir@/argv'\n{body}"
        ),
    )
}

fn params(program: &Path, timeout: Duration) -> KubeParams<'_> {
    KubeParams {
        timeout,
        drain_grace: Duration::from_millis(300),
        ..KubeParams::new(program.as_os_str(), Path::new(KUBECONFIG))
    }
}

fn argv_log(dir: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(dir.join("argv"))
        .unwrap_or_default()
        .split("--\n")
        .filter(|c| !c.is_empty())
        .map(|c| c.lines().map(str::to_owned).collect())
        .collect()
}

const LEASES: &str = r#"{"items":[{"metadata":{"name":"luna"},"spec":{"renewTime":"2026-11-20T15:00:30.000000Z"}}]}"#;

const LIST: &str = r#"{"items":[{"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},{"metadata":{"name":"sol"},"spec":{"unschedulable":true},"status":{"conditions":[{"type":"Ready","status":"False"}]}}]}"#;

#[tokio::test]
async fn the_node_list_is_asked_with_the_exact_argv_and_parsed() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake(
        &t,
        dir.path(),
        &format!(
            "case \" $* \" in\n\
             *\" get leases \"*) cat <<'EOF'\n{LEASES}\nEOF\n;;\n\
             *) cat <<'EOF'\n{LIST}\nEOF\n;;\nesac"
        ),
    );
    let nodes = nodes(
        &params(&program, Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        argv_log(dir.path()),
        [
            vec!["--kubeconfig", KUBECONFIG, "get", "nodes", "-o", "json"],
            vec![
                "--kubeconfig",
                KUBECONFIG,
                "get",
                "leases",
                "-n",
                "kube-node-lease",
                "-o",
                "json"
            ],
        ]
    );
    // luna has a lease renewed at 15:00:30; sol has none.
    assert_eq!(nodes[0].heartbeat, Some(1_795_186_830));
    assert_eq!(nodes[1].heartbeat, None);
    let seen: Vec<(&str, bool, bool)> = nodes
        .iter()
        .map(|n| (n.name.as_str(), n.ready, n.cordoned))
        .collect();
    assert_eq!(seen, [("luna", true, false), ("sol", false, true)]);
}

#[tokio::test]
async fn a_failing_or_garbled_kubectl_is_a_line_not_an_empty_cluster() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let failing = fake(
        &t,
        dir.path(),
        "echo 'Unable to connect to the server: dial tcp 10.0.0.1:6443: i/o timeout' >&2\nexit 1",
    );
    let why = nodes(
        &params(&failing, Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        why.contains("exited 1") && why.contains("Unable to connect"),
        "{why}"
    );

    let garbled = fake(&t, dir.path(), "echo 'No resources found'");
    let why = nodes(
        &params(&garbled, Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("not the expected JSON"), "{why}");

    let missing = dir.path().join("no-such-kubectl");
    let why = nodes(
        &params(&missing, Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("cannot run kubectl get nodes"), "{why}");
}

#[tokio::test]
async fn cordon_and_uncordon_name_exactly_one_node() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake(&t, dir.path(), "echo 'node/luna cordoned'");
    let p = params(&program, Duration::from_secs(10));
    let cancel = CancellationToken::new();
    set_cordon(&p, "luna", true, &cancel)
        .await
        .unwrap()
        .unwrap();
    set_cordon(&p, "luna", false, &cancel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        argv_log(dir.path()),
        [
            ["--kubeconfig", KUBECONFIG, "cordon", "luna"],
            ["--kubeconfig", KUBECONFIG, "uncordon", "luna"],
        ]
    );
    // A name that could act on more is refused, and nothing is run for it.
    let before = argv_log(dir.path()).len();
    for bad in ["--all", "luna sol", ""] {
        assert!(
            set_cordon(&p, bad, true, &cancel).await.unwrap().is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(argv_log(dir.path()).len(), before);
}

#[tokio::test]
async fn a_drain_is_the_exact_argv_and_its_output_is_streamed() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake(
        &t,
        dir.path(),
        "echo 'node/luna cordoned'\necho 'evicting pod default/web-1' >&2\necho 'node/luna drained'",
    );
    let (tx, mut rx) = unbounded_channel();
    drain(
        &params(&program, Duration::from_secs(10)),
        "luna",
        600,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        argv_log(dir.path()),
        [[
            "--kubeconfig",
            KUBECONFIG,
            "drain",
            "luna",
            "--ignore-daemonsets",
            "--delete-emptydir-data",
            "--timeout",
            "600s"
        ]]
    );
    let mut lines = Vec::new();
    while let Ok(l) = rx.try_recv() {
        lines.push(l.text);
    }
    lines.sort();
    assert_eq!(
        lines,
        [
            "evicting pod default/web-1",
            "node/luna cordoned",
            "node/luna drained"
        ]
    );
}

#[tokio::test]
async fn a_failed_drain_is_an_error_with_the_exit_code() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake(
        &t,
        dir.path(),
        "echo 'error when evicting pods: cannot evict pod as it would violate the budget' >&2\nexit 1",
    );
    let (tx, _rx) = unbounded_channel();
    let why = drain(
        &params(&program, Duration::from_secs(10)),
        "luna",
        5,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(why, "kubectl drain exited 1");
}

/// kubectl enforces `--timeout` itself; this is the backstop for one that hangs, and it kills
/// the whole group, not only the leader.
#[tokio::test]
async fn a_drain_that_hangs_is_killed_after_its_timeout_and_the_grace() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let pids = dir.path().join("pids");
    let program = fake(
        &t,
        dir.path(),
        &format!(
            "printf '%s\\n' $$ >> '{p}'\n@sleep@ 30 &\nprintf '%s\\n' $! >> '{p}'\nwait",
            p = pids.display()
        ),
    );
    let (tx, _rx) = unbounded_channel();
    let started = Instant::now();
    let why = drain(
        &params(&program, Duration::from_secs(10)),
        "luna",
        1,
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("did not finish within 1s"), "{why}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // The backstop is kubectl's own timeout plus the grace (300 ms here), not the timeout alone:
    // kubectl enforces its own, and is given the time to say so.
    assert!(
        started.elapsed() >= Duration::from_millis(1250),
        "killed before the grace: {:?}",
        started.elapsed()
    );
    let gone = still_alive_after_wait(&wait_for_pids(&pids, 2));
    assert_eq!(gone, Vec::<i32>::new(), "left running after the timeout");
}

#[tokio::test]
async fn a_cancelled_call_kills_and_reaps_kubectl() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let pids = dir.path().join("pids");
    let program = fake(
        &t,
        dir.path(),
        &format!(
            "printf '%s\\n' $$ >> '{p}'\n@sleep@ 30 &\nprintf '%s\\n' $! >> '{p}'\nwait",
            p = pids.display()
        ),
    );
    for which in ["nodes", "drain"] {
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
        let p = params(&program, Duration::from_secs(60));
        let answer = if which == "nodes" {
            nodes(&p, &cancel).await.map(|_| ())
        } else {
            let (tx, _rx) = unbounded_channel();
            drain(&p, "luna", 600, &cancel, tx).await.map(|_| ())
        };
        assert_eq!(answer, Err(Cancelled), "{which}");
        let spawned = canceller.await.unwrap();
        let left = still_alive_after_wait(&spawned);
        cleanup(&left);
        assert_eq!(
            left,
            Vec::<i32>::new(),
            "{which}: left running after a cancel"
        );
    }
}

/// The program is found on `PATH` by name, and an absolute kubeconfig is required.
#[tokio::test]
async fn a_relative_kubeconfig_runs_nothing() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake(&t, dir.path(), "echo '{\"items\":[]}'");
    let p = KubeParams::new(program.as_os_str(), Path::new("kube/config"));
    let why = nodes(&p, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert!(why.contains("not an absolute path"), "{why}");
    assert!(argv_log(dir.path()).is_empty(), "nothing was run");
}

/// A cluster whose leases cannot be read still has its nodes: the lease is an extra sign of life,
/// and a caller that waits for one says the node is stale, instead of the whole call failing.
#[tokio::test]
async fn a_lease_list_that_fails_leaves_the_nodes_without_lease_heartbeats() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake(
        &t,
        dir.path(),
        &format!(
            "case \" $* \" in\n\
             *\" get leases \"*) echo 'forbidden' >&2; exit 1;;\n\
             *) cat <<'EOF'\n{LIST}\nEOF\n;;\nesac"
        ),
    );
    let nodes = nodes(
        &params(&program, Duration::from_secs(10)),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].heartbeat, None);
}

#[tokio::test]
async fn the_cluster_is_asked_which_it_is_and_every_pinned_call_carries_the_context() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let view = r#"{"current-context":"rke2","contexts":[{"name":"rke2","context":{"cluster":"c1"}}],"clusters":[{"name":"c1","cluster":{"server":"https://10.0.0.1:6443"}}]}"#;
    let program = fake(&t, dir.path(), &format!("cat <<'EOF'\n{view}\nEOF"));
    let mut p = params(&program, Duration::from_secs(10));
    let id = auspex_effects::kube::cluster_id(&p, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (id.context.as_str(), id.server.as_str()),
        ("rke2", "https://10.0.0.1:6443")
    );
    p.context = Some("rke2");
    set_cordon(&p, "luna", false, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    let log = argv_log(dir.path());
    assert_eq!(
        log[0],
        [
            "--kubeconfig",
            KUBECONFIG,
            "config",
            "view",
            "--minify",
            "-o",
            "json"
        ]
    );
    assert_eq!(
        log[1],
        [
            "--kubeconfig",
            KUBECONFIG,
            "--context=rke2",
            "uncordon",
            "luna"
        ]
    );
}
