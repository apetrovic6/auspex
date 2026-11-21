//! `U`, decided by the core alone: a hand-driven shell, no process, no clock.
//!
//! The fleet is the one the feature was written for, in miniature: `luna`, `mars`, `sol` and
//! `terra` are the cluster (rolling tag `taghmata`, Kubernetes), `alfrost` and `cerberus` are
//! machines of no rolling tag. Every test names the rule it pins; the report lists how each was
//! broken to see it fail.

mod support;

use std::collections::BTreeSet;

use auspex_core::effects::{
    ActionEnd, ActionRequest, ActionResult, AwaitRequest, EndResult, EvalAnswer, KubeOp, KubeReply,
    KubeRequest, LogOp, PreflightRequest,
};
use auspex_core::{
    Config, ConfirmKind, Event, FreezeConfig, Mode, Phase, RollState, RollStatus, RollingConfig,
    SavedFreezes,
};
use auspex_domain::roll::{ClusterId, EtcdPod, Node, RollRule};
use auspex_domain::status::Badge;
use crux_core::Request;
use support::{Shell, clean_preflight, evaluated, observed, path};

const FLEET: &str = r#"{
  "alfrost":  {"deploy": {"targetHost": "root@192.168.1.1"}},
  "cerberus": {"deploy": {"targetHost": "root@192.168.1.2"}},
  "luna":     {"tags": ["taghmata", "skitarii"], "deploy": {"targetHost": "root@192.168.1.11"}},
  "mars":     {"tags": ["taghmata"],             "deploy": {"targetHost": "root@192.168.1.12"}},
  "sol":      {"tags": ["taghmata", "skitarii"], "deploy": {"targetHost": "root@192.168.1.13"}},
  "terra":    {"tags": ["taghmata", "skitarii"], "deploy": {"targetHost": "root@192.168.1.14"}}
}"#;

const ALL: [&str; 6] = ["alfrost", "cerberus", "luna", "mars", "sol", "terra"];
const CLUSTER: [&str; 4] = ["luna", "mars", "sol", "terra"];

fn old(m: &str) -> String {
    path('a', &format!("nixos-system-{m}-old"))
}

fn new(m: &str) -> String {
    path('b', &format!("nixos-system-{m}-new"))
}

fn config_with(rule: RollRule, parallel: usize) -> Config {
    Config {
        flake: "/fleet".to_owned(),
        allow_act: true,
        local_system: "x86_64-linux".to_owned(),
        rolling: RollingConfig {
            rules: [("taghmata".to_owned(), rule)].into(),
            parallel,
            ..RollingConfig::default()
        },
        ..Config::default()
    }
}

fn k8s() -> RollRule {
    RollRule {
        kubernetes: true,
        ..RollRule::default()
    }
}

const fn draining() -> RollRule {
    RollRule {
        kubernetes: true,
        drain: true,
        drain_timeout_seconds: 600,
        etcd: false,
    }
}

/// The six machines, scanned to the end, each running `old` and evaluating to `new`, in act mode.
fn settled(config: Config) -> Shell {
    settled_with(config, |_| String::new())
}

/// [`settled`], with `extra(machine)` more probe lines for each machine.
fn settled_with(config: Config, extra: impl Fn(&str) -> String) -> Shell {
    let mut s = Shell::new();
    s.start_with(config, FLEET);
    let mut eval = s.take_eval();
    for m in ALL {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ALL {
        s.probe(m, observed(&old(m), &extra(m)));
    }
    assert_eq!(s.view().refresh.phase, Phase::Settled);
    assert_eq!(s.row("luna").status, Badge::UpdatePending);
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().mode, Mode::Act);
    s
}

fn node(name: &str, ready: bool, cordoned: bool) -> Node {
    Node {
        name: name.to_owned(),
        ready,
        cordoned,
        heartbeat: None,
        condition: None,
        lease: None,
        etcd: false,
    }
}

fn cluster() -> ClusterId {
    ClusterId {
        context: "rke2".to_owned(),
        server: "https://10.0.0.1:6443".to_owned(),
        kubeconfig: "/k/config".to_owned(),
    }
}

fn healthy_cluster() -> Vec<Node> {
    CLUSTER.iter().map(|n| node(n, true, false)).collect()
}

/// The one Kubernetes request outstanding, taken.
fn take_kube(s: &mut Shell) -> Request<KubeRequest> {
    assert_eq!(
        s.kubes.len(),
        1,
        "one call to the cluster outstanding, got {:?}",
        s.kubes.iter().map(|k| &k.operation.op).collect::<Vec<_>>()
    );
    s.kubes.remove(0)
}

/// The plan asks which cluster this is, then for its nodes: answers both.
fn answer_nodes(s: &mut Shell, nodes: Vec<Node>) {
    let mut k = take_kube(s);
    assert_eq!(k.operation.op, KubeOp::Context);
    assert_eq!(
        k.operation.context, None,
        "the question that names the context has none"
    );
    s.resolve(&mut k, Ok(KubeReply::Cluster(cluster())));
    let mut k = take_kube(s);
    assert_eq!(k.operation.op, KubeOp::Nodes);
    assert_eq!(k.operation.context.as_deref(), Some("rke2"));
    s.resolve(&mut k, Ok(KubeReply::Nodes(nodes)));
}

fn take_preflight(s: &mut Shell, machine: &str) -> Request<PreflightRequest> {
    let i = s
        .preflights
        .iter()
        .position(|r| r.operation.machine == machine)
        .unwrap_or_else(|| panic!("no checks asked for {machine}"));
    s.preflights.remove(i)
}

fn take_action(s: &mut Shell, machine: &str) -> Request<ActionRequest> {
    let i = s
        .actions
        .iter()
        .position(|r| r.operation.machine == machine)
        .unwrap_or_else(|| panic!("no update of {machine} requested"));
    s.actions.remove(i)
}

fn take_await(s: &mut Shell, machine: &str) -> Request<AwaitRequest> {
    let i = s
        .awaits
        .iter()
        .position(|r| r.operation.machine == machine)
        .unwrap_or_else(|| panic!("no wait for {machine}"));
    s.awaits.remove(i)
}

fn preflights(s: &Shell) -> BTreeSet<String> {
    s.preflights
        .iter()
        .map(|r| r.operation.machine.clone())
        .collect()
}

fn actions(s: &Shell) -> BTreeSet<String> {
    s.actions
        .iter()
        .map(|r| r.operation.machine.clone())
        .collect()
}

fn state(s: &Shell, machine: &str) -> RollState {
    s.view()
        .roll
        .expect("a roll")
        .machines
        .into_iter()
        .find(|m| m.name == machine)
        .unwrap_or_else(|| panic!("{machine} is not in the roll"))
        .state
}

fn ended(result: ActionResult) -> ActionEnd {
    ActionEnd {
        result,
        exit_code: match result {
            ActionResult::Succeeded => Some(0),
            ActionResult::Failed => Some(1),
            _ => None,
        },
        summary: vec!["the last line".to_owned()],
        explanation: None,
    }
}

/// `U`, the cluster's answer when it is asked for, and `y`.
fn start(s: &mut Shell, nodes: Vec<Node>) {
    s.send(Event::UpdateAll);
    if !s.kubes.is_empty() {
        answer_nodes(s, nodes);
    }
    let q = s.view().confirm.expect("the plan is asked about");
    assert_eq!(q.kind, ConfirmKind::UpdateAll, "{q:?}");
    s.send(Event::ConfirmAnswer(true));
    assert!(s.view().roll.is_some());
}

/// Answers the checks of `machine` cleanly, then its baseline probe, then (a machine of the cluster)
/// the read of the cluster that comes before it goes out of service.
fn pass_checks(s: &mut Shell, machine: &str) {
    let mut p = take_preflight(s, machine);
    s.resolve(&mut p, clean_preflight());
    s.probe(machine, observed(&old(machine), ""));
    if CLUSTER.contains(&machine)
        && let Some(i) = s.kubes.iter().position(|k| k.operation.op == KubeOp::Nodes)
    {
        let mut k = s.kubes.remove(i);
        s.resolve(&mut k, Ok(KubeReply::Nodes(healthy_cluster())));
    }
}

/// A whole machine, start to finish, with nothing special: checks, update, healthy.
fn finish_machine(s: &mut Shell, machine: &str) {
    pass_checks(s, machine);
    let mut a = take_action(s, machine);
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let mut w = take_await(s, machine);
    s.resolve(&mut w, Ok(()));
}

fn notice(s: &Shell) -> String {
    s.view().notice.expect("a notice")
}

