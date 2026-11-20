//! `U`, decided by the core alone: a hand-driven shell, no process, no clock.
//!
//! The fleet is the one the feature was written for, in miniature: `luna`, `mars`, `sol` and
//! `terra` are the cluster (rolling tag `taghmata`, Kubernetes), `alfrost` and `cerberus` are
//! machines of no rolling tag. Every test names the rule it pins; the report lists how each was
//! broken to see it fail.

mod support;

use std::collections::BTreeSet;

use auspex_core::effects::{
    ActionEnd, ActionRequest, ActionResult, AwaitRequest, EvalAnswer, KubeOp, KubeReply,
    KubeRequest, LogOp, PreflightRequest,
};
use auspex_core::{
    Config, ConfirmKind, Event, FreezeConfig, Mode, Phase, RollState, RollStatus, RollingConfig,
    SavedFreezes,
};
use auspex_domain::roll::{Node, RollRule};
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

fn answer_nodes(s: &mut Shell, nodes: Vec<Node>) {
    let mut k = take_kube(s);
    assert_eq!(k.operation.op, KubeOp::Nodes);
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

/// Answers the checks of `machine` cleanly.
fn pass_checks(s: &mut Shell, machine: &str) {
    let mut p = take_preflight(s, machine);
    s.resolve(&mut p, clean_preflight());
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
            "rolling taghmata (one at a time; each node must be Ready before the next; drains \
             and uncordons luna (600s), mars (600s), sol (600s), terra (600s)): \
             luna → mars → sol → terra",
            "skipped alfrost: marked public: its name has to be typed, so update it with u",
            "skipped cerberus: frozen",
            "each machine is checked at its turn: a refusal skips it, a failure stops its \
             rolling group",
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
        lines[1],
        "rolling taghmata (one at a time; each node must be Ready before the next): \
         luna → mars → sol → terra"
    );
    assert_eq!(
        lines[2],
        "in parallel, at most 2 at a time: alfrost, cerberus"
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
                why.contains("uncordon it by hand (kubectl uncordon luna)"),
                "{why}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// A drained node is not left cordoned by a failed update either, nor by a machine that never
/// becomes healthy.
#[test]
fn a_drained_node_is_uncordoned_after_a_failed_update_and_after_a_failed_wait() {
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
        let mut un = take_kube(&mut s);
        assert_eq!(
            un.operation.op,
            KubeOp::Uncordon {
                node: "luna".to_owned()
            },
            "{failing}"
        );
        s.resolve(&mut un, Ok(KubeReply::Done));
        assert!(
            matches!(state(&s, "luna"), RollState::Failed(_)),
            "{failing}"
        );
        assert!(
            matches!(state(&s, "mars"), RollState::Skipped(_)),
            "{failing}"
        );
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
    pass_checks(&mut s, "mars");
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

/// Warnings of the checks do not stop a machine: they are written into its part of the log.
#[test]
fn a_warning_of_the_checks_is_logged_and_does_not_stop_the_machine() {
    let mut s = settled(config_with(k8s(), 2));
    start(&mut s, healthy_cluster());
    let mut p = take_preflight(&mut s, "luna");
    let mut answer = clean_preflight();
    answer.tree = Err("not a git repository".to_owned());
    s.resolve(&mut p, answer);
    let a = take_action(&mut s, "luna");
    assert!(
        log_lines(&s)
            .iter()
            .any(|l| l.starts_with("warning: could not check the fleet repository")),
        "{:?}",
        log_lines(&s)
    );
    assert!(
        a.operation.log.is_some(),
        "the update writes into luna's run"
    );
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

/// `q` asks; on `y` nothing new starts, the running updates are cancelled, a node auspex
/// cordoned is uncordoned, and quit comes only when every machine has ended.
#[test]
fn quitting_asks_cancels_the_updates_never_starts_the_next_and_uncordons() {
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
    // luna's node was cordoned by auspex: it is given back before quitting.
    let mut un = take_kube(&mut s);
    assert_eq!(
        un.operation.op,
        KubeOp::Uncordon {
            node: "luna".to_owned()
        }
    );
    assert!(!s.view().quit, "the uncordon is not over");
    s.resolve(&mut un, Ok(KubeReply::Done));
    assert_eq!(state(&s, "luna"), RollState::Cancelled);
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
/// dropped by the shell, so nothing is left to wait for), uncordoning a drained node.
#[test]
fn quitting_while_waiting_for_health_uncordons_and_quits() {
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
    assert!(!s.view().quit, "luna's node is still cordoned");
    let mut un = take_kube(&mut s);
    assert_eq!(
        un.operation.op,
        KubeOp::Uncordon {
            node: "luna".to_owned()
        }
    );
    s.resolve(&mut un, Ok(KubeReply::Done));
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
