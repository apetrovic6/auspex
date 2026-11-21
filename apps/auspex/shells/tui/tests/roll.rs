//! `U` for real: the core, the shell and the adapters over the demo's scripted fleet, with a fake
//! `clan` and a fake `kubectl` standing in for the programs. What is run and in what order, that
//! two machines of one rolling tag are never updating at the same moment (the fake `clan`
//! records when each update starts and ends, across two auspex processes too), the node reads,
//! waits and drains, and what a quit, a signal and a forced quit do to the processes and to a
//! cordoned node.
//!
//! The clock is real (processes and timers must not be fast-forwarded). In the demo, `luna` and
//! `sol` are the machines with an update pending that carry the tag `k8s-server`, and
//! `cypramundi` is pending and public; here it is made an ordinary machine, so it is the free one.
//!
//! Nothing here runs the real `clan`, `nix` or `kubectl`, ssh, or touches a real cluster, fleet or
//! state directory.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use auspex_core::{ConfirmKind, Event, RollState, RollStatus, ViewModel};
use auspex_domain::roll::RollRule;
use auspex_effects::deploylog::{
    ActionKind as LoggedKind, ActionLog, ActionResult as LoggedResult, Identity, OpenParams,
};
use auspex_effects::freeze::FreezeStore;
use auspex_tui::backend::{Backend, Real};
use auspex_tui::demo::{self, Demo};
use auspex_tui::log::Log;
use auspex_tui::shell::Shell;
use common::{Tools, alive, cleanup, still_alive_after_wait, tempdir, tools};

struct World {
    dir: tempfile::TempDir,
    fleet: tempfile::TempDir,
    shell: Arc<Shell>,
    db: PathBuf,
}

/// The kubeconfig file name: a space and a single quote in it, so that every command the shell
/// builds by hand must be quoted to be pasted.
const KUBECONFIG: &str = "kube config's";

impl World {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The lines `kubectl` was called with, one call per line, without the lease reads that
    /// accompany every node read.
    fn kubectl_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("kubectl.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.contains(" get leases "))
            .map(str::to_owned)
            .collect()
    }

    /// `start <machine>` and `end <machine>` lines, in the order the fake `clan` wrote them.
    fn clan_events(&self) -> Vec<String> {
        lines(&self.path("clan.log"))
    }

    /// A call as the log shows it, for this world's kubeconfig.
    fn kc(&self, rest: &str) -> String {
        format!(
            "--kubeconfig {}/{KUBECONFIG} {rest}",
            self.dir.path().display()
        )
    }

    /// The same, pinned to the context the plan named.
    fn kcx(&self, rest: &str) -> String {
        self.kc(&format!("--context=rke2 {rest}"))
    }
}

fn lines(p: &Path) -> Vec<String> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn identity() -> Identity {
    Identity {
        who: "tester".to_owned(),
        host: "testhost".to_owned(),
        auspex_version: "0.0.0-test".to_owned(),
    }
}

const NODES_READY: &str = r#"{"items":[
  {"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"mars"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"terra"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}
]}"#;

const NODES_SOL_DOWN: &str = r#"{"items":[
  {"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"False"}]}}
]}"#;

/// mars (an etcd member that is not in the roll) has gone `NotReady` and sol was cordoned by someone.
const NODES_SICK: &str = r#"{"items":[
  {"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"mars"},"status":{"conditions":[{"type":"Ready","status":"False"}]}},
  {"metadata":{"name":"sol"},"spec":{"unschedulable":true},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"terra"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}
]}"#;

/// Ready, with a heartbeat from long before any update. sol's is nine minutes newer than luna's: with
/// no lease to go by, that is not silence (the condition is rewritten only every few minutes).
const NODES_STALE: &str = r#"{"items":[
  {"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True","lastHeartbeatTime":"2020-01-01T00:00:00Z"}]}},
  {"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True","lastHeartbeatTime":"2020-01-01T00:09:00Z"}]}}
]}"#;

const VIEW: &str = r#"{"current-context":"rke2","contexts":[{"name":"rke2","context":{"cluster":"c1"}}],"clusters":[{"name":"c1","cluster":{"server":"https://10.0.0.1:6443"}}]}"#;

/// The lease list the fake `kubectl` answers, in which every kubelet renews at each read: the
/// times are the kubelets' own, a fixed day plus one second per read, so they never come near this
/// machine's clock (which auspex must not compare them with), and each read is newer than the one
/// before it (a node is back only for a heartbeat newer than the one it had when the wait began).
/// `behind` names the kubelets that are that many seconds behind the others, from read number
/// `from_read` on (0: from the first). A node that is not in `nodes` has no lease.
fn leases(nodes: &[&str], behind: &[(&str, u32)], from_read: u32) -> String {
    use std::fmt::Write as _;
    let mut items = Vec::new();
    let mut args = Vec::new();
    let mut backs = String::new();
    for (i, node) in nodes.iter().enumerate() {
        let back = behind
            .iter()
            .find(|(b, _)| b == node)
            .map_or(0, |(_, secs)| *secs);
        let _ = write!(backs, "b{i}=0; [ \"$r\" -gt {from_read} ] && b{i}={back}; ");
        items.push(format!(
            r#"{{"metadata":{{"name":"{node}"}},"spec":{{"renewTime":"%s"}}}}"#
        ));
        // `printf` and shell arithmetic only: nothing here is GNU-only, and it needs no `date`.
        args.push(format!(
            r#""$(printf '2030-01-01T%02d:%02d:%02dZ' $(((n-b{i})/3600)) $(((n-b{i})/60%60)) $(((n-b{i})%60)))""#
        ));
    }
    format!(
        "f='@dir@/lease.reads'; r=$(@cat@ \"$f\" 2>/dev/null || echo 0); r=$((r+1)); \
         echo $r > \"$f\"; n=$((r+36000)); {backs}\
         printf '{{\"items\":[{}]}}\\n' {}",
        items.join(","),
        args.join(" ")
    )
}

/// Every kubelet of the demo cluster renews at every read.
fn leases_alive() -> String {
    leases(&["luna", "mars", "sol", "terra"], &[], 0)
}

/// The fake `clan`: records `start <machine>` and `end <machine>` around `sleep_seconds`, and
/// its own pid, for the tests that kill it. `log` is where `start`/`end` go (shared by two
/// auspex processes in the test that has two).
fn clan_body_to(log: &str, sleep_seconds: &str) -> String {
    format!(
        "for a in \"$@\"; do m=$a; done\n\
         printf '%s\\n' $$ >> '@dir@/clan.pids'\n\
         printf 'start %s\\n' \"$m\" >> '{log}'\n\
         echo \"deploying $m\"\n\
         @sleep@ {sleep_seconds}\n\
         printf 'end %s\\n' \"$m\" >> '{log}'\n\
         echo \"done $m\"\n"
    )
}

fn clan_body(sleep_seconds: &str) -> String {
    clan_body_to("@dir@/clan.log", sleep_seconds)
}

fn cat(json: &str) -> String {
    // `printf` of the text, not a here-document: the snippet may be followed by `; else`.
    format!("printf '%s\\n' '{json}'")
}

/// The fake `kubectl`: records its arguments, and answers each kind of call with the shell
/// snippets given: `get_nodes`, `drain`, `uncordon` and `leases`. `config view` always says the
/// kubeconfig points at the cluster `rke2`.
fn kubectl_with(get_nodes: &str, drain: &str, uncordon: &str, leases: &str) -> String {
    format!(
        "printf '%s\\n' \"$*\" >> '@dir@/kubectl.log'\n\
         case \" $* \" in\n\
         *\" config view \"*) {view}\n;;\n\
         *\" get leases \"*) {leases}\n;;\n\
         *\" get nodes \"*) {get_nodes}\n;;\n\
         *\" drain \"*) {drain}\n;;\n\
         *\" uncordon \"*) {uncordon}\n;;\n\
         *) echo 'ok (fake)';;\n\
         esac\n",
        view = cat(VIEW)
    )
}

