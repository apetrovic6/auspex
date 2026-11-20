//! `U` for real: the core, the shell and the adapters over the demo's scripted fleet, with a fake
//! `clan` and a fake `kubectl` standing in for the programs. What is run and in what order, that
//! two machines of one rolling tag are never updating at the same moment (the fake `clan`
//! records when each update starts and ends), the node waits and drains, and what a quit does to
//! the processes and to a cordoned node.
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
use auspex_effects::deploylog::{ActionKind as LoggedKind, ActionLog, Identity, OpenParams};
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

impl World {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The lines `kubectl` was called with, one call per line.
    fn kubectl_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("kubectl.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `start <machine>` and `end <machine>` lines, in the order the fake `clan` wrote them.
    fn clan_events(&self) -> Vec<String> {
        fs::read_to_string(self.path("clan.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
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

/// The fake `clan`: records `start <machine>` and `end <machine>` around `sleep_seconds`, and
/// its own pid, for the tests that kill it.
fn clan_body(sleep_seconds: &str) -> String {
    format!(
        "for a in \"$@\"; do m=$a; done\n\
         printf '%s\\n' $$ >> '@dir@/clan.pids'\n\
         printf 'start %s\\n' \"$m\" >> '@dir@/clan.log'\n\
         echo \"deploying $m\"\n\
         @sleep@ {sleep_seconds}\n\
         printf 'end %s\\n' \"$m\" >> '@dir@/clan.log'\n\
         echo \"done $m\"\n"
    )
}

/// The fake `kubectl`: records its arguments, and answers `get nodes` with `nodes`.
fn kubectl_body(nodes: &str, drain_exit: i32) -> String {
    format!(
        "printf '%s\\n' \"$*\" >> '@dir@/kubectl.log'\n\
         case \" $* \" in\n\
         *\" get nodes \"*) cat <<'EOF'\n{nodes}\nEOF\n;;\n\
         *\" drain \"*) echo 'node drained (fake)'; exit {drain_exit};;\n\
         *) echo 'ok (fake)';;\n\
         esac\n"
    )
}

/// A shell over the scripted fleet whose actions run the fake programs, with the tag `k8s-server`
/// rolling under `rule`, and cypramundi an ordinary machine.
fn world(t: &Tools, clan: &str, kubectl: &str, rule: RollRule, ready_timeout: u64) -> World {
    let dir = tempdir();
    let fleet = tempdir();
    let clan = t.script(dir.path(), "clan", clan);
    let nix = t.script(dir.path(), "nix", "exit 0");
    let kubectl = t.script(dir.path(), "kubectl", kubectl);
    let real = Real::for_actions(nix.into_os_string(), Some(clan))
        .with_kubectl(kubectl.into_os_string(), dir.path().join("kubeconfig"));
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
    let mut config = demo::config();
    fleet.path().to_str().unwrap().clone_into(&mut config.flake);
    config.public_machines.clear();
    config.rolling.rules = [("k8s-server".to_owned(), rule)].into();
    config.rolling.parallel = 2;
    config.rolling.ready_timeout_seconds = ready_timeout;
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

/// `U`, and `y` to the plan once it is asked.
async fn roll(shell: &Arc<Shell>) {
    shell.send(Event::UpdateAll);
    until(shell, "the plan to be asked about", |v| {
        v.confirm
            .as_ref()
            .is_some_and(|c| c.kind == ConfirmKind::UpdateAll)
    })
    .await;
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
            "end" => now -= 1,
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

/// The whole thing, for real processes: luna then sol (never together) beside cypramundi, the
/// output of each in the Log tab, one action-log row each, and the cluster asked only for
/// nodes (no drain was configured).
#[tokio::test]
async fn u_updates_the_group_one_at_a_time_beside_the_free_machine() {
    let Some(t) = tools(&["sleep", "cat"]) else {
        return;
    };
    let w = world(
        &t,
        &clan_body("0.4"),
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
    // luna before sol, whole.
    let at = |e: &str| events.iter().position(|x| x == e).unwrap();
    assert!(at("end luna") < at("start sol"), "{events:?}");
    // The free machine ran beside the group, not after it.
    assert!(
        at("start cypramundi") < at("end sol"),
        "cypramundi is not part of the group: {events:?}"
    );
    // Nothing but the node list was asked of the cluster: the check and one wait per machine.
    let calls = w.kubectl_calls();
    assert!(
        calls.iter().all(|c| c.contains("get nodes -o json")),
        "{calls:?}"
    );
    assert_eq!(
        calls.len(),
        3,
        "the plan's look, then one wait for each of luna and sol: {calls:?}"
    );
    // The Log tab: the plan first, a run for each machine with clan's own output.
    let all: Vec<String> = w
        .shell
        .output()
        .window(0, 200)
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(all[0], "── update all ──", "{all:?}");
    assert!(all.contains(&"update 3 machines:".to_owned()), "{all:?}");
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
    }
}

/// With `drain = true`, in the order the brief says: drain, update, wait for Ready, uncordon,
/// and the next machine's drain only after that.
#[tokio::test]
async fn with_drain_kubectl_is_drain_wait_uncordon_for_each_machine_in_turn() {
    let Some(t) = tools(&["sleep", "cat"]) else {
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
    let kc = |s: &str| format!("--kubeconfig {}/kubeconfig {s}", w.dir.path().display());
    let drain = |m: &str| {
        kc(&format!(
            "drain {m} --ignore-daemonsets --delete-emptydir-data --timeout 60s"
        ))
    };
    assert_eq!(
        calls,
        [
            kc("get nodes -o json"),
            drain("luna"),
            kc("get nodes -o json"),
            kc("uncordon luna"),
            drain("sol"),
            kc("get nodes -o json"),
            kc("uncordon sol"),
        ],
        "{calls:#?}"
    );
    w.shell.shutdown().await;
}

/// A failed drain stops the group and the node is uncordoned again; the machine is never
/// updated, and the free machine is.
#[tokio::test]
async fn a_failed_drain_uncordons_and_never_runs_clan_for_that_machine() {
    let Some(t) = tools(&["sleep", "cat"]) else {
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
}

/// A cluster with a `NotReady` node is not rolled: `clan` is never run for its machines.
#[tokio::test]
async fn a_notready_node_means_clan_is_never_run_for_the_cluster() {
    let Some(t) = tools(&["sleep", "cat"]) else {
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
    w.shell.send(Event::UpdateAll);
    until(&w.shell, "the plan", |v| {
        v.confirm
            .as_ref()
            .is_some_and(|c| c.kind == ConfirmKind::UpdateAll)
    })
    .await;
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

/// A quit during the roll kills the running `clan` processes, never starts the next machine,
/// and gives back a node auspex cordoned; the shell has nothing left to wait for.
#[tokio::test]
async fn quitting_mid_roll_kills_clan_starts_nothing_and_uncordons() {
    let Some(t) = tools(&["sleep", "cat"]) else {
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
    let pids = w.path("clan.pids");
    // luna (drained) and cypramundi are both updating.
    let running = {
        let pids = pids.clone();
        // A blocking wait would stop the runtime that is running the roll.
        let mut found = Vec::new();
        for _ in 0..400 {
            found = fs::read_to_string(&pids)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse::<i32>().ok())
                .collect();
            if found.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(found.len() >= 2, "both updates started: {found:?}");
        found
    };
    until(&w.shell, "luna to be updating", |v| {
        v.roll.as_ref().is_some_and(|r| {
            r.machines
                .iter()
                .any(|m| m.name == "luna" && m.state == RollState::Updating)
        })
    })
    .await;

    w.shell.send(Event::Quit);
    assert_eq!(w.shell.view().confirm.unwrap().kind, ConfirmKind::Quit);
    w.shell.send(Event::ConfirmAnswer(true));
    until(&w.shell, "the roll to stop and quit", |v| v.quit).await;
    let left = still_alive_after_wait(&running);
    cleanup(&left);
    assert_eq!(left, Vec::<i32>::new(), "clan was left running");
    assert!(running.iter().all(|p| !alive(*p)));
    assert_eq!(state(&w.shell, "luna"), RollState::Cancelled);
    assert!(
        matches!(state(&w.shell, "sol"), RollState::Skipped(_)),
        "sol never started"
    );
    let calls = w.kubectl_calls();
    assert!(
        calls
            .iter()
            .any(|c| c
                .ends_with("drain luna --ignore-daemonsets --delete-emptydir-data --timeout 60s")),
        "{calls:#?}"
    );
    assert!(
        calls.last().is_some_and(|c| c.ends_with("uncordon luna")),
        "the node is given back last: {calls:#?}"
    );
    assert_eq!(
        w.clan_events()
            .iter()
            .filter(|e| e.starts_with("start "))
            .count(),
        2
    );
    w.shell.shutdown().await;
}

/// The wait between machines is for the node: one that never becomes `Ready` after its machine
/// was updated fails that machine (with the node named) and stops the group. The fake `kubectl`
/// answers the plan's look at the cluster with a healthy one and every later look with `luna`
/// `NotReady`.
#[tokio::test]
async fn a_node_that_never_becomes_ready_fails_the_machine_and_stops_the_group() {
    let Some(t) = tools(&["sleep", "cat"]) else {
        return;
    };
    let down = r#"{"items":[{"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"False"}]}},{"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#;
    let kubectl = format!(
        "printf '%s\\n' \"$*\" >> '@dir@/kubectl.log'\n\
         if [ -e '@dir@/asked' ]; then cat <<'EOF'\n{down}\nEOF\n\
         else touch '@dir@/asked'; cat <<'EOF'\n{NODES_READY}\nEOF\nfi\n"
    );
    let w = world(&t, &clan_body("0.1"), &kubectl, kube_rule(false), 1);
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