fn log_lines(s: &Shell) -> Vec<String> {
    s.logs
        .iter()
        .filter_map(|l| match &l.op {
            LogOp::Line { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// The question
// ---------------------------------------------------------------------------------------

/// One question for the whole plan, with the order, what runs in parallel and what is skipped;
/// and nothing is requested before the `y`.
#[test]
fn the_plan_is_one_question_listing_the_order_the_parallelism_and_what_is_skipped() {
    let mut s = settled(Config {
        freeze: FreezeConfig {
            declared: ["cerberus".to_owned()].into(),
            saved: SavedFreezes::default(),
        },
        public_machines: ["alfrost".to_owned()].into(),
        ..config_with(draining(), 2)
    });
    s.send(Event::UpdateAll);
    let checking = s.view().confirm.expect("the cluster is being read first");
    assert_eq!(checking.kind, ConfirmKind::CheckingCluster);
    answer_nodes(&mut s, healthy_cluster());
    let q = s.view().confirm.expect("the plan");
    assert_eq!(q.kind, ConfirmKind::UpdateAll);
    assert_eq!(
        q.lines,
        [
            "update 4 machines:",
            "cluster: context rke2 (https://10.0.0.1:6443); every kubectl call is pinned to it",
            "rolling taghmata (one at a time; each node must be Ready before the next; drains \
             and uncordons luna (600s), mars (600s), sol (600s), terra (600s)): \
             luna → mars → sol → terra",
            "at most 1 machine updating at once: one of the cluster's group",
            "skipped alfrost: marked public: its name has to be typed, so update it with u",
            "skipped cerberus: frozen",
            "each machine is checked at its turn: a refusal skips it, a failure stops its \
             rolling group, and a new warning about the fleet repository refuses it",
        ]
    );
    assert_eq!(q.question, "update 4 machines as listed above? y/N");
    assert!(s.preflights.is_empty() && s.actions.is_empty() && s.kubes.is_empty());
    assert!(s.view().roll.is_none(), "nothing has started");
    // Anything but y is no.
    s.send(Event::ConfirmAnswer(false));
    assert!(s.view().roll.is_none() && s.preflights.is_empty());
}

#[test]
fn free_machines_are_listed_with_their_parallel_limit() {
    let mut s = settled(config_with(k8s(), 2));
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    let lines = s.view().confirm.unwrap().lines;
    assert_eq!(
        lines[2],
        "rolling taghmata (one at a time; each node must be Ready before the next): \
         luna → mars → sol → terra"
    );
    assert_eq!(
        lines[3],
        "in parallel, at most 2 at a time: alfrost, cerberus"
    );
    assert_eq!(
        lines[4],
        "at most 3 machines updating at once: one of the cluster's group + 2 of the machines in \
         parallel"
    );
}

// ---------------------------------------------------------------------------------------
// Order and parallelism
// ---------------------------------------------------------------------------------------

/// Machines sharing a rolling tag never update at the same time, whatever order the answers
/// arrive in; free machines go two at a time; groups run beside the free machines.
#[test]
fn a_rolling_group_is_one_machine_at_a_time_in_name_order_beside_the_free_ones() {
    let mut s = settled(config_with(k8s(), 1));
    start(&mut s, healthy_cluster());
    // luna (the group's first) and alfrost (the free pool's first, with a limit of 1).
    assert_eq!(
        preflights(&s),
        ["alfrost".to_owned(), "luna".to_owned()].into()
    );

    // Whatever else happens, mars waits for luna's whole turn.
    pass_checks(&mut s, "alfrost");
    pass_checks(&mut s, "luna");
    assert_eq!(
        actions(&s),
        ["alfrost".to_owned(), "luna".to_owned()].into()
    );
    let mut luna = take_action(&mut s, "luna");
    assert!(s.preflights.is_empty(), "mars waits");
    s.resolve(&mut luna, ended(ActionResult::Succeeded));
    assert!(s.preflights.is_empty(), "mars waits for luna to be healthy");
    assert_eq!(state(&s, "luna"), RollState::Waiting);
    let wait = take_await(&mut s, "luna");
    assert_eq!(wait.operation.node.as_deref(), Some("luna"), "its node too");
    assert_eq!(wait.operation.expect.as_str(), new("luna"));
    let mut wait = wait;
    s.resolve(&mut wait, Ok(()));
    assert_eq!(state(&s, "luna"), RollState::Done);
    assert_eq!(preflights(&s), ["mars".to_owned()].into(), "now mars");

    // The free pool has a limit of 1: cerberus starts when alfrost is over, not before.
    assert!(!preflights(&s).contains("cerberus"));
    let mut alfrost = take_action(&mut s, "alfrost");
    s.resolve(&mut alfrost, ended(ActionResult::Succeeded));
    let mut w = take_await(&mut s, "alfrost");
    assert_eq!(
        w.operation.node, None,
        "a free machine has no node to wait for"
    );
    s.resolve(&mut w, Ok(()));
    assert_eq!(
        preflights(&s),
        ["cerberus".to_owned(), "mars".to_owned()].into()
    );
}

/// The invariant itself, checked after every step of a whole roll with arbitrary completion
/// order: at no moment are two machines of the cluster between "checks asked" and "ended".
#[test]
fn never_two_machines_of_one_rolling_tag_at_once_through_a_whole_roll() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let busy = |s: &Shell| {
        CLUSTER
            .iter()
            .filter(|m| {
                !matches!(
                    state(s, m),
                    RollState::Queued
                        | RollState::Done
                        | RollState::Refused(_)
                        | RollState::Failed(_)
                        | RollState::Skipped(_)
                        | RollState::Cancelled
                )
            })
            .count()
    };
    let mut turns = 0;
    while s
        .view()
        .roll
        .as_ref()
        .is_some_and(|r| r.status == RollStatus::Running)
    {
        assert!(
            busy(&s) <= 1,
            "two cluster machines at once: {:?}",
            s.view().roll
        );
        // Always advance the one that is furthest on, in whatever kind of request it waits.
        if let Some(m) = s.awaits.first().map(|r| r.operation.machine.clone()) {
            let mut w = take_await(&mut s, &m);
            s.resolve(&mut w, Ok(()));
        } else if let Some(m) = s.actions.first().map(|r| r.operation.machine.clone()) {
            let mut a = take_action(&mut s, &m);
            s.resolve(&mut a, ended(ActionResult::Succeeded));
        } else if let Some(m) = s.preflights.first().map(|r| r.operation.machine.clone()) {
            pass_checks(&mut s, &m);
        } else {
            panic!("stuck: {:?}", s.view().roll);
        }
        turns += 1;
        assert!(turns < 100);
    }
    let roll = s.view().roll.unwrap();
    assert_eq!(roll.status, RollStatus::Ended);
    assert!(
        roll.machines.iter().all(|m| m.state == RollState::Done),
        "{roll:?}"
    );
    assert_eq!(roll.finished(), 6);
}

// ---------------------------------------------------------------------------------------
// Kubernetes
// ---------------------------------------------------------------------------------------

/// Any `NotReady` or cordoned node of the cluster refuses the roll of the cluster's machines;
/// the free machines are still offered.
#[test]
fn a_cluster_with_a_notready_or_cordoned_node_is_not_rolled_and_says_why() {
    for (bad, expect) in [
        (node("sol", false, false), "node sol is NotReady"),
        (node("sol", true, true), "node sol is cordoned"),
    ] {
        let mut s = settled(config_with(k8s(), 2));
        s.send(Event::UpdateAll);
        let mut nodes = healthy_cluster();
        nodes.retain(|n| n.name != "sol");
        nodes.push(bad);
        answer_nodes(&mut s, nodes);
        let q = s
            .view()
            .confirm
            .expect("the free machines are still asked about");
        assert_eq!(q.lines[0], "update 2 machines:");
        assert!(
            q.lines.iter().any(|l| l.contains("alfrost, cerberus")),
            "{:?}",
            q.lines
        );
        for m in CLUSTER {
            let line = q
                .lines
                .iter()
                .find(|l| l.starts_with(&format!("skipped {m}:")))
                .unwrap_or_else(|| panic!("{m} is listed as skipped: {:?}", q.lines));
            assert!(
                line.contains("not rolled with taghmata") && line.contains(expect),
                "{line}"
            );
        }
        s.send(Event::ConfirmAnswer(true));
        // None of the cluster is ever touched.
        assert_eq!(
            preflights(&s),
            ["alfrost".to_owned(), "cerberus".to_owned()].into()
        );
    }
}

#[test]
fn a_cluster_that_cannot_be_read_or_has_no_such_node_is_not_rolled() {
    // kubectl failed.
    let mut s = settled(config_with(k8s(), 2));
    s.send(Event::UpdateAll);
    let mut k = take_kube(&mut s);
    s.resolve(&mut k, Err("Unable to connect to the server".to_owned()));
    let lines = s.view().confirm.unwrap().lines;
    assert!(
        lines.iter().any(|l| l.starts_with("skipped luna:")
            && l.contains("cannot read the cluster: Unable to connect")),
        "{lines:?}"
    );

    // A machine that is no node of the cluster: waiting for it could never end.
    let mut s = settled(config_with(k8s(), 2));
    s.send(Event::UpdateAll);
    answer_nodes(
        &mut s,
        vec![node("luna", true, false), node("mars", true, false)],
    );
    let lines = s.view().confirm.unwrap().lines;
    assert!(
        lines.iter().any(
            |l| l.starts_with("skipped sol:") && l.contains("sol is not a node of the cluster")
        ),
        "{lines:?}"
    );
}

/// No kubeconfig could be chosen: the Kubernetes group is refused without asking anything.
#[test]
fn no_usable_kubeconfig_refuses_the_kubernetes_group_and_asks_nothing() {
    let mut config = config_with(k8s(), 2);
    config.rolling.kubernetes_unavailable = Some("KUBECONFIG names two files".to_owned());
    let mut s = settled(config);
    s.send(Event::UpdateAll);
    assert!(s.kubes.is_empty(), "the cluster is not asked");
    let lines = s.view().confirm.unwrap().lines;
    assert!(
        lines.iter().any(|l| l.starts_with("skipped luna:")
            && l.contains("kubernetes is not usable here: KUBECONFIG names two files")),
        "{lines:?}"
    );
    assert_eq!(lines[0], "update 2 machines:");
}

/// A rolling tag without `kubernetes` never reads the cluster.
#[test]
fn a_rolling_group_without_kubernetes_never_asks_the_cluster_and_waits_for_no_node() {
    let mut s = settled(config_with(RollRule::default(), 2));
    s.send(Event::UpdateAll);
    assert!(s.kubes.is_empty());
    s.send(Event::ConfirmAnswer(true));
    pass_checks(&mut s, "luna");
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    assert_eq!(take_await(&mut s, "luna").operation.node, None);
}

/// Drain is opt-in: with `drain = false` no cordon, drain or uncordon is ever asked for.
#[test]
fn without_drain_the_node_is_never_cordoned_or_drained() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    for m in CLUSTER {
        finish_machine(&mut s, m);
        assert!(s.kubes.is_empty(), "{m}: nothing was asked of the cluster");
    }
}

/// With `drain = true`: the checks, then the drain, then the update, then the wait, then the
/// uncordon, and the next machine only after the uncordon.
#[test]
fn with_drain_a_machine_is_drained_before_and_uncordoned_after_and_the_next_waits_for_both() {
    let mut s = settled(config_with(draining(), 2));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    assert!(
        s.actions.iter().all(|a| a.operation.machine != "luna"),
        "drain first"
    );
    let mut drain = take_kube(&mut s);
    assert_eq!(
        drain.operation.op,
        KubeOp::Drain {
            node: "luna".to_owned(),
            timeout_seconds: 600
        }
    );
    assert_eq!(state(&s, "luna"), RollState::Draining);
    s.resolve(&mut drain, Ok(KubeReply::Done));
    let mut a = take_action(&mut s, "luna");
    assert_eq!(state(&s, "luna"), RollState::Updating);
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let mut w = take_await(&mut s, "luna");
    assert!(s.kubes.is_empty(), "not uncordoned before it is healthy");
    s.resolve(&mut w, Ok(()));
    let mut un = take_kube(&mut s);
    assert_eq!(
        un.operation.op,
        KubeOp::Uncordon {
            node: "luna".to_owned()
        }
    );
    assert_eq!(state(&s, "luna"), RollState::Uncordoning);
    assert!(
        !preflights(&s).contains("mars"),
        "mars waits for the uncordon"
    );
    s.resolve(&mut un, Ok(KubeReply::Done));
    assert_eq!(state(&s, "luna"), RollState::Done);
    assert!(preflights(&s).contains("mars"));
}

/// A failed or timed-out drain stops the roll of the group and leaves the node uncordoned again;
/// the machine is never updated, and the other machines carry on.
#[test]
fn a_failed_drain_stops_the_group_uncordons_and_never_updates_the_machine() {
    let mut s = settled(config_with(draining(), 2));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut drain = take_kube(&mut s);
    s.resolve(
        &mut drain,
        Err("kubectl drain did not finish within 600s and was stopped".to_owned()),
    );
    // The node is given back before the machine ends.
    let mut un = take_kube(&mut s);
    assert_eq!(
        un.operation.op,
        KubeOp::Uncordon {
            node: "luna".to_owned()
        }
    );
    assert_eq!(state(&s, "luna"), RollState::Uncordoning);
    s.resolve(&mut un, Ok(KubeReply::Done));
    match state(&s, "luna") {
        RollState::Failed(why) => assert!(
            why.starts_with("drain failed: kubectl drain did not finish"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    for m in ["mars", "sol", "terra"] {
        assert!(matches!(state(&s, m), RollState::Skipped(_)), "{m}");
    }
    assert!(
        s.actions.iter().all(|a| a.operation.machine != "luna"),
        "never updated"
    );
    assert!(
        !preflights(&s).contains("mars"),
        "the rest of the group never starts"
    );
    // The free machines were not part of it.
    assert_eq!(
        preflights(&s),
        ["alfrost".to_owned(), "cerberus".to_owned()].into()
    );
    assert_eq!(s.view().roll.unwrap().status, RollStatus::Running);
}

#[test]
fn an_uncordon_that_fails_is_a_failure_that_says_to_do_it_by_hand() {
    let mut s = settled(config_with(draining(), 2));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut drain = take_kube(&mut s);
    s.resolve(&mut drain, Err("evicting pod web-1 failed".to_owned()));
    let mut un = take_kube(&mut s);
    s.resolve(&mut un, Err("connection refused".to_owned()));
    match state(&s, "luna") {
        RollState::Failed(why) => {
            assert!(
                why.contains("drain failed: evicting pod web-1 failed"),
                "{why}"
            );
            assert!(
                why.contains("uncordon it by hand (kubectl --kubeconfig /k/config --context rke2 uncordon luna)"),
                "{why}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// After a failed update or a machine that never became healthy the node **stays cordoned**: the
/// machine may be broken, and workloads must not return to it unasked. It is said everywhere an
/// operator looks, with the command that gives it back.
#[test]
fn a_drained_node_stays_cordoned_after_a_failed_update_and_after_a_failed_wait_and_says_so() {
    for failing in ["update", "wait"] {
        let mut s = settled(config_with(draining(), 2));
        start(&mut s, healthy_cluster());
        pass_checks(&mut s, "luna");
        let mut drain = take_kube(&mut s);
        s.resolve(&mut drain, Ok(KubeReply::Done));
        let mut a = take_action(&mut s, "luna");
        if failing == "update" {
            s.resolve(&mut a, ended(ActionResult::Failed));
        } else {
            s.resolve(&mut a, ended(ActionResult::Succeeded));
            let mut w = take_await(&mut s, "luna");
            s.resolve(&mut w, Err("systemd is degraded".to_owned()));
        }
        assert!(s.kubes.is_empty(), "{failing}: no uncordon is asked for");
        let by_hand = "kubectl --kubeconfig /k/config --context rke2 uncordon luna";
        match state(&s, "luna") {
            RollState::Failed(why) => assert!(
                why.contains("node luna is left cordoned") && why.contains(by_hand),
                "{failing}: {why}"
            ),
            other => panic!("{failing}: {other:?}"),
        }
        assert!(
            matches!(state(&s, "mars"), RollState::Skipped(_)),
            "{failing}"
        );
        assert_eq!(s.view().roll.unwrap().left_cordoned, ["luna"], "{failing}");
        assert!(
            log_lines(&s)
                .iter()
                .any(|l| l.starts_with("luna: failed:") && l.contains(by_hand)),
            "{failing}: the roll's own run says it"
        );
        // The row of the action log is closed with the same words (the shell writes it from this).
        let end = s.logs.iter().find_map(|l| match &l.op {
            LogOp::End { text, result } if text.starts_with("failed:") => Some((text, *result)),
            _ => None,
        });
        let (text, result) = end.expect("luna's run is closed");
        assert!(
            text.contains(by_hand) && text.ends_with("[node drained first]"),
            "{text}"
        );
        assert_eq!(result, EndResult::Failed);
        // Everything else of the roll ends, and the notice at the end names the node.
        finish_machine(&mut s, "alfrost");
        finish_machine(&mut s, "cerberus");
        assert!(notice(&s).contains("LEFT CORDONED: luna"), "{}", notice(&s));
    }
}

// ---------------------------------------------------------------------------------------
// Failures and refusals
// ---------------------------------------------------------------------------------------

/// A failure stops its own group and no other: the free machines finish.
#[test]
fn a_failure_stops_its_group_and_the_other_machines_carry_on() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Failed));
    match state(&s, "luna") {
        RollState::Failed(why) => {
            assert!(why.starts_with("failed, exit 1: the last line"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    for m in ["mars", "sol", "terra"] {
        assert!(matches!(state(&s, m), RollState::Skipped(_)), "{m}");
    }
    assert!(s.awaits.is_empty() && !preflights(&s).contains("mars"));
    // The failed machine is refreshed, as for `u`.
    assert!(s.probes.iter().any(|p| p.operation.machine == "luna"));
    // The free machines finish.
    finish_machine(&mut s, "alfrost");
    finish_machine(&mut s, "cerberus");
    let roll = s.view().roll.unwrap();
    assert_eq!(roll.status, RollStatus::Ended);
    assert!(
        notice(&s).contains("1 failed, 3 skipped")
            || notice(&s).contains("2 done, 0 refused, 1 failed, 3 skipped"),
        "{}",
        notice(&s)
    );
}

/// A machine that does not become healthy stops the group, with the reason.
#[test]
fn a_machine_that_never_becomes_healthy_stops_the_group() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let mut w = take_await(&mut s, "luna");
    s.resolve(&mut w, Err("node luna is NotReady".to_owned()));
    match state(&s, "luna") {
        RollState::Failed(why) => {
            assert_eq!(why, "not healthy after the update: node luna is NotReady");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&s, "mars"), RollState::Skipped(_)));
}

/// The checks refuse one machine: it is skipped and the roll goes on with the next of its group.
#[test]
fn a_refusal_by_the_checks_skips_that_machine_and_the_roll_goes_on() {
    // mars has 100 KB free on /nix/store, and the closure is 2 GiB.
    let mut s = settled_with(config_with(k8s(), 2), |m| {
        if m == "mars" {
            "store_disk_kb=1000 900 100\n".to_owned()
        } else {
            String::new()
        }
    });
    start(&mut s, healthy_cluster());
    finish_machine(&mut s, "luna");
    let mut p = take_preflight(&mut s, "mars");
    s.resolve(&mut p, clean_preflight());
    match state(&s, "mars") {
        RollState::Refused(why) => assert!(why.contains("/nix/store has"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(
        s.actions.iter().all(|a| a.operation.machine != "mars"),
        "never touched"
    );
    // sol is next, not skipped.
    assert!(preflights(&s).contains("sol"));
    assert_eq!(state(&s, "sol"), RollState::Checking);
    assert!(
        log_lines(&s)
            .iter()
            .any(|l| l.starts_with("mars: refused: ")),
        "{:?}",
        log_lines(&s)
    );
    finish_machine(&mut s, "sol");
    finish_machine(&mut s, "terra");
    assert_eq!(state(&s, "terra"), RollState::Done);
}

/// A warning about the size of a closure or a disk that is not known belongs to one machine and
/// cannot be in the plan: it is written into the machine's part of the log and does not stop it.
#[test]
fn a_warning_of_the_checks_about_the_machine_is_logged_and_does_not_stop_it() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let mut p = take_preflight(&mut s, "luna");
    let mut answer = clean_preflight();
    answer.closure = Err("nix could not size it".to_owned());
    s.resolve(&mut p, answer);
    s.probe("luna", observed(&old("luna"), ""));
    let a = {
        let mut k = s.kubes.remove(0);
        s.resolve(&mut k, Ok(KubeReply::Nodes(healthy_cluster())));
        take_action(&mut s, "luna")
    };
    assert!(
        log_lines(&s)
            .iter()
            .any(|l| l.starts_with("warning: the size of the pending closure")),
        "{:?}",
        log_lines(&s)
    );
    assert!(
        a.operation.log.is_some(),
        "the update writes into luna's run"
    );
}

/// The fleet repository's warnings are run once, before the question, and shown in the plan; the
/// machines are then held to them: a warning that was not in the plan refuses the machine.
#[test]
fn the_plans_warnings_about_the_fleet_repository_are_shown_and_a_new_one_refuses_the_machine() {
    let mut s = settled(config_with(k8s(), 2));
    s.tree_answer = Err("not a git repository".to_owned());
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    let lines = s.view().confirm.unwrap().lines;
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("warning: could not check the fleet repository")),
        "{lines:?}"
    );
    s.send(Event::ConfirmAnswer(true));
    // The same warning at luna's turn is the one that was confirmed: it goes on.
    let mut p = take_preflight(&mut s, "luna");
    let mut same = clean_preflight();
    same.tree = Err("not a git repository".to_owned());
    s.resolve(&mut p, same);
    assert_eq!(state(&s, "luna"), RollState::Checking);
    assert!(
        s.probes.iter().any(|p| p.operation.machine == "luna"),
        "its baseline is next"
    );
    // A different one at alfrost's turn (the tree is dirty now) is news.
    let mut p = take_preflight(&mut s, "alfrost");
    let mut dirty = clean_preflight();
    dirty.tree = Err("a different failure".to_owned());
    s.resolve(&mut p, dirty);
    match state(&s, "alfrost") {
        RollState::Refused(why) => {
            assert!(why.contains("not as the confirmed plan found it"), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

/// A tree that was clean when the plan was confirmed and is dirty at a machine's turn: that machine
/// is refused, and the roll goes on.
#[test]
fn a_tree_edited_after_the_y_refuses_the_machine_it_reaches() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let mut p = take_preflight(&mut s, "luna");
    let mut dirty = clean_preflight();
    dirty.tree = Ok(auspex_domain::preflight::TreeState {
        changed_files: 3,
        upstream: auspex_domain::preflight::Upstream::None,
    });
    s.resolve(&mut p, dirty);
    match state(&s, "luna") {
        RollState::Refused(why) => assert!(
            why.contains("uncommitted changes to 3 tracked files"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(
        s.actions.iter().all(|a| a.operation.machine != "luna"),
        "never updated"
    );
    assert!(preflights(&s).contains("mars"), "the group goes on");
}

/// The shell's own last look at the freeze file refuses: a refusal, not a failure.
#[test]
fn a_refusal_by_the_shell_is_a_skip_not_a_failure() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut a = take_action(&mut s, "luna");
    let mut refused = ended(ActionResult::Refused);
    refused.summary = vec!["luna is frozen; not updating it".to_owned()];
    s.resolve(&mut a, refused);
    assert_eq!(
        state(&s, "luna"),
        RollState::Refused("luna is frozen; not updating it".to_owned())
    );
    assert!(preflights(&s).contains("mars"), "the group goes on");
}

// ---------------------------------------------------------------------------------------
// Freezes
// ---------------------------------------------------------------------------------------

#[test]
fn frozen_machines_are_skipped_whichever_way_they_are_frozen() {
    let mut s = settled(Config {
        freeze: FreezeConfig {
            declared: ["sol".to_owned()].into(),
            saved: SavedFreezes::Known(["mars".to_owned()].into()),
        },
        ..config_with(k8s(), 2)
    });
    // A third way: frozen with f in this run.
    s.send(Event::Search("alfrost".to_owned()));
    s.send(Event::ToggleFreeze);
    s.freezes.clear();
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    let lines = s.view().confirm.unwrap().lines;
    for m in ["alfrost", "mars", "sol"] {
        assert!(
            lines.contains(&format!("skipped {m}: frozen")),
            "{m}: {lines:?}"
        );
    }
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(
        preflights(&s),
        ["cerberus".to_owned(), "luna".to_owned()].into()
    );
    finish_machine(&mut s, "luna");
    // mars and sol are not in the roll at all: terra is next.
    assert_eq!(
        preflights(&s),
        ["cerberus".to_owned(), "terra".to_owned()].into()
    );
}

/// While it is unknown which machines are frozen, `U` updates nothing: "the rest" would include
/// the one the operator froze.
#[test]
fn unknown_freezes_refuse_the_whole_u() {
    let mut s = settled(Config {
        freeze: FreezeConfig {
            declared: BTreeSet::new(),
            saved: SavedFreezes::Unknown("corrupt state file".to_owned()),
        },
        ..config_with(k8s(), 2)
    });
    s.send(Event::UpdateAll);
    assert_eq!(
        notice(&s),
        "which machines are frozen is unknown (corrupt state file); not updating anything"
    );
    assert!(s.view().confirm.is_none());
    assert!(s.kubes.is_empty() && s.preflights.is_empty() && s.actions.is_empty());
}

/// A freeze made while the roll runs holds at the machine's turn.
#[test]
fn a_freeze_made_during_the_roll_holds_at_that_machines_turn() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    s.send(Event::Search("terra".to_owned()));
    s.send(Event::ToggleFreeze);
    s.freezes.clear();
    for m in ["luna", "mars", "sol"] {
        finish_machine(&mut s, m);
    }
    match state(&s, "terra") {
        RollState::Refused(why) => assert!(why.contains("terra is frozen"), "{why}"),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------------------

#[test]
fn u_needs_act_mode_and_a_loaded_fleet() {
    let mut s = Shell::new();
    s.start_with(config_with(k8s(), 2), FLEET);
    s.send(Event::UpdateAll);
    // Not scanned yet: nothing is known to be pending, and a scan is running.
    assert!(s.view().confirm.is_none());
    let mut s = settled(config_with(k8s(), 2));
    s.send(Event::ToggleAct);
    assert_eq!(s.view().mode, Mode::Observe);
    s.send(Event::UpdateAll);
    assert_eq!(notice(&s), "act mode is off (A)");
    assert!(s.view().confirm.is_none() && s.kubes.is_empty());
}

#[test]
fn u_with_nothing_pending_says_so() {
    let mut s = Shell::new();
    s.start_with(config_with(k8s(), 2), FLEET);
    let mut eval = s.take_eval();
    for m in ALL {
        s.eval_item(&mut eval, m, evaluated(m, &old(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ALL {
        s.probe(m, observed(&old(m), ""));
    }
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    s.send(Event::UpdateAll);
    assert_eq!(notice(&s), "no machine has an update that U can start");
    assert!(s.view().confirm.is_none());
}

/// While a roll runs, nothing else changes the world under it.
#[test]
fn while_a_roll_runs_u_b_r_and_a_say_so_and_do_nothing() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let before = (s.inventory.len(), s.preflights.len());
    s.send(Event::UpdateAll);
    assert_eq!(notice(&s), "an update of all machines is already running");
    s.send(Event::UpdateSelected);
    assert_eq!(notice(&s), "an update of all machines is running");
    s.send(Event::BuildSelected);
    assert_eq!(notice(&s), "an update of all machines is running");
    s.send(Event::Refresh);
    assert!(notice(&s).starts_with("an update of all machines is running: refresh"));
    assert_eq!(s.inventory.len(), before.0, "no refresh began");
    s.send(Event::ToggleAct);
    assert!(notice(&s).contains("act mode stays on"));
    assert_eq!(s.view().mode, Mode::Act);
    assert!(s.view().confirm.is_none());
}

// ---------------------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------------------

#[test]
fn the_log_has_the_plan_first_a_line_per_machine_and_a_run_for_each_machine() {
    let mut s = settled(config_with(RollRule::default(), 1));
    start(&mut s, Vec::new());
    // The roll's own run opens a new group of runs, and the plan is its first lines.
    let first = &s.logs[0];
    assert!(
        matches!(&first.op, LogOp::Begin { title, group: true } if title == "update all"),
        "{first:?}"
    );
    let plan: Vec<&str> = s
        .logs
        .iter()
        .take(4)
        .filter_map(|l| match &l.op {
            LogOp::Line { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(plan[0], "update 6 machines:");
    // Each machine's run is begun at its turn, as part of the group, and the update writes
    // into it.
    pass_checks(&mut s, "luna");
    let run = s
        .logs
        .iter()
        .find_map(|l| match &l.op {
            LogOp::Begin {
                title,
                group: false,
            } if title == "update luna" => Some(l.run),
            _ => None,
        })
        .expect("luna has a run");
    assert_eq!(take_action(&mut s, "luna").operation.log, Some(run));
    for m in ALL {
        if m != "luna" {
            // The others are driven in whatever order they come.
            while !s.preflights.is_empty() || !s.actions.is_empty() || !s.awaits.is_empty() {
                if let Some(p) = s.preflights.first().map(|r| r.operation.machine.clone()) {
                    pass_checks(&mut s, &p);
                } else if let Some(a) = s.actions.first().map(|r| r.operation.machine.clone()) {
                    let mut a = take_action(&mut s, &a);
                    s.resolve(&mut a, ended(ActionResult::Succeeded));
                } else if let Some(w) = s.awaits.first().map(|r| r.operation.machine.clone()) {
                    let mut w = take_await(&mut s, &w);
                    s.resolve(&mut w, Ok(()));
                }
            }
            break;
        }
    }
    assert_eq!(
        s.view().roll.unwrap().status,
        RollStatus::Running,
        "luna's update is still out"
    );
}

// ---------------------------------------------------------------------------------------
// Quitting
// ---------------------------------------------------------------------------------------

/// `q` asks; on `y` nothing new starts, the running updates are cancelled, and quit comes only when
/// every machine has ended. A node whose update was under way stays cordoned (it is named); one
/// drained and not yet updated is given back (the next test).
#[test]
fn quitting_asks_cancels_the_updates_never_starts_the_next_and_names_the_cordon() {
    let mut s = settled(config_with(draining(), 1));
    start(&mut s, healthy_cluster());
    // luna: drained and updating. alfrost: updating. cerberus: queued behind alfrost.
    pass_checks(&mut s, "luna");
    let mut drain = take_kube(&mut s);
    s.resolve(&mut drain, Ok(KubeReply::Done));
    pass_checks(&mut s, "alfrost");
    let luna_id = s
        .actions
        .iter()
        .find(|a| a.operation.machine == "luna")
        .unwrap()
        .operation
        .id;
    let alfrost_id = s
        .actions
        .iter()
        .find(|a| a.operation.machine == "alfrost")
        .unwrap()
        .operation
        .id;

    s.send(Event::Quit);
    let q = s.view().confirm.expect("quit asks");
    assert_eq!(q.kind, ConfirmKind::Quit);
    assert!(
        q.question
            .starts_with("an update of all machines is running (0 of 6 over; now: alfrost, luna)"),
        "{}",
        q.question
    );
    assert!(q.question.ends_with("y/N"));
    assert!(!s.view().quit);
    assert!(
        s.cancels.is_empty(),
        "nothing is cancelled before the answer"
    );

    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().roll.as_ref().unwrap().status, RollStatus::Stopping);
    assert!(s.cancels.contains(&luna_id) && s.cancels.contains(&alfrost_id));
    assert!(!s.view().quit, "quit waits for the processes");
    assert!(
        matches!(state(&s, "cerberus"), RollState::Skipped(_)),
        "never starts"
    );
    assert!(matches!(state(&s, "mars"), RollState::Skipped(_)));

    // The shell answers a cancelled action once its process group is gone.
    let mut a = take_action(&mut s, "alfrost");
    s.resolve(&mut a, ended(ActionResult::Cancelled));
    assert_eq!(state(&s, "alfrost"), RollState::Cancelled);
    assert!(!s.view().quit, "luna is still stopping");
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Cancelled));
    // luna's update was under way when it was stopped: it may be half done, so its node stays
    // cordoned, and the operator is told with the command.
    assert!(s.kubes.is_empty(), "no uncordon is asked for");
    match state(&s, "luna") {
        RollState::Failed(why) => assert!(
            why.starts_with("cancelled; node luna is left cordoned")
                && why.contains("kubectl --kubeconfig /k/config --context rke2 uncordon luna"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.view().roll.as_ref().unwrap().left_cordoned, ["luna"]);
    assert!(s.view().quit, "now every machine has ended");
    assert_eq!(s.view().roll.unwrap().status, RollStatus::Ended);
    // Nothing was started on the way out.
    assert!(s.preflights.is_empty() && s.actions.is_empty() && s.awaits.is_empty());
}

/// A quit in the middle of a drain: kubectl is cancelled, and only when its answer says it is
/// gone is the node uncordoned (the drain cordoned it).
#[test]
fn quitting_during_a_drain_uncordons_after_the_drain_is_gone() {
    let mut s = settled(config_with(draining(), 1));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut drain = take_kube(&mut s);
    let drain_id = drain.operation.id;
    pass_checks(&mut s, "alfrost");
    let mut a = take_action(&mut s, "alfrost");
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(true));
    assert!(s.cancels.contains(&drain_id));
    assert!(
        s.kubes.is_empty(),
        "no uncordon while the drain may still be running"
    );
    s.resolve(&mut a, ended(ActionResult::Cancelled));
    assert!(!s.view().quit);
    s.resolve(&mut drain, Ok(KubeReply::Cancelled));
    let mut un = take_kube(&mut s);
    assert_eq!(
        un.operation.op,
        KubeOp::Uncordon {
            node: "luna".to_owned()
        }
    );
    assert!(s.actions.is_empty(), "luna is never updated");
    s.resolve(&mut un, Ok(KubeReply::Done));
    assert!(s.view().quit);
}

/// A quit while a machine is being checked or waited for ends it at once (those requests are
/// dropped by the shell, so nothing is left to wait for). A machine that was updated and is being
/// waited for keeps its cordon: nobody knows it is well.
#[test]
fn quitting_while_waiting_for_health_keeps_the_cordon_and_quits() {
    let mut s = settled(config_with(draining(), 1));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut drain = take_kube(&mut s);
    s.resolve(&mut drain, Ok(KubeReply::Done));
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let wait_id = s.awaits[0].operation.id;
    // alfrost is only being checked.
    let check_id = s
        .preflights
        .iter()
        .find(|p| p.operation.machine == "alfrost")
        .unwrap()
        .operation
        .id;
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(true));
    assert!(s.cancels.contains(&wait_id) && s.cancels.contains(&check_id));
    assert!(s.kubes.is_empty(), "no uncordon is asked for");
    assert_eq!(s.view().roll.as_ref().unwrap().left_cordoned, ["luna"]);
    assert!(s.view().quit);
}

/// A second quit while it is stopping forces.
#[test]
fn a_second_quit_forces() {
    let mut s = settled(config_with(k8s(), 1));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(true));
    assert!(!s.view().quit, "luna's update has not been answered");
    s.send(Event::Quit);
    assert!(s.view().quit);
}

/// A termination signal stops without asking.
#[test]
fn a_termination_signal_stops_the_roll_without_a_question() {
    let mut s = settled(config_with(k8s(), 1));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let luna = s.actions[0].operation.id;
    s.send(Event::Terminate);
    assert!(s.cancels.contains(&luna));
    assert!(s.view().confirm.is_none());
    assert!(!s.view().quit, "luna's update has not been answered");
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Cancelled));
    assert!(s.view().quit, "the last machine has ended");
}

/// Answering no to the quit question changes nothing.
#[test]
fn saying_no_to_quit_leaves_the_roll_running() {
    let mut s = settled(config_with(k8s(), 1));
    start(&mut s, healthy_cluster());
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(false));
    assert!(s.cancels.is_empty() && !s.view().quit);
    assert_eq!(s.view().roll.unwrap().status, RollStatus::Running);
}

/// A quit while the plan is still being asked about cancels the cluster look.
#[test]
fn quitting_while_the_cluster_is_being_read_cancels_that() {
    let mut s = settled(config_with(k8s(), 1));
    s.send(Event::UpdateAll);
    let id = s.kubes[0].operation.id;
    s.send(Event::Quit);
    assert!(s.cancels.contains(&id));
    assert!(s.view().quit);
}

// ---------------------------------------------------------------------------------------
// Review round 1
// ---------------------------------------------------------------------------------------

/// A whole machine of a draining group: checks, baseline, the read of the cluster, the drain, the
/// update, the wait, the uncordon.
fn finish_drained(s: &mut Shell, machine: &str) {
    pass_checks(s, machine);
    let mut drain = take_kube(s);
    s.resolve(&mut drain, Ok(KubeReply::Done));
    let mut a = take_action(s, machine);
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let mut w = take_await(s, machine);
    s.resolve(&mut w, Ok(()));
    let mut un = take_kube(s);
    s.resolve(&mut un, Ok(KubeReply::Done));
}

/// F3. The cluster is read again before every machine that uses it, not only for the plan: mars
/// has gone `NotReady` (it was not in the roll, it had nothing pending) and someone cordoned sol.
/// The next machine is not drained, the group stops, and a node auspex did not cordon is never
/// uncordoned by it.
#[test]
fn a_cluster_that_got_sick_during_the_roll_stops_the_group_before_the_next_drain() {
    let mut s = settled(config_with(draining(), 2));
    start(&mut s, healthy_cluster());
    finish_drained(&mut s, "luna");
    // mars's turn: checks, baseline, and then the read, which now says the cluster is short.
    let mut p = take_preflight(&mut s, "mars");
    s.resolve(&mut p, clean_preflight());
    s.probe("mars", observed(&old("mars"), ""));
    let mut read = take_kube(&mut s);
    assert_eq!(read.operation.op, KubeOp::Nodes);
    assert_eq!(read.operation.context.as_deref(), Some("rke2"));
    let sick = vec![
        node("luna", true, false),
        node("mars", true, false),
        node("sol", true, true),
        node("terra", false, false),
    ];
    s.resolve(&mut read, Ok(KubeReply::Nodes(sick)));
    match state(&s, "mars") {
        RollState::Failed(why) => assert_eq!(
            why,
            "the cluster is not healthy: node sol is cordoned; node terra is NotReady"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&s, "sol"), RollState::Skipped(_)));
    assert!(matches!(state(&s, "terra"), RollState::Skipped(_)));
    assert!(
        s.kubes.is_empty(),
        "nothing is drained, and sol's cordon is not auspex's to lift"
    );
    assert!(s.actions.iter().all(|a| a.operation.machine != "mars"));
    assert_eq!(s.view().roll.unwrap().left_cordoned.len(), 0);
}

/// F3. A cluster that cannot be read before a machine stops the group too: not knowing is not
/// permission.
#[test]
fn a_cluster_that_cannot_be_read_before_a_machine_stops_the_group() {
    let mut s = settled(config_with(draining(), 2));
    start(&mut s, healthy_cluster());
    pass_checks_without_read(&mut s, "luna");
    let mut read = take_kube(&mut s);
    s.resolve(&mut read, Err("Unable to connect to the server".to_owned()));
    match state(&s, "luna") {
        RollState::Failed(why) => assert!(
            why.starts_with("the cluster could not be read before it: Unable to connect"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&s, "mars"), RollState::Skipped(_)));
    assert!(s.kubes.is_empty());
}

/// Checks and baseline, leaving the read of the cluster outstanding.
fn pass_checks_without_read(s: &mut Shell, machine: &str) {
    let mut p = take_preflight(s, machine);
    s.resolve(&mut p, clean_preflight());
    s.probe(machine, observed(&old(machine), ""));
}

/// F8. Every call after the plan names the context the plan showed: the reads, the drain, the
/// uncordon and the wait.
#[test]
fn every_call_after_the_plan_is_pinned_to_the_context_the_plan_showed() {
    let mut s = settled(config_with(draining(), 2));
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    let lines = s.view().confirm.unwrap().lines;
    assert!(
        lines[1].starts_with("cluster: context rke2 (https://10.0.0.1:6443)"),
        "{lines:?}"
    );
    s.send(Event::ConfirmAnswer(true));
    pass_checks_without_read(&mut s, "luna");
    let mut contexts = Vec::new();
    let mut read = take_kube(&mut s);
    contexts.push(read.operation.context.clone());
    s.resolve(&mut read, Ok(KubeReply::Nodes(healthy_cluster())));
    let mut drain = take_kube(&mut s);
    contexts.push(drain.operation.context.clone());
    s.resolve(&mut drain, Ok(KubeReply::Done));
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let mut w = take_await(&mut s, "luna");
    contexts.push(w.operation.context.clone());
    s.resolve(&mut w, Ok(()));
    let un = take_kube(&mut s);
    contexts.push(un.operation.context);
    assert!(
        contexts.iter().all(|c| c.as_deref() == Some("rke2")),
        "{contexts:?}"
    );
}

/// F6. The units that were failed when the machine's turn began are its baseline, handed to the
/// wait: a unit that was already failed is not the update's failure. A machine that does not
/// answer a probe at its turn is refused: its health afterwards could not be told from before.
#[test]
fn the_failed_units_at_the_turn_are_the_baseline_and_a_silent_machine_is_refused() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let mut checks = take_preflight(&mut s, "luna");
    s.resolve(&mut checks, clean_preflight());
    s.probe(
        "luna",
        support::observed_failed_unit(&old("luna"), "old.service"),
    );
    let mut read = take_kube(&mut s);
    s.resolve(&mut read, Ok(KubeReply::Nodes(healthy_cluster())));
    let mut update = take_action(&mut s, "luna");
    s.resolve(&mut update, ended(ActionResult::Succeeded));
    let wait = take_await(&mut s, "luna");
    assert_eq!(wait.operation.baseline, ["old.service"]);
    assert!(
        log_lines(&s)
            .iter()
            .any(|l| l.contains("already failed before the update: old.service")),
        "{:?}",
        log_lines(&s)
    );
    // mars: the probe at its turn gets no answer.
    drop(wait);
    let mut checks = take_preflight(&mut s, "alfrost");
    s.resolve(&mut checks, clean_preflight());
    s.probe("alfrost", support::unreachable("no route to host"));
    match state(&s, "alfrost") {
        RollState::Refused(why) => assert_eq!(
            why,
            "it does not answer a probe at its turn: no route to host"
        ),
        other => panic!("{other:?}"),
    }
}

/// F4. The `y` takes the fleet's lock before anything starts, and a refusal (another auspex holds
/// it) starts nothing and says so.
#[test]
fn the_y_takes_the_fleets_lock_and_a_refusal_starts_nothing() {
    let mut s = settled(config_with(k8s(), 2));
    s.lock_answer =
        Err("another auspex is updating this fleet (/fleet): pid 9: update all".to_owned());
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.lock_requests.len(), 1);
    assert_eq!(s.lock_requests[0].what, "update all");
    assert_eq!(
        notice(&s),
        "not updating: another auspex is updating this fleet (/fleet): pid 9: update all"
    );
    assert!(s.view().roll.is_none());
    assert!(s.preflights.is_empty() && s.kubes.is_empty() && s.actions.is_empty());
    // Nothing is held by this one, so nothing is let go either.
    assert_eq!(s.unlocks.len(), 0);
    // And the keys work again: it can be asked again.
    s.lock_answer = Ok(());
    s.send(Event::UpdateAll);
    assert!(s.view().confirm.is_some());
}

/// F4. The lock is held for the whole roll and let go when it is over.
#[test]
fn the_lock_is_held_for_the_whole_roll_and_let_go_when_it_ends() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let lock = s.lock_requests[0].id;
    for m in CLUSTER {
        finish_machine(&mut s, m);
        assert!(s.unlocks.is_empty(), "{m}: still held");
    }
    finish_machine(&mut s, "alfrost");
    finish_machine(&mut s, "cerberus");
    assert_eq!(s.view().roll.unwrap().status, RollStatus::Ended);
    assert_eq!(s.unlocks, [lock]);
}

/// F4. A quit while the lock is being taken starts nothing and lets it go.
#[test]
fn a_roll_stopped_before_it_started_lets_the_lock_go() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let lock = s.lock_requests[0].id;
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(true));
    // luna's checks and alfrost's are dropped, the rest skipped: all over at once.
    assert!(s.view().quit);
    assert_eq!(s.unlocks, [lock]);
}

/// F5. The repository is looked at before the question, and the question waits for it; any key
/// cancels the look.
#[test]
fn the_plan_waits_for_the_look_at_the_repository_and_a_key_cancels_it() {
    let mut s = settled(config_with(RollRule::default(), 2));
    s.hold_tree = true;
    s.send(Event::UpdateAll);
    assert_eq!(s.view().confirm.unwrap().kind, ConfirmKind::CheckingCluster);
    assert_eq!(s.trees.len(), 1);
    let id = s.trees[0].operation.id;
    s.send(Event::ConfirmAnswer(true));
    assert!(
        s.view().confirm.is_none(),
        "a y is not a yes to a question nobody has seen"
    );
    assert!(s.cancels.contains(&id));
    // The late answer is nobody\'s.
    let mut late = s.trees.remove(0);
    s.resolve(&mut late, clean_preflight().tree);
    assert!(s.view().confirm.is_none());
}

/// F11. Two groups that both use the cluster share it: never a machine of each at once. The plan
/// says what runs at once.
#[test]
fn groups_that_use_the_cluster_share_it_and_the_plan_says_what_runs_at_once() {
    let fleet = r#"{
      "alfrost": {"deploy": {"targetHost": "root@192.168.1.1"}},
      "luna":    {"tags": ["a"], "deploy": {"targetHost": "root@192.168.1.11"}},
      "mars":    {"tags": ["a"], "deploy": {"targetHost": "root@192.168.1.12"}},
      "sol":     {"tags": ["b"], "deploy": {"targetHost": "root@192.168.1.13"}},
      "terra":   {"tags": ["b"], "deploy": {"targetHost": "root@192.168.1.14"}}
    }"#;
    let mut config = config_with(k8s(), 2);
    config.rolling.rules = [("a".to_owned(), k8s()), ("b".to_owned(), k8s())].into();
    let mut s = Shell::new();
    s.start_with(config, fleet);
    let mut eval = s.take_eval();
    let names = ["alfrost", "luna", "mars", "sol", "terra"];
    for m in names {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in names {
        s.probe(m, observed(&old(m), ""));
    }
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    let lines = s.view().confirm.unwrap().lines;
    assert!(
        lines.contains(
            &"at most 2 machines updating at once: one of the 2 cluster groups together (they \
              share one cluster) + 1 of the machines in parallel"
                .to_owned()
        ),
        "{lines:?}"
    );
    s.send(Event::ConfirmAnswer(true));
    let busy = |s: &Shell| {
        ["luna", "mars", "sol", "terra"]
            .iter()
            .filter(|m| {
                !matches!(
                    state(s, m),
                    RollState::Queued
                        | RollState::Done
                        | RollState::Refused(_)
                        | RollState::Failed(_)
                        | RollState::Skipped(_)
                        | RollState::Cancelled
                )
            })
            .count()
    };
    let mut turns = 0;
    while s
        .view()
        .roll
        .as_ref()
        .is_some_and(|r| r.status == RollStatus::Running)
    {
        assert!(
            busy(&s) <= 1,
            "two machines of the cluster at once: {:?}",
            s.view().roll
        );
        if let Some(m) = s.awaits.first().map(|r| r.operation.machine.clone()) {
            let mut w = take_await(&mut s, &m);
            s.resolve(&mut w, Ok(()));
        } else if let Some(m) = s.actions.first().map(|r| r.operation.machine.clone()) {
            let mut a = take_action(&mut s, &m);
            s.resolve(&mut a, ended(ActionResult::Succeeded));
        } else if let Some(m) = s.preflights.first().map(|r| r.operation.machine.clone()) {
            pass_checks(&mut s, &m);
        } else {
            panic!("stuck: {:?}", s.view().roll);
        }
        turns += 1;
        assert!(turns < 100);
    }
    let roll = s.view().roll.unwrap();
    assert!(
        roll.machines.iter().all(|m| m.state == RollState::Done),
        "{roll:?}"
    );
}

/// M06. `U` is refused while a refresh is running (the list may be about to change).
#[test]
fn u_is_refused_while_a_refresh_is_running() {
    let mut s = settled(config_with(k8s(), 2));
    s.send(Event::Refresh);
    s.send(Event::UpdateAll);
    assert_eq!(
        notice(&s),
        "a refresh is running; ask again when it has finished"
    );
    assert!(s.view().confirm.is_none() && s.kubes.is_empty());
}

/// M19. An uncordon that was cancelled did not uncordon: the machine is failed and says the node
/// may still be cordoned, with the command.
#[test]
fn a_cancelled_uncordon_is_not_an_uncordon() {
    let mut s = settled(config_with(draining(), 1));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut drain = take_kube(&mut s);
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(true));
    s.resolve(&mut drain, Ok(KubeReply::Cancelled));
    let mut un = take_kube(&mut s);
    s.resolve(&mut un, Ok(KubeReply::Cancelled));
    match state(&s, "luna") {
        RollState::Failed(why) => assert!(
            why.contains(
                "may still be cordoned: could not uncordon it (the request was cancelled)"
            ) && why.contains("kubectl --kubeconfig /k/config --context rke2 uncordon luna"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.view().roll.unwrap().left_cordoned, ["luna"]);
}

/// M06 (the other half). The list is in but the evaluation is still running: what is pending is
/// not settled, so `U` waits too.
#[test]
fn u_is_refused_while_the_evaluation_is_still_running() {
    let mut s = Shell::new();
    s.start_with(config_with(k8s(), 2), FLEET);
    // The list has loaded (the evaluator is out), and nothing has answered yet.
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    s.send(Event::UpdateAll);
    assert_eq!(
        notice(&s),
        "a refresh is running; ask again when it has finished"
    );
    assert!(s.view().confirm.is_none() && s.kubes.is_empty());
}

/// Round 2. A refresh between the `y` and the lock's answer would clear every row, and the roll
/// would then begin to find every machine "not evaluated" and refuse them all: `r` is refused from
/// the `y`, not only from the moment the roll exists.
#[test]
fn a_refresh_between_the_y_and_the_lock_is_refused() {
    let mut s = settled(config_with(k8s(), 2));
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    s.hold_lock = true;
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.held_locks.len(), 1, "the lock is being taken");
    assert!(s.view().roll.is_none());
    s.send(Event::Refresh);
    assert!(
        notice(&s).starts_with("an update of all machines is running: refresh"),
        "{}",
        notice(&s)
    );
    assert_eq!(s.inventory.len(), 0, "no refresh began");
    // The lock arrives and the roll starts on the rows it was planned on.
    let mut lock = s.held_locks.remove(0);
    s.hold_lock = false;
    s.resolve(&mut lock, Ok(()));
    assert_eq!(state(&s, "luna"), RollState::Checking);
    assert!(preflights(&s).contains("luna"));
}

/// Round 3. From the `y` to the lock's answer nothing else may be asked for: `b` (which `r` was
/// already refused beside) would start a build beside the roll, and a `u` would open a question
/// that can only be refused.
#[test]
fn a_build_or_an_update_between_the_y_and_the_lock_is_refused() {
    let mut s = settled(config_with(k8s(), 2));
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    s.hold_lock = true;
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.held_locks.len(), 1, "the lock is being taken");
    for event in [Event::BuildSelected, Event::UpdateSelected] {
        s.send(event);
        assert_eq!(notice(&s), "an update is starting: ask again in a moment");
    }
    assert!(s.actions.is_empty() && s.preflights.is_empty());
    assert!(s.view().confirm.is_none());
    let mut lock = s.held_locks.remove(0);
    s.hold_lock = false;
    s.resolve(&mut lock, Ok(()));
    assert_eq!(state(&s, "luna"), RollState::Checking);
}

/// The by-hand command is meant to be pasted into a shell: a kubeconfig path with a space and a
/// single quote, and a context with a space, are quoted (POSIX single quotes), in the roll's note
/// and so in the Log run line and the action-log row.
#[test]
fn the_by_hand_command_quotes_a_kubeconfig_and_a_context_that_need_it() {
    let mut s = settled(config_with(draining(), 2));
    s.send(Event::UpdateAll);
    let mut k = take_kube(&mut s);
    assert_eq!(k.operation.op, KubeOp::Context);
    s.resolve(
        &mut k,
        Ok(KubeReply::Cluster(ClusterId {
            context: "my ctx".to_owned(),
            server: "https://10.0.0.1:6443".to_owned(),
            kubeconfig: "/my dir/it's/config".to_owned(),
        })),
    );
    let mut k = take_kube(&mut s);
    s.resolve(&mut k, Ok(KubeReply::Nodes(healthy_cluster())));
    s.send(Event::ConfirmAnswer(true));
    pass_checks(&mut s, "luna");
    let mut drain = take_kube(&mut s);
    s.resolve(&mut drain, Ok(KubeReply::Done));
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Failed));
    let by_hand = r"kubectl --kubeconfig '/my dir/it'\''s/config' --context 'my ctx' uncordon luna";
    match state(&s, "luna") {
        RollState::Failed(why) => assert!(why.contains(by_hand), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(log_lines(&s).iter().any(|l| l.contains(by_hand)));
    assert!(
        s.logs
            .iter()
            .any(|l| matches!(&l.op, LogOp::End { text, .. } if text.contains(by_hand)))
    );
}

// ---------------------------------------------------------------------------------------
// etcd
// ---------------------------------------------------------------------------------------

fn etcd_rule() -> RollRule {
    RollRule {
        kubernetes: true,
        etcd: true,
        ..RollRule::default()
    }
}

/// The cluster's four nodes, all labelled etcd members, all Ready.
fn etcd_cluster() -> Vec<Node> {
    CLUSTER
        .iter()
        .map(|n| Node {
            etcd: true,
            ..node(n, true, false)
        })
        .collect()
}

fn pods(ready: &[(&str, bool)]) -> Vec<EtcdPod> {
    ready
        .iter()
        .map(|(n, r)| EtcdPod {
            node: (*n).to_owned(),
            ready: *r,
        })
        .collect()
}

fn all_ready() -> Vec<EtcdPod> {
    pods(&[
        ("luna", true),
        ("mars", true),
        ("sol", true),
        ("terra", true),
    ])
}

/// `U` for an etcd group: the context, the nodes, then the etcd pods, answered; returns the dialog.
fn plan_with_etcd(s: &mut Shell, nodes: Vec<Node>, etcd: Result<KubeReply, String>) -> Vec<String> {
    s.send(Event::UpdateAll);
    let mut k = take_kube(s);
    assert_eq!(k.operation.op, KubeOp::Context);
    s.resolve(&mut k, Ok(KubeReply::Cluster(cluster())));
    let mut k = take_kube(s);
    assert_eq!(k.operation.op, KubeOp::Nodes);
    s.resolve(&mut k, Ok(KubeReply::Nodes(nodes)));
    let mut k = take_kube(s);
    assert_eq!(k.operation.op, KubeOp::EtcdPods);
    assert_eq!(k.operation.context.as_deref(), Some("rke2"));
    s.resolve(&mut k, etcd);
    s.view().confirm.expect("the plan is asked about").lines
}

/// A machine's turn up to the point where its etcd is read, which is answered with `etcd`.
fn etcd_turn(s: &mut Shell, machine: &str, nodes: Vec<Node>, etcd: Result<KubeReply, String>) {
    pass_checks_without_read(s, machine);
    let mut read = take_kube(s);
    assert_eq!(read.operation.op, KubeOp::Nodes);
    s.resolve(&mut read, Ok(KubeReply::Nodes(nodes)));
    let mut read = take_kube(s);
    assert_eq!(read.operation.op, KubeOp::EtcdPods);
    assert_eq!(read.operation.context.as_deref(), Some("rke2"));
    s.resolve(&mut read, etcd);
}

#[test]
fn an_etcd_group_with_every_member_ready_is_planned_and_says_what_it_checks() {
    let mut s = settled(config_with(etcd_rule(), 2));
    let lines = plan_with_etcd(&mut s, etcd_cluster(), Ok(KubeReply::EtcdPods(all_ready())));
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("rolling taghmata (") && l.contains("every etcd member")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("skipped luna")),
        "{lines:?}"
    );
}

#[test]
fn an_etcd_that_is_unhealthy_unreadable_or_unlabelled_refuses_the_group_in_the_plan() {
    for (nodes, etcd, expect) in [
        (
            etcd_cluster(),
            Ok(KubeReply::EtcdPods(pods(&[
                ("luna", true),
                ("mars", false),
                ("sol", true),
                ("terra", true),
            ]))),
            "etcd on mars is not ready",
        ),
        (
            etcd_cluster(),
            Err("Unable to connect to the server".to_owned()),
            "cannot read etcd: Unable to connect",
        ),
        (
            healthy_cluster(),
            Ok(KubeReply::EtcdPods(all_ready())),
            "cannot tell which nodes are etcd members",
        ),
    ] {
        let mut s = settled(config_with(etcd_rule(), 2));
        let lines = plan_with_etcd(&mut s, nodes, etcd);
        let line = lines
            .iter()
            .find(|l| l.starts_with("skipped luna:"))
            .unwrap_or_else(|| panic!("luna is skipped: {lines:?}"));
        assert!(line.contains(expect), "{line}");
        s.send(Event::ConfirmAnswer(true));
        assert!(!preflights(&s).contains("luna"));
    }
}

#[test]
fn a_group_without_etcd_never_asks_for_the_pods() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, etcd_cluster());
    pass_checks(&mut s, "luna");
    assert!(s.kubes.is_empty(), "no pod list is asked for");
}

#[test]
fn a_member_whose_etcd_is_not_ready_at_its_turn_stops_the_group_before_it_goes_out() {
    let mut s = settled(config_with(etcd_rule(), 2));
    plan_with_etcd(&mut s, etcd_cluster(), Ok(KubeReply::EtcdPods(all_ready())));
    s.send(Event::ConfirmAnswer(true));
    // luna: all ready, so it is updated.
    etcd_turn(
        &mut s,
        "luna",
        etcd_cluster(),
        Ok(KubeReply::EtcdPods(all_ready())),
    );
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let mut w = take_await(&mut s, "luna");
    assert!(w.operation.etcd, "the wait asks for the member's etcd too");
    s.resolve(&mut w, Ok(()));
    // mars's turn: sol's etcd is down.
    etcd_turn(
        &mut s,
        "mars",
        etcd_cluster(),
        Ok(KubeReply::EtcdPods(pods(&[
            ("luna", true),
            ("mars", true),
            ("sol", false),
            ("terra", true),
        ]))),
    );
    match state(&s, "mars") {
        RollState::Failed(why) => assert_eq!(why, "etcd is not healthy: etcd on sol is not ready"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&s, "sol"), RollState::Skipped(_)));
    assert!(matches!(state(&s, "terra"), RollState::Skipped(_)));
    assert!(s.actions.iter().all(|a| a.operation.machine != "mars"));
    assert!(s.kubes.is_empty(), "nothing is drained or cordoned");
}

#[test]
fn an_etcd_that_cannot_be_read_at_a_machines_turn_stops_the_group() {
    for etcd in [
        Err("Unable to connect to the server".to_owned()),
        Ok(KubeReply::Done),
        Ok(KubeReply::EtcdPods(Vec::new())),
    ] {
        let mut s = settled(config_with(etcd_rule(), 2));
        plan_with_etcd(&mut s, etcd_cluster(), Ok(KubeReply::EtcdPods(all_ready())));
        s.send(Event::ConfirmAnswer(true));
        etcd_turn(&mut s, "luna", etcd_cluster(), etcd);
        match state(&s, "luna") {
            RollState::Failed(why) => assert!(why.contains("etcd"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(state(&s, "mars"), RollState::Skipped(_)));
        assert!(s.actions.iter().all(|a| a.operation.machine != "luna"));
    }
}

#[test]
fn the_wait_asks_for_etcd_only_when_the_group_does() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    assert!(!take_await(&mut s, "luna").operation.etcd);
}
/// `update.poll_interval` and `update.ready_timeout` reach the wait: the request carries both.
#[test]
fn the_wait_is_asked_to_poll_at_the_configured_interval() {
    let mut config = config_with(k8s(), 2);
    config.rolling.poll_interval_seconds = 7;
    config.rolling.ready_timeout_seconds = 321;
    let mut s = settled(config);
    start(&mut s, healthy_cluster());
    pass_checks(&mut s, "luna");
    let mut a = take_action(&mut s, "luna");
    s.resolve(&mut a, ended(ActionResult::Succeeded));
    let w = take_await(&mut s, "luna");
    assert_eq!(w.operation.poll_seconds, 7);
    assert_eq!(w.operation.timeout_seconds, 321);
}

/// A machine that is a node of the cluster but has no rolling group, or only a group whose rule does
/// not use the cluster, updates beside the cluster's group, which is what the group is for: the plan
/// warns, naming it (and the group, when it has one). A free machine that is no node of the cluster
/// (alfrost) is no business of the cluster's and gets no warning.
#[test]
fn a_node_of_the_cluster_that_rolls_with_no_group_is_warned_about_in_the_plan() {
    let fleet = r#"{
      "alfrost": {"deploy": {"targetHost": "root@192.168.1.1"}},
      "luna":    {"tags": ["a"], "deploy": {"targetHost": "root@192.168.1.11"}},
      "mars":    {"tags": ["a"], "deploy": {"targetHost": "root@192.168.1.12"}},
      "sol":     {"tags": ["b"], "deploy": {"targetHost": "root@192.168.1.13"}},
      "terra":   {"deploy": {"targetHost": "root@192.168.1.14"}}
    }"#;
    let mut config = config_with(k8s(), 2);
    // `b` rolls one at a time but does not use the cluster.
    config.rolling.rules = [
        ("a".to_owned(), k8s()),
        ("b".to_owned(), RollRule::default()),
    ]
    .into();
    let mut s = Shell::new();
    s.start_with(config, fleet);
    let mut eval = s.take_eval();
    let names = ["alfrost", "luna", "mars", "sol", "terra"];
    for m in names {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in names {
        s.probe(m, observed(&old(m), ""));
    }
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    s.send(Event::UpdateAll);
    answer_nodes(&mut s, healthy_cluster());
    let lines = s.view().confirm.unwrap().lines;
    let warnings: Vec<&String> = lines.iter().filter(|l| l.starts_with("warning:")).collect();
    assert_eq!(warnings.len(), 2, "{lines:?}");
    assert!(
        warnings[0].starts_with("warning: sol is a node of the cluster but rolls in group b")
            && warnings[0].contains("does not use the cluster")
            && warnings[0].contains("in parallel with the cluster's group"),
        "{warnings:?}"
    );
    assert!(
        warnings[1].starts_with("warning: terra is a node of the cluster but rolls with no group")
            && warnings[1].contains("in parallel with the cluster's group"),
        "{warnings:?}"
    );
    // The warning is the plan's alone: it does not stop a machine at its turn, and the
    // roll goes ahead on the `y`.
    s.send(Event::ConfirmAnswer(true));
    assert!(
        s.view().roll.is_some(),
        "the roll started: {:?}",
        s.view().notice
    );
}