/// A healthy cluster whose drains succeed (or exit `drain_exit`).
fn kubectl_body(nodes: &str, drain_exit: i32) -> String {
    kubectl_with(
        &cat(nodes),
        &format!("echo 'node drained (fake)'; exit {drain_exit}"),
        "echo 'node uncordoned (fake)'",
        &leases_alive(),
    )
}

/// A fake `kubectl` whose `get nodes` is healthy for its first `healthy_calls` calls and
/// `later` after that (a node list, or `ERROR` for a failing kubectl).
fn kubectl_degrading(healthy_calls: u32, later: &str) -> String {
    let later = if later == "ERROR" {
        "echo 'Unable to connect to the server' >&2; exit 1".to_owned()
    } else {
        cat(later)
    };
    let get_nodes = format!(
        "n=$(@cat@ '@dir@/count' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '@dir@/count'; \
         if [ $n -le {healthy_calls} ]; then {ready}; else {later}; fi",
        ready = cat(NODES_READY)
    );
    kubectl_with(
        &get_nodes,
        "echo 'node drained (fake)'",
        "echo 'node uncordoned (fake)'",
        &leases_alive(),
    )
}

/// A shell over the scripted fleet whose actions run the fake programs, with the tag `k8s-server`
/// rolling under `rule`, and cypramundi an ordinary machine.
fn world(t: &Tools, clan: &str, kubectl: &str, rule: RollRule, ready_timeout: u64) -> World {
    world_in(t, clan, kubectl, rule, ready_timeout, None)
}

/// [`world`], with the freeze file (and so the fleet's update lock) in `state`: two worlds with
/// the same `state` are two auspex processes on one fleet.
fn world_in(
    t: &Tools,
    clan: &str,
    kubectl: &str,
    rule: RollRule,
    ready_timeout: u64,
    state: Option<&Path>,
) -> World {
    world_polling(t, clan, kubectl, rule, ready_timeout, 5, state)
}

/// [`world_in`], with the wait between machines looking again every `poll` seconds.
fn world_polling(
    t: &Tools,
    clan: &str,
    kubectl: &str,
    rule: RollRule,
    ready_timeout: u64,
    poll: u64,
    state: Option<&Path>,
) -> World {
    let dir = tempdir();
    let fleet = tempdir();
    // `MISSING`: no `clan` at all, so nothing can be started.
    let clan = if clan == "MISSING" {
        dir.path().join("no-such-clan")
    } else {
        t.script(dir.path(), "clan", clan)
    };
    let nix = t.script(dir.path(), "nix", "exit 0");
    let kubectl = t.script(dir.path(), "kubectl", kubectl);
    let real = Real::for_actions(nix.into_os_string(), Some(clan))
        .with_kubectl(kubectl.into_os_string(), dir.path().join(KUBECONFIG));
    let backend = Backend::Demo(Demo {
        real_actions: Some(Box::new(real)),
        ..Demo::default()
    });
    let shell = Shell::new(backend, Log::memory(), Duration::from_secs(120));
    let db = dir.path().join("state").join("log.sqlite");
    shell.set_action_log(OpenParams::new(
        db.clone(),
        identity(),
        fleet.path().to_path_buf(),
    ));
    if let Some(state) = state {
        shell.set_freeze_store(FreezeStore::new(
            state.join("state.toml"),
            "/the/fleet".to_owned(),
        ));
    }
    let mut config = demo::config();
    fleet.path().to_str().unwrap().clone_into(&mut config.flake);
    config.public_machines.clear();
    config.rolling.rules = [("k8s-server".to_owned(), rule)].into();
    config.rolling.parallel = 2;
    config.rolling.ready_timeout_seconds = ready_timeout;
    config.rolling.poll_interval_seconds = poll;
    shell.send(Event::Start(config));
    World {
        dir,
        fleet,
        shell,
        db,
    }
}

const fn kube_rule(drain: bool) -> RollRule {
    RollRule {
        kubernetes: true,
        drain,
        drain_timeout_seconds: 60,
        etcd: false,
    }
}

async fn until(shell: &Arc<Shell>, what: &str, ready: impl Fn(&ViewModel) -> bool) {
    let wait = async {
        while !ready(&shell.view()) {
            tokio::select! {
                () = shell.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(60), wait)
        .await
        .is_err()
    {
        panic!("timed out waiting for {what}");
    }
}

/// The fleet has settled and the machines `U` is about are evaluated.
async fn settled(shell: &Arc<Shell>) {
    until(shell, "the fleet to settle", |v| {
        v.refresh.settled
            && ["luna", "sol", "cypramundi"].iter().all(|n| {
                v.report
                    .machines
                    .iter()
                    .any(|m| m.name == *n && m.pending_system.is_some())
            })
    })
    .await;
}

fn act_on(shell: &Arc<Shell>) {
    shell.send(Event::ToggleAct);
    shell.send(Event::ConfirmAnswer(true));
}

/// `U`, and the plan once it is asked.
async fn plan(shell: &Arc<Shell>) {
    shell.send(Event::UpdateAll);
    until(shell, "the plan to be asked about", |v| {
        v.confirm
            .as_ref()
            .is_some_and(|c| c.kind == ConfirmKind::UpdateAll)
    })
    .await;
}

/// `U`, and `y` to the plan once it is asked.
async fn roll(shell: &Arc<Shell>) {
    plan(shell).await;
    shell.send(Event::ConfirmAnswer(true));
}

async fn until_over(shell: &Arc<Shell>) {
    until(shell, "the roll to end", |v| {
        v.roll
            .as_ref()
            .is_some_and(|r| r.status == RollStatus::Ended)
    })
    .await;
}

fn state(shell: &Arc<Shell>, machine: &str) -> RollState {
    shell
        .view()
        .roll
        .unwrap()
        .machines
        .into_iter()
        .find(|m| m.name == machine)
        .unwrap()
        .state
}

/// The most updates running at once, from the fake `clan`'s record.
fn most_at_once(events: &[String], only: &[&str]) -> usize {
    let (mut now, mut most) = (0usize, 0usize);
    for e in events {
        let (what, machine) = e.split_once(' ').unwrap();
        if !only.contains(&machine) {
            continue;
        }
        match what {
            "start" => {
                now += 1;
                most = most.max(now);
            }
            "end" => now = now.saturating_sub(1),
            other => panic!("{other}"),
        }
    }
    most
}

async fn rows(db: &Path, fleet: &Path, machine: &str) -> Vec<auspex_effects::deploylog::Action> {
    let log = ActionLog::open(OpenParams::new(
        db.to_path_buf(),
        identity(),
        fleet.to_path_buf(),
    ))
    .await
    .expect("the log opens");
    let rows = log.recent(machine, 10).await.expect("the log reads");
    log.close().await;
    rows
}

/// The pids the fake `clan` has written, read without blocking the runtime that runs the roll.
async fn clan_pids(w: &World, at_least: usize) -> Vec<i32> {
    let mut found = Vec::new();
    for _ in 0..400 {
        found = lines(&w.path("clan.pids"))
            .iter()
            .filter_map(|l| l.trim().parse::<i32>().ok())
            .collect();
        if found.len() >= at_least {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{at_least} updates did not start: {found:?}");
}

async fn until_state(shell: &Arc<Shell>, machine: &str, want: RollState) {
    until(shell, &format!("{machine} to be {want:?}"), |v| {
        v.roll.as_ref().is_some_and(|r| {
            r.machines
                .iter()
                .any(|m| m.name == machine && m.state == want)
        })
    })
    .await;
}

/// The whole thing, for real processes: luna then sol (never together) beside cypramundi, the
/// output of each in the Log tab, one action-log row each, and the cluster asked which it is, for
/// its nodes before and after each machine, and nothing else (no drain was configured).
#[tokio::test]
async fn u_updates_the_group_one_at_a_time_beside_the_free_machine() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("0.8"),
        &kubectl_body(NODES_READY, 0),
        kube_rule(false),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;

    for m in ["luna", "sol", "cypramundi"] {
        assert_eq!(state(&w.shell, m), RollState::Done, "{m}");
    }
    let events = w.clan_events();
    assert_eq!(events.len(), 6, "{events:?}");
    assert_eq!(
        most_at_once(&events, &["luna", "sol"]),
        1,
        "two machines of one rolling tag updated at once: {events:?}"
    );
    let at = |e: &str| events.iter().position(|x| x == e).unwrap();
    assert!(at("end luna") < at("start sol"), "{events:?}");
    assert!(
        at("start cypramundi") < at("end sol"),
        "cypramundi is not part of the group: {events:?}"
    );
    // The cluster: which one (before the context is known, so unpinned), then the nodes for the
    // plan, and for each of luna and sol one read before, and for the wait after two: the first
    // records the node's heartbeat as the wait begins, the second finds a newer one (the fake
    // lease moves on at every read). Every call after the question is pinned to the context it
    // showed.
    let calls = w.kubectl_calls();
    assert_eq!(calls[0], w.kc("config view --minify -o json"), "{calls:#?}");
    let nodes = w.kcx("get nodes -o json");
    assert_eq!(
        calls[1..],
        [
            nodes.clone(),
            nodes.clone(),
            nodes.clone(),
            nodes.clone(),
            nodes.clone(),
            nodes.clone(),
            nodes.clone()
        ],
        "{calls:#?}"
    );
    // The Log tab: the plan first, a run for each machine with clan's own output.
    let all: Vec<String> = w
        .shell
        .output()
        .window(0, 300)
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(all[0], "── update all ──", "{all:?}");
    assert!(all.contains(&"update 3 machines:".to_owned()), "{all:?}");
    assert!(
        all.iter()
            .any(|l| l.starts_with("cluster: context rke2 (https://10.0.0.1:6443)")),
        "{all:?}"
    );
    for m in ["luna", "sol", "cypramundi"] {
        assert!(all.contains(&format!("── update {m} ──")), "{m}: {all:?}");
        assert!(all.contains(&format!("deploying {m}")), "{m}: {all:?}");
        assert!(all.contains(&format!("{m}: done")), "{m}: {all:?}");
    }
    // Each machine is one row of the action log.
    w.shell.shutdown().await;
    for m in ["luna", "sol", "cypramundi"] {
        let r = rows(&w.db, w.fleet.path(), m).await;
        assert_eq!(r.len(), 1, "{m}: {r:?}");
        assert_eq!(r[0].kind, LoggedKind::Deploy);
        assert_eq!(r[0].result, Some(LoggedResult::Succeeded), "{m}");
    }
}

/// With `drain = true`, in the order the brief says, each call pinned to the context: read the
/// cluster, drain, update, wait for Ready (a read that records the node's heartbeat, then one that
/// finds a newer), uncordon, and the next machine's read only after that.
/// The drain is in the machine's action-log row.
#[tokio::test]
async fn with_drain_kubectl_is_read_drain_wait_uncordon_for_each_machine_in_turn() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("0.2"),
        &kubectl_body(NODES_READY, 0),
        kube_rule(true),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    for m in ["luna", "sol"] {
        assert_eq!(state(&w.shell, m), RollState::Done, "{m}");
    }
    let calls = w.kubectl_calls();
    let drain = |m: &str| {
        w.kcx(&format!(
            "drain {m} --ignore-daemonsets --delete-emptydir-data --timeout 60s"
        ))
    };
    let nodes = w.kcx("get nodes -o json");
    assert_eq!(
        calls,
        [
            w.kc("config view --minify -o json"),
            nodes.clone(),
            nodes.clone(),
            drain("luna"),
            nodes.clone(),
            nodes.clone(),
            w.kcx("uncordon luna"),
            nodes.clone(),
            drain("sol"),
            nodes.clone(),
            nodes.clone(),
            w.kcx("uncordon sol"),
        ],
        "{calls:#?}"
    );
    w.shell.shutdown().await;
    // The row of a drained machine says so, and how it ended.
    let r = rows(&w.db, w.fleet.path(), "luna").await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].result, Some(LoggedResult::Succeeded));
    let detail = r[0].detail.clone().unwrap_or_default();
    assert!(
        detail.contains("drained first (kubectl drain)")
            && detail.contains("done [node drained first]"),
        "{detail}"
    );
}

/// A failed drain stops the group and the node is uncordoned again; the machine is never
/// updated, the free machine is, and the drained machine has a row saying what happened.
#[tokio::test]
async fn a_failed_drain_uncordons_and_never_runs_clan_for_that_machine() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_body(NODES_READY, 1),
        kube_rule(true),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    match state(&w.shell, "luna") {
        RollState::Failed(why) => assert!(
            why.starts_with("drain failed: kubectl drain exited 1"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&w.shell, "sol"), RollState::Skipped(_)));
    assert_eq!(state(&w.shell, "cypramundi"), RollState::Done);
    let calls = w.kubectl_calls();
    assert!(
        calls.iter().any(|c| c.ends_with("uncordon luna")),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .all(|c| !c.contains("drain sol") && !c.ends_with("uncordon sol")),
        "{calls:#?}"
    );
    assert_eq!(w.clan_events(), ["start cypramundi", "end cypramundi"]);
    w.shell.shutdown().await;
    let r = rows(&w.db, w.fleet.path(), "luna").await;
    assert_eq!(
        r.len(),
        1,
        "a drained machine has a row even if clan never ran: {r:?}"
    );
    assert_eq!(r[0].result, Some(LoggedResult::Failed));
    assert!(
        r[0].detail
            .clone()
            .unwrap_or_default()
            .contains("drain failed"),
        "{:?}",
        r[0].detail
    );
}

/// A cluster with a `NotReady` node is not rolled: `clan` is never run for its machines.
#[tokio::test]
async fn a_notready_node_means_clan_is_never_run_for_the_cluster() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_body(NODES_SOL_DOWN, 0),
        kube_rule(false),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    plan(&w.shell).await;
    let lines = w.shell.view().confirm.unwrap().lines;
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("skipped luna:") && l.contains("node sol is NotReady")),
        "{lines:?}"
    );
    w.shell.send(Event::ConfirmAnswer(true));
    until_over(&w.shell).await;
    assert_eq!(w.clan_events(), ["start cypramundi", "end cypramundi"]);
    w.shell.shutdown().await;
}

/// F2/F9. A quit while luna's update runs: `clan` is killed, nothing new starts, and because the
/// update was under way the node **stays cordoned** (the machine may be half updated), said in
/// the Log tab, the status bar, and, on the way out, by the exit report with the command.
#[tokio::test]
async fn quitting_mid_update_kills_clan_starts_nothing_and_names_the_cordon() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("30"),
        &kubectl_body(NODES_READY, 0),
        kube_rule(true),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    // luna (drained) and cypramundi are both updating.
    let running = clan_pids(&w, 2).await;
    until_state(&w.shell, "luna", RollState::Updating).await;

    w.shell.send(Event::Quit);
    assert_eq!(w.shell.view().confirm.unwrap().kind, ConfirmKind::Quit);
    w.shell.send(Event::ConfirmAnswer(true));
    until(&w.shell, "the roll to stop and quit", |v| v.quit).await;
    let left = still_alive_after_wait(&running);
    cleanup(&left);
    assert_eq!(left, Vec::<i32>::new(), "clan was left running");
    assert!(running.iter().all(|p| !alive(*p)));
    match state(&w.shell, "luna") {
        RollState::Failed(why) => assert!(
            why.starts_with("cancelled; node luna is left cordoned"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&w.shell, "sol"), RollState::Skipped(_)));
    assert_eq!(w.shell.view().roll.unwrap().left_cordoned, ["luna"]);
    let calls = w.kubectl_calls();
    assert!(
        calls.iter().any(|c| c.contains("drain luna ")),
        "{calls:#?}"
    );
    assert!(
        calls.iter().all(|c| !c.contains("uncordon")),
        "a node whose update was stopped is not put back: {calls:#?}"
    );
    assert_eq!(
        w.clan_events()
            .iter()
            .filter(|e| e.starts_with("start "))
            .count(),
        2
    );
    // On the way out it is said, with the command.
    w.shell.terminate().await;
    let report = w.shell.exit_report();
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(
        report[0].starts_with("node luna was left cordoned")
            && report[0].contains("uncordon luna")
            && report[0].contains("--context rke2"),
        "{report:?}"
    );
    // The row of the action log says it too.
    let r = rows(&w.db, w.fleet.path(), "luna").await;
    assert_eq!(r.len(), 1, "{r:?}");
    let detail = r[0].detail.clone().unwrap_or_default();
    let by_hand = format!(
        "kubectl --kubeconfig {} --context rke2 uncordon luna",
        auspex_domain::text::shell_quote(&w.path(KUBECONFIG).display().to_string())
    );
    assert!(detail.contains("is left cordoned"), "{detail:?}");
    assert!(detail.contains(&by_hand), "{detail:?}");
    // The shell's own report, written from its own record, quotes the same way.
    assert!(report[0].contains(&by_hand), "{report:?}");
}

/// F2. A termination signal while luna's update runs does what a `q` does: `clan` is stopped,
/// the node whose update was under way is named, and the roll's rows are closed.
#[tokio::test]
async fn a_signal_mid_update_stops_clan_and_names_the_cordon() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("30"),
        &kubectl_body(NODES_READY, 0),
        kube_rule(true),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    let running = clan_pids(&w, 2).await;
    until_state(&w.shell, "luna", RollState::Updating).await;
    // What `tui()` does after `term.recv()`.
    w.shell.terminate().await;
    let left = still_alive_after_wait(&running);
    cleanup(&left);
    assert_eq!(
        left,
        Vec::<i32>::new(),
        "clan was left running after a signal"
    );
    let report = w.shell.exit_report();
    assert!(
        report
            .iter()
            .any(|l| l.contains("node luna was left cordoned")),
        "{report:?}"
    );
    let r = rows(&w.db, w.fleet.path(), "luna").await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_ne!(r[0].result, None, "the row is closed, not running: {r:?}");
}

/// F2 (the reviewer's R1, where it matters). A signal while luna is **being drained**, before any
/// update: the node is given back before auspex leaves.
#[tokio::test]
async fn a_signal_during_a_drain_uncordons_the_node_before_exit() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo 'evicting (fake)'; @sleep@ 30",
        // Slow enough that the core's uncordon is still running when the shell could act.
        "@sleep@ 1; echo 'node uncordoned (fake)'",
        &leases_alive(),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(true), 20);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_state(&w.shell, "luna", RollState::Draining).await;
    w.shell.terminate().await;
    let calls = w.kubectl_calls();
    assert!(
        calls.iter().any(|c| c.ends_with("uncordon luna")),
        "luna was left cordoned after a signal: {calls:#?}"
    );
    // MC. Once: the core's own, given its grace to finish. A shell that went on to its own last
    // resort at once would run a second one beside it.
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.ends_with("uncordon luna"))
            .count(),
        1,
        "{calls:#?}"
    );
    assert_eq!(
        w.shell.exit_report(),
        Vec::<String>::new(),
        "nothing is left"
    );
    assert!(
        w.clan_events().iter().all(|e| !e.ends_with(" luna")),
        "luna was never updated: {:?}",
        w.clan_events()
    );
    // MC. The core was given its grace to stop the roll itself: it cancelled the drain, uncordoned
    // luna (that uncordon was not cut off by the shell's own cancelling) and ended the machine as
    // cancelled, closing its row so. A shell that cancelled everything at once would leave the core
    // with an uncordon that never ran, a node it thinks is still cordoned, and an interrupted row.
    assert_eq!(state(&w.shell, "luna"), RollState::Cancelled);
    assert_eq!(
        w.shell.view().roll.unwrap().left_cordoned,
        Vec::<String>::new()
    );
    let r = rows(&w.db, w.fleet.path(), "luna").await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].result, Some(LoggedResult::Cancelled), "{r:?}");
}

/// F2. A second `q` while the first is stopping the roll forces the exit and cancels the uncordon
/// the core had asked for: the shell uncordons the node itself, within a bound. The first
/// uncordon hangs (the fake says so once), the second works.
#[tokio::test]
async fn a_forced_quit_during_a_drain_still_uncordons_the_node() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo 'evicting (fake)'; @sleep@ 30",
        "if [ -e '@dir@/unc1' ]; then echo 'node uncordoned (fake)'; else touch '@dir@/unc1'; @sleep@ 30; fi",
        &leases_alive(),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(true), 20);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_state(&w.shell, "luna", RollState::Draining).await;
    w.shell.send(Event::Quit);
    w.shell.send(Event::ConfirmAnswer(true));
    // The drain is cancelled, the core asks for the uncordon, which hangs: wait until kubectl is
    // really running it, so the second quit cancels a process and not a request not yet spawned.
    until_state(&w.shell, "luna", RollState::Uncordoning).await;
    for _ in 0..200 {
        if w.kubectl_calls()
            .iter()
            .any(|c| c.ends_with("uncordon luna"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    w.shell.send(Event::Quit);
    until(&w.shell, "the forced quit", |v| v.quit).await;
    w.shell.terminate().await;
    let uncordons: Vec<String> = w
        .kubectl_calls()
        .into_iter()
        .filter(|c| c.ends_with("uncordon luna"))
        .collect();
    assert_eq!(
        uncordons.len(),
        2,
        "the core's, then the shell's: {uncordons:#?}"
    );
    assert_eq!(w.shell.exit_report(), Vec::<String>::new(), "luna is back");
}

/// F2. An uncordon that cannot be made at exit (kubectl refuses) is **said**, with the command,
/// on the way out: the node is not left cordoned silently.
#[tokio::test]
async fn an_uncordon_that_cannot_be_made_at_exit_is_said_with_the_command() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo 'evicting (fake)'; @sleep@ 30",
        "echo 'forbidden' >&2; exit 1",
        &leases_alive(),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(true), 20);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_state(&w.shell, "luna", RollState::Draining).await;
    w.shell.terminate().await;
    let report = w.shell.exit_report();
    assert!(
        report
            .iter()
            .any(|l| l.starts_with("node luna is STILL CORDONED")
                && l.contains("uncordon luna")
                && l.contains("--context rke2")),
        "{report:?}"
    );
    assert!(
        w.shell
            .log
            .lines()
            .iter()
            .any(|l| l.contains("STILL CORDONED")),
        "and in the log"
    );
}

/// MD, ME, MF and N7 (the hanging case). A signal while luna is being drained, with a `kubectl
/// uncordon` that never answers (the core's, and the shell's own at exit). A second signal (here: a
/// second after the first) must cut the core's grace short, the exit must kill the hung uncordon at
/// its bound, say which node is still cordoned, and the row of the machine whose run never ended must
/// be closed saying so. Every bound is real time, so the test runs against a clock: it fails if the
/// exit takes more than a few seconds (the grace is 20 s, the uncordon's `sleep` is 30 s).
#[tokio::test]
async fn a_second_signal_cuts_the_grace_short_and_a_hung_uncordon_is_killed_and_named() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo 'evicting (fake)'; @sleep@ 30",
        // `exec`: killing the script kills the `sleep`, so nothing outlives the test.
        "exec @sleep@ 30",
        &leases_alive(),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(true), 20);
    w.shell.set_last_resort_bound(Duration::from_secs(1));
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_state(&w.shell, "luna", RollState::Draining).await;
    let began = std::time::Instant::now();
    let second_signal = tokio::time::sleep(Duration::from_secs(1));
    if tokio::time::timeout(
        Duration::from_secs(12),
        w.shell.terminate_with(second_signal),
    )
    .await
    .is_err()
    {
        panic!("leaving took more than 12 s: the second signal or the bound did nothing");
    }
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    let report = w.shell.exit_report();
    assert!(
        report
            .iter()
            .any(|l| l.starts_with("node luna is STILL CORDONED")
                && l.contains("took more than 1s")
                && l.contains("uncordon luna")),
        "{report:?}"
    );
    // MF, and N7 from the other side: the row was held open (the core never ended luna), is closed
    // as interrupted, and names the node, because it really is left cordoned.
    let r = rows(&w.db, w.fleet.path(), "luna").await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].result, Some(LoggedResult::Interrupted), "{r:?}");
    let detail = r[0].detail.clone().unwrap_or_default();
    assert!(
        detail.contains("auspex left before this was over"),
        "{detail:?}"
    );
    assert!(
        detail.contains("nodes still cordoned by auspex: luna"),
        "{detail:?}"
    );
}

/// N7. The same signal, but the shell's own uncordon at exit works (the core's hangs, the fake
/// says so once). The node is back, so the row of the machine, closed after it, must not say it
/// is still cordoned. Closed first, it did.
#[tokio::test]
async fn a_row_closed_at_exit_names_only_the_nodes_really_left_cordoned() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo 'evicting (fake)'; @sleep@ 30",
        "if [ -e '@dir@/unc1' ]; then echo 'node uncordoned (fake)'; else touch '@dir@/unc1'; exec @sleep@ 30; fi",
        &leases_alive(),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(true), 20);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_state(&w.shell, "luna", RollState::Draining).await;
    let second_signal = tokio::time::sleep(Duration::from_secs(1));
    w.shell.terminate_with(second_signal).await;
    assert_eq!(w.shell.exit_report(), Vec::<String>::new(), "luna is back");
    let uncordons = w
        .kubectl_calls()
        .into_iter()
        .filter(|c| c.ends_with("uncordon luna"))
        .count();
    assert_eq!(uncordons, 2, "the core's (killed), then the shell's");
    let r = rows(&w.db, w.fleet.path(), "luna").await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].result, Some(LoggedResult::Interrupted), "{r:?}");
    let detail = r[0].detail.clone().unwrap_or_default();
    assert!(
        detail.contains("auspex left before this was over"),
        "{detail:?}"
    );
    assert!(
        !detail.contains("still cordoned"),
        "the node was given back first: {detail:?}"
    );
}

/// F3. The reviewer's R2: mars (an etcd member that is not in the roll) goes `NotReady` and sol is
/// cordoned by someone while luna updates. sol is neither drained nor updated, and sol's cordon is
/// not auspex's to lift.
#[tokio::test]
async fn a_cluster_that_got_sick_during_the_roll_stops_the_group_before_sol() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    // The plan's look and luna's read are healthy; the wait for luna and everything after
    // are not (luna itself is fine).
    let w = world(
        &t,
        &clan_body("0.2"),
        &kubectl_degrading(2, NODES_SICK),
        kube_rule(true),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    assert_eq!(state(&w.shell, "luna"), RollState::Done);
    match state(&w.shell, "sol") {
        RollState::Failed(why) => assert!(
            why.starts_with(
                "the cluster is not healthy: node mars is NotReady; node sol is cordoned"
            ),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    let events = w.clan_events();
    assert!(!events.contains(&"start sol".to_owned()), "{events:?}");
    let calls = w.kubectl_calls();
    assert!(
        calls
            .iter()
            .all(|c| !c.contains("drain sol") && !c.contains("uncordon sol")),
        "{calls:#?}"
    );
    w.shell.shutdown().await;
}

/// F4. The reviewer's R3: two auspex processes on one fleet. The second `U`, pressed while the
/// first is on sol, is refused (and says who holds the lock): two members of one group are never
/// out at once. When the first is over the second can run.
#[tokio::test]
async fn a_second_auspex_cannot_roll_the_fleet_while_the_first_does() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let shared = tempdir();
    let clan_log = shared.path().join("clan.log");
    let state_root = tempdir();
    // A directory that does not exist yet: auspex creates its state directory private (0700).
    let state = state_root.path().join("auspex");
    let clan = clan_body_to(&clan_log.display().to_string(), "1");
    let a = world_in(
        &t,
        &clan,
        &kubectl_body(NODES_READY, 0),
        kube_rule(false),
        20,
        Some(&state),
    );
    let b = world_in(
        &t,
        &clan,
        &kubectl_body(NODES_READY, 0),
        kube_rule(false),
        20,
        Some(&state),
    );
    settled(&a.shell).await;
    settled(&b.shell).await;
    act_on(&a.shell);
    act_on(&b.shell);
    roll(&a.shell).await;
    // MG. A lone `u` takes the same lock: while A rolls, B's `u` of a machine is refused too (the
    // shell's lock is not only for "update all"). A took the lock at its `y`, before anything
    // started, so this needs no race with A's machines.
    b.shell.send(Event::Search("cypramundi".to_owned()));
    b.shell.send(Event::UpdateSelected);
    until(&b.shell, "B's `u` to be asked about", |v| {
        v.confirm
            .as_ref()
            .is_some_and(|c| c.kind == ConfirmKind::Update)
    })
    .await;
    b.shell.send(Event::ConfirmAnswer(true));
    until(&b.shell, "B's refusal of the `u`", |v| {
        v.notice.as_deref().is_some_and(|n| {
            n.starts_with("not updating: another auspex is updating this fleet")
                && n.contains("update all")
        })
    })
    .await;
    // (cypramundi is one of A's machines too: it is updated once, by A.)
    let starts = lines(&clan_log)
        .iter()
        .filter(|l| *l == "start cypramundi")
        .count();
    assert!(starts <= 1, "B's `u` ran an update: {:?}", lines(&clan_log));
    // A is on sol when B asks.
    let log = clan_log.clone();
    let wait = async {
        while !lines(&log).contains(&"start sol".to_owned()) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    if tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .is_err()
    {
        panic!(
            "A never reached sol: {:?} {:?} clan {:?} log {:?}",
            a.shell.view().roll,
            a.shell.view().notice,
            lines(&clan_log),
            a.shell.log.lines()
        );
    }
    roll(&b.shell).await;
    until(&b.shell, "B's refusal", |v| {
        v.notice
            .as_deref()
            .is_some_and(|n| n.starts_with("not updating: another auspex is updating this fleet"))
    })
    .await;
    assert!(b.shell.view().roll.is_none(), "B never started");
    let notice = b.shell.view().notice.unwrap();
    assert!(
        notice.contains("update all"),
        "it names what A is doing: {notice}"
    );
    until_over(&a.shell).await;
    let events = lines(&clan_log);
    assert_eq!(
        most_at_once(&events, &["luna", "sol"]),
        1,
        "two members of one rolling group at once: {events:?}"
    );
    // A is over: its lock is let go, and B can take it.
    roll(&b.shell).await;
    until_over(&b.shell).await;
    a.shell.shutdown().await;
    b.shell.shutdown().await;
}

/// F7. A node that says `Ready` with a heartbeat from before the update (and no lease) is not
/// back: the wait goes on until its deadline and fails the machine, saying why.
#[tokio::test]
async fn a_ready_from_before_the_update_does_not_end_the_wait() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_STALE),
        "echo drained",
        "echo uncordoned",
        "echo '{\"items\":[]}'",
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(false), 1);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    match state(&w.shell, "luna") {
        RollState::Failed(why) => assert!(
            why.contains(
                "node luna is Ready, but its kubelet has not been heard from since the wait began"
            ),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&w.shell, "sol"), RollState::Skipped(_)));
    // N2: with no lease at all, silence is not judged (luna's condition heartbeat is nine minutes
    // older than sol's, and luna was let through to its update), and the machine's Log line says so.
    assert!(
        w.clan_events().contains(&"start luna".to_owned()),
        "{:?}",
        w.clan_events()
    );
    let out = w.shell.output();
    let log: Vec<String> = out
        .window(0, out.rows())
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        log.iter()
            .any(|l| l.contains("luna: the node leases could not be read")),
        "{log:#?}"
    );
    w.shell.shutdown().await;
}

/// M08/M02. A kubectl that fails during the wait, or a node that is no longer in the list, is not
/// "healthy": the machine fails.
#[tokio::test]
async fn a_kubectl_that_fails_or_a_node_that_vanishes_during_the_wait_fails_the_machine() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let gone = r#"{"items":[{"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#;
    for (later, expect) in [
        (
            "ERROR",
            "cannot read the cluster: kubectl get nodes exited 1",
        ),
        (gone, "node luna is not in the cluster"),
    ] {
        let w = world(
            &t,
            &clan_body("0.1"),
            &kubectl_degrading(2, later),
            kube_rule(false),
            1,
        );
        settled(&w.shell).await;
        act_on(&w.shell);
        roll(&w.shell).await;
        until_over(&w.shell).await;
        match state(&w.shell, "luna") {
            RollState::Failed(why) => assert!(why.contains(expect), "{later}: {why}"),
            other => panic!("{other:?}"),
        }
        w.shell.shutdown().await;
    }
}

/// The wait between machines is for the node: one that never becomes `Ready` after its machine
/// was updated fails that machine (with the node named) and stops the group. The fake `kubectl`
/// answers the plan's look and the read before luna with a healthy cluster and every later look
/// with `luna` `NotReady`.
#[tokio::test]
async fn a_node_that_never_becomes_ready_fails_the_machine_and_stops_the_group() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let down = r#"{"items":[{"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"False"}]}},{"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#;
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_degrading(2, down),
        kube_rule(false),
        1,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    match state(&w.shell, "luna") {
        RollState::Failed(why) => assert_eq!(
            why, "not healthy after the update: node luna is NotReady",
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&w.shell, "sol"), RollState::Skipped(_)));
    let started = w
        .clan_events()
        .iter()
        .filter(|e| e.starts_with("start "))
        .count();
    assert_eq!(
        started,
        2,
        "luna and cypramundi, not sol: {:?}",
        w.clan_events()
    );
    w.shell.shutdown().await;
}

/// Round 2. A `clan` that cannot be found or started has done nothing to the machine: the update
/// is a refusal, and a node drained for it goes **back** (it is not left cordoned as "may be
/// broken"). The group goes on, as for any refusal.
#[tokio::test]
async fn a_clan_that_cannot_be_started_refuses_the_machine_and_gives_the_node_back() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        "MISSING",
        &kubectl_body(NODES_READY, 0),
        kube_rule(true),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    for m in ["luna", "sol"] {
        match state(&w.shell, m) {
            RollState::Refused(why) => assert!(why.contains("clan"), "{m}: {why}"),
            other => panic!("{m}: {other:?}"),
        }
    }
    let calls = w.kubectl_calls();
    for m in ["luna", "sol"] {
        assert!(
            calls.iter().any(|c| c.ends_with(&format!("uncordon {m}"))),
            "{m} was put back: {calls:#?}"
        );
    }
    assert_eq!(w.shell.view().roll.unwrap().left_cordoned.len(), 0);
    w.shell.terminate().await;
    assert_eq!(
        w.shell.exit_report(),
        Vec::<String>::new(),
        "nothing is left"
    );
}

/// Round 3. A node is auspex's to leave cordoned only from the moment `clan` is spawned. Before
/// that (here: no `clan` could be found) the update has done nothing to the machine, so the node
/// stays in the record of nodes to give back, and is **not** reported as "left cordoned, the
/// machine may be broken".
#[tokio::test]
async fn a_node_is_committed_when_clan_is_spawned_and_not_before() {
    use auspex_core::effects::{ActionKind, ActionRequest, RequestId};
    use auspex_domain::fleet::MachineClass;
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let run = |clan: String| {
        let t = &t;
        async move {
            let w = world(t, &clan, &kubectl_body(NODES_READY, 0), kube_rule(true), 20);
            settled(&w.shell).await;
            w.shell.cordons().register(
                "luna",
                "/nonexistent/kubectl".into(),
                "/k/config".into(),
                Some("rke2".to_owned()),
            );
            let (tx, rx) = tokio::sync::oneshot::channel();
            w.shell.start_action(
                ActionRequest {
                    id: RequestId(900),
                    kind: ActionKind::Update,
                    machine: "luna".to_owned(),
                    class: MachineClass::Nixos,
                    system: None,
                    flake: w.fleet.path().to_str().unwrap().to_owned(),
                    build_host: None,
                    target: None,
                    log: None,
                },
                move |_, end| {
                    let _ = tx.send(end);
                },
            );
            let end = tokio::time::timeout(Duration::from_secs(20), rx)
                .await
                .expect("the action ends")
                .unwrap();
            let report = w.shell.cordons().last_resort(Duration::from_secs(1));
            w.shell.shutdown().await;
            (end, report)
        }
    };
    let (end, report) = run("MISSING".to_owned()).await;
    assert_eq!(end.result, auspex_core::ActionResult::Refused, "{end:?}");
    assert!(
        report.len() == 1 && report[0].starts_with("node luna is STILL CORDONED"),
        "still to be given back, not left on purpose: {report:?}"
    );
    let (_, report) = run(clan_body("0")).await;
    assert!(
        report.len() == 1 && report[0].starts_with("node luna was left cordoned"),
        "clan was spawned: left on purpose: {report:?}"
    );
}

// ---------------------------------------------------------------------------------------
// etcd
// ---------------------------------------------------------------------------------------

const fn etcd_rule() -> RollRule {
    RollRule {
        kubernetes: true,
        etcd: true,
        drain: false,
        drain_timeout_seconds: 60,
    }
}

/// luna and sol, both etcd members, Ready.
const NODES_ETCD: &str = r#"{"items":[
  {"metadata":{"name":"luna","labels":{"node-role.kubernetes.io/etcd":"true"}},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"sol","labels":{"node-role.kubernetes.io/etcd":"true"}},"status":{"conditions":[{"type":"Ready","status":"True"}]}}
]}"#;

fn etcd_pods_json(luna: bool, sol: bool) -> String {
    let pod = |n: &str, r: bool| {
        format!(
            r#"{{"spec":{{"nodeName":"{n}"}},"status":{{"phase":"Running","conditions":[{{"type":"Ready","status":"{}"}}]}}}}"#,
            if r { "True" } else { "False" }
        )
    };
    format!(r#"{{"items":[{},{}]}}"#, pod("luna", luna), pod("sol", sol))
}

/// A fake `kubectl` over [`NODES_ETCD`] whose `get pods` answers `pods` for its first
/// `healthy_calls` calls and `later` after that (`ERROR`: a failing kubectl).
fn kubectl_etcd(healthy_calls: u32, pods: &str, later: &str) -> String {
    let later = if later == "ERROR" {
        "echo 'Unable to connect to the server' >&2; exit 1".to_owned()
    } else {
        cat(later)
    };
    let get_pods = format!(
        "n=$(@cat@ '@dir@/pcount' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '@dir@/pcount'; \
         if [ $n -le {healthy_calls} ]; then {ok}; else {later}; fi",
        ok = cat(pods)
    );
    format!(
        "printf '%s\\n' \"$*\" >> '@dir@/kubectl.log'\n\
         case \" $* \" in\n\
         *\" config view \"*) {view}\n;;\n\
         *\" get leases \"*) {leases}\n;;\n\
         *\" get pods \"*) {get_pods}\n;;\n\
         *\" get nodes \"*) {nodes}\n;;\n\
         *) echo 'ok (fake)';;\n\
         esac\n",
        view = cat(VIEW),
        leases = leases_alive(),
        nodes = cat(NODES_ETCD),
    )
}

const ETCD_POD_CALL: &str = "get pods -n kube-system -l component=etcd -o json";

fn started(w: &World) -> Vec<String> {
    w.clan_events()
        .into_iter()
        .filter(|e| e.starts_with("start "))
        .collect()
}

/// All members ready all along: the group rolls luna then sol, and etcd is read in the plan, before
/// each machine and in each wait.
#[tokio::test]
async fn with_etcd_healthy_the_group_rolls_and_etcd_is_read_at_every_step() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let ok = etcd_pods_json(true, true);
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_etcd(1000, &ok, &ok),
        etcd_rule(),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    for m in ["luna", "sol"] {
        assert_eq!(state(&w.shell, m), RollState::Done, "{m}");
    }
    let pods = w
        .kubectl_calls()
        .iter()
        .filter(|c| c.contains(ETCD_POD_CALL))
        .count();
    assert!(
        pods >= 5,
        "plan, luna's read and wait, sol's read and wait: {pods}"
    );
    assert!(
        w.kubectl_calls()
            .iter()
            .all(|c| c.contains("--context=rke2") || c.contains("config view")),
        "{:#?}",
        w.kubectl_calls()
    );
    w.shell.shutdown().await;
}

/// Sol's etcd goes down while luna is being updated: luna is done, and sol is not taken out.
#[tokio::test]
async fn a_member_whose_etcd_went_down_during_the_roll_stops_the_group_before_it_is_touched() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let ok = etcd_pods_json(true, true);
    let sol_down = etcd_pods_json(true, false);
    // The plan's look and luna's read are healthy; luna's wait and sol's read see sol down. (luna
    // itself is fine, so luna's own wait passes.)
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_etcd(2, &ok, &sol_down),
        etcd_rule(),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    assert_eq!(state(&w.shell, "luna"), RollState::Done);
    match state(&w.shell, "sol") {
        RollState::Failed(why) => {
            assert_eq!(why, "etcd is not healthy: etcd on sol is not ready");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        !started(&w).contains(&"start sol".to_owned()),
        "{:?}",
        w.clan_events()
    );
    w.shell.shutdown().await;
}

/// The machine just updated is not done until its own etcd pod is Ready again.
#[tokio::test]
async fn a_machine_whose_etcd_does_not_come_back_fails_its_wait_and_stops_the_group() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let ok = etcd_pods_json(true, true);
    let luna_down = etcd_pods_json(false, true);
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_etcd(2, &ok, &luna_down),
        etcd_rule(),
        1,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    match state(&w.shell, "luna") {
        RollState::Failed(why) => {
            assert_eq!(
                why,
                "not healthy after the update: etcd on luna is not ready"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&w.shell, "sol"), RollState::Skipped(_)));
    assert!(!started(&w).contains(&"start sol".to_owned()));
    w.shell.shutdown().await;
}

/// A kubectl that cannot list the etcd pods is not permission: the group is refused in the plan.
#[tokio::test]
async fn an_etcd_that_cannot_be_read_refuses_the_group_and_clan_never_runs_for_it() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_etcd(0, "{}", "ERROR"),
        etcd_rule(),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    plan(&w.shell).await;
    let lines = w.shell.view().confirm.unwrap().lines;
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("skipped luna:") && l.contains("cannot read etcd")),
        "{lines:?}"
    );
    w.shell.send(Event::ConfirmAnswer(true));
    until_over(&w.shell).await;
    assert_eq!(w.clan_events(), ["start cypramundi", "end cypramundi"]);
    w.shell.shutdown().await;
}

/// Without `etcd = true` the same unhealthy etcd is not looked at: the roll is as before.
#[tokio::test]
async fn without_the_etcd_key_etcd_is_never_asked_about() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let down = etcd_pods_json(false, false);
    let w = world(
        &t,
        &clan_body("0.1"),
        &kubectl_etcd(1000, &down, &down),
        kube_rule(false),
        20,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    for m in ["luna", "sol"] {
        assert_eq!(state(&w.shell, m), RollState::Done, "{m}");
    }
    assert!(
        w.kubectl_calls().iter().all(|c| !c.contains(ETCD_POD_CALL)),
        "{:#?}",
        w.kubectl_calls()
    );
    w.shell.shutdown().await;
}

/// A fake `kubectl` whose node `luna` is `NotReady` for exactly one look after the three that
/// precede the wait's first (the plan's, the one before luna's turn and the wait's record), and `Ready`
/// ever after.
fn kubectl_recovering() -> String {
    let down = r#"{"items":[{"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"False"}]}},{"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#;
    let get_nodes = format!(
        "n=$(@cat@ '@dir@/count' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '@dir@/count'; @date@ +%s >> '@dir@/times'; \
         if [ $n -eq 4 ]; then {down}; else {ready}; fi",
        down = cat(down),
        ready = cat(NODES_READY)
    );
    kubectl_with(
        &get_nodes,
        "echo 'node drained (fake)'",
        "echo 'node uncordoned (fake)'",
        &leases_alive(),
    )
}

/// `update.poll_interval` is the wait's pace: a node that is back one look later is noticed after
/// that interval, so with 1 s the wait ends well before the 5 s default could have looked again.
#[tokio::test]
async fn a_short_poll_interval_ends_the_wait_sooner_than_the_default() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world_polling(
        &t,
        &clan_body("0.1"),
        &kubectl_recovering(),
        kube_rule(false),
        60,
        1,
        None,
    );
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_state(&w.shell, "luna", RollState::Done).await;
    // The looks are the plan's, the one before luna's turn, the wait's record, the NotReady one and the
    // Ready one:
    // the last two are one interval apart (the clock has whole seconds, so 1 reads as 1 or 2),
    // where the default would put them 5 apart.
    let times: Vec<u64> = lines(&w.path("times"))
        .iter()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert!(times.len() >= 2, "{times:?}");
    let gap = times[times.len() - 1] - times[times.len() - 2];
    assert!(
        gap <= 2,
        "a 1 s interval looked again after {gap} s: {times:?}"
    );
    w.shell.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// the kubelets' heartbeats (N2, N4)
// ---------------------------------------------------------------------------------------

/// N2 (the reviewer's RR2), at the plan. mars's kubelet is 30 s behind every other (three missed
/// renewals): it is down, and the node controller has not yet turned its `Ready` into `Unknown` (its
/// grace is about 40 s), which is why the bound is below it. The
/// plan refuses the cluster's group, saying which node and for how long, and `clan` is not run for
/// it. The time is measured between the kubelets (the newest heartbeat of the same read), never
/// against this machine's clock: the leases here are dated 2030.
#[tokio::test]
async fn a_node_whose_kubelet_went_silent_refuses_the_group_at_the_plan() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo drained",
        "echo uncordoned",
        &leases(&["luna", "mars", "sol", "terra"], &[("mars", 30)], 0),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(true), 20);
    settled(&w.shell).await;
    act_on(&w.shell);
    plan(&w.shell).await;
    let lines = w.shell.view().confirm.unwrap().lines;
    assert!(
        lines.iter().any(|l| l.starts_with("skipped luna:")
            && l.contains("node mars is Ready, but its kubelet has been silent for 30 s")),
        "{lines:?}"
    );
    w.shell.send(Event::ConfirmAnswer(true));
    until_over(&w.shell).await;
    assert_eq!(w.clan_events(), ["start cypramundi", "end cypramundi"]);
    assert!(
        w.kubectl_calls()
            .iter()
            .all(|c| !c.contains("drain") && !c.contains("uncordon")),
        "{:#?}",
        w.kubectl_calls()
    );
    w.shell.shutdown().await;
}

/// N2, at the read before a machine: the plan's look and the read at `y` are healthy, then mars's
/// kubelet stops. The next read stops the group before luna is drained, and the Log says why. (With
/// `drain = true`, the case the reviewer reproduced, and the default `drain = false` has the same
/// read.)
#[tokio::test]
async fn a_kubelet_that_went_silent_after_the_plan_stops_the_group_before_the_next_machine() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo drained",
        "echo uncordoned",
        &leases(&["luna", "mars", "sol", "terra"], &[("mars", 30)], 1),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(true), 20);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    match state(&w.shell, "luna") {
        RollState::Failed(why) => assert!(
            why.contains("node mars is Ready, but its kubelet has been silent for 30 s"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&w.shell, "sol"), RollState::Skipped(_)));
    let events = w.clan_events();
    assert!(!events.contains(&"start luna".to_owned()), "{events:?}");
    assert!(
        w.kubectl_calls()
            .iter()
            .all(|c| !c.contains("drain luna") && !c.contains("uncordon luna")),
        "nothing was drained: {:#?}",
        w.kubectl_calls()
    );
    w.shell.shutdown().await;
}

/// The reviewer's RR3 lease list (frozen, and ahead of this machine's clock): every lease says one fixed time, in
/// 2030, and no kubelet renews after the plan.
fn leases_frozen() -> String {
    let item = |n: &str| {
        format!(r#"{{"metadata":{{"name":"{n}"}},"spec":{{"renewTime":"2030-01-01T00:01:00Z"}}}}"#)
    };
    format!(
        "printf '%s\\n' '{{\"items\":[{}]}}'",
        ["luna", "mars", "sol", "terra"].map(item).join(",")
    )
}

/// N4 (the reviewer's RR3). The wait must not compare the kubelets' clock with this machine's:
/// nodes a few years ahead whose kubelets never renew after the update would count as "heard from
/// since" at once. A node is back only for a heartbeat newer than the one it had when the wait
/// began, so these fail at the deadline.
#[tokio::test]
async fn a_node_clock_ahead_of_auspex_does_not_pass_the_wait_on_a_heartbeat_from_before() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo drained",
        "echo uncordoned",
        &leases_frozen(),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(false), 3);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    until_over(&w.shell).await;
    let luna = state(&w.shell, "luna");
    w.shell.shutdown().await;
    match luna {
        RollState::Failed(why) => assert!(
            why.contains("its kubelet has not been heard from since the wait began"),
            "{why}"
        ),
        other => panic!("no kubelet renewed after the update, yet: {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// the record of a wait taken from a partial read (R1, R7 of the review of PR #38)
// ---------------------------------------------------------------------------------------

/// The lease list of [`leases_frozen`], except that read number `fails_on` (counting every lease
/// read, the plan's first) exits 1: `kubectl get nodes` answers and `get leases` does not.
fn leases_frozen_failing_on(fails_on: u32) -> String {
    let item = |n: &str| {
        format!(r#"{{"metadata":{{"name":"{n}"}},"spec":{{"renewTime":"2030-01-01T00:01:00Z"}}}}"#)
    };
    format!(
        "f='@dir@/lease.reads'; r=$(@cat@ \"$f\" 2>/dev/null || echo 0); r=$((r+1)); echo $r > \"$f\"; \
         if [ \"$r\" -eq {fails_on} ]; then echo 'leases are down' >&2; exit 1; fi; \
         printf '%s\\n' '{{\"items\":[{}]}}'",
        ["luna", "mars", "sol", "terra"].map(item).join(",")
    )
}

/// Every node `Ready` with a condition heartbeat from before the leases' fixed time, which no
/// kubelet renews after the plan.
const NODES_READY_WITH_HEARTBEAT: &str = r#"{"items":[
  {"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True","lastHeartbeatTime":"2030-01-01T00:00:00Z"}]}},
  {"metadata":{"name":"mars"},"status":{"conditions":[{"type":"Ready","status":"True","lastHeartbeatTime":"2030-01-01T00:00:00Z"}]}},
  {"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True","lastHeartbeatTime":"2030-01-01T00:00:00Z"}]}},
  {"metadata":{"name":"terra"},"status":{"conditions":[{"type":"Ready","status":"True","lastHeartbeatTime":"2030-01-01T00:00:00Z"}]}}
]}"#;

/// luna's wait ends at its deadline, as sol's would: no kubelet renewed anything since the update.
async fn assert_luna_fails_at_the_deadline(w: &World) {
    until_over(&w.shell).await;
    let luna = state(&w.shell, "luna");
    let events = w.clan_events();
    assert!(
        matches!(luna, RollState::Failed(ref why) if why.contains("has not been heard from since the wait began")),
        "no kubelet renewed after the update, yet luna is {luna:?}; clan {events:?}"
    );
    assert!(
        !events.contains(&"start sol".to_owned()),
        "the group went on to sol: {events:?}"
    );
}

/// R1 (the review's zz1). `get leases` fails at the moment the wait takes its record, so the record
/// holds only the `Ready` condition's heartbeat, which a kubelet rewrites every few minutes. The next
/// read has the leases: a lease renewed before the switch is newer than that condition, but it is a
/// different kind of heartbeat, so it proves nothing. The record and the proof must be of one kind.
#[tokio::test]
async fn a_lease_read_that_failed_at_the_record_does_not_let_an_old_lease_pass_the_wait() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    // Lease reads: the plan's (1), luna's read before it (2), the wait's record (3).
    let kubectl = kubectl_with(
        &cat(NODES_READY_WITH_HEARTBEAT),
        "echo drained",
        "echo uncordoned",
        &leases_frozen_failing_on(3),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(false), 3);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    assert_luna_fails_at_the_deadline(&w).await;
    w.shell.shutdown().await;
}

/// R1 (the review's zz2): the same, with a node list that has no condition heartbeat, so the record
/// is empty and "any heartbeat counts" would let the first lease through.
#[tokio::test]
async fn a_lease_read_that_failed_at_the_record_with_no_condition_heartbeat_does_not_pass_the_wait()
{
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let kubectl = kubectl_with(
        &cat(NODES_READY),
        "echo drained",
        "echo uncordoned",
        &leases_frozen_failing_on(3),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(false), 3);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    assert_luna_fails_at_the_deadline(&w).await;
    w.shell.shutdown().await;
}

/// R7 (mutation B1). `get nodes` itself fails at the record. The first read that has the node takes
/// the record instead, and a heartbeat after that is the proof; with no fallback the wait would
/// accept the first `Ready` it sees.
#[tokio::test]
async fn a_node_read_that_failed_at_the_record_is_recorded_by_the_next_one() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    // Node reads: the plan's (1), luna's read before it (2), the wait's record (3).
    let get_nodes = format!(
        "f='@dir@/nodes.reads'; r=$(@cat@ \"$f\" 2>/dev/null || echo 0); r=$((r+1)); echo $r > \"$f\"; \
         if [ \"$r\" -eq 3 ]; then echo 'Unable to connect to the server' >&2; exit 1; fi; {}",
        cat(NODES_READY)
    );
    let kubectl = kubectl_with(
        &get_nodes,
        "echo drained",
        "echo uncordoned",
        &leases_frozen(),
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(false), 3);
    settled(&w.shell).await;
    act_on(&w.shell);
    roll(&w.shell).await;
    assert_luna_fails_at_the_deadline(&w).await;
    w.shell.shutdown().await;
}
