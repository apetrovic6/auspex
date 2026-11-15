//! The core's sequencing rules, driven through its public surface the way a shell drives it:
//! events in, effects out, requests resolved by hand.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use auspex_core::effects::{EvalAnswer, EvalMachine};
use auspex_core::{
    Config, EVAL_MISSING, Event, FleetError, FleetErrorKind, INVENTORY_TIMED_OUT, PROBE_TIMED_OUT,
    Phase, PrometheusConfig, PrometheusStatus, RequestId,
};
use auspex_domain::fleet::MachineClass;
use auspex_domain::observe::HealthSource;
use auspex_domain::status::{Badge, NarCache};
use support::*;

// ---------------------------------------------------------------------------------------
// The list comes first
// ---------------------------------------------------------------------------------------

#[test]
fn the_machine_list_is_requested_first_and_alone() {
    let mut shell = Shell::new();
    shell.send(Event::Start(full_config()));

    assert_eq!(shell.inventory.len(), 1);
    assert_eq!(shell.inventory[0].operation.flake, "/fleet");
    assert_eq!(
        shell.waiting(),
        1,
        "no probe, evaluation, Prometheus, cache or resolve request before the list"
    );
    let view = shell.view();
    assert_eq!(view.refresh.phase, Phase::ListingMachines);
    assert_eq!(view.report.machines.len(), 0);
    assert!(
        !view.refresh.settled,
        "an empty fleet is not settled while the list is out"
    );

    // And only once it arrives does the rest go out.
    let mut list = shell.take_inventory();
    shell.resolve(&mut list, Ok(inventory(THREE)));
    assert_eq!(shell.probes.len(), 3);
    assert_eq!(shell.evals.len(), 1);
    assert_eq!(shell.prometheus.len(), 1);
    assert_eq!(shell.resolves.len(), 0, "every target is an IP literal");
    assert!(
        shell.caches.is_empty(),
        "nothing is known to be pending yet"
    );
}

#[test]
fn one_probe_per_valid_target_and_none_for_a_refused_one() {
    let mut shell = Shell::new();
    let mut config = bare_config();
    config
        .target_overrides
        .insert("luna".to_owned(), "admin@192.168.1.99:2222".to_owned());
    config
        .target_overrides
        .insert("ghost".to_owned(), "root@10.0.0.1".to_owned());
    shell.start_with(
        config,
        r#"{
          "terra":  {"deploy": {"targetHost": "root@192.168.1.10"}},
          "luna":   {"deploy": {"targetHost": "root@192.168.1.11"}},
          "genetor":{"deploy": {"targetHost": "admin@192.168.1.109"}, "machineClass": "darwin"},
          "broken": {"deploy": {"targetHost": "-oProxyCommand=sh"}}
        }"#,
    );

    let mut probed: Vec<_> = shell
        .probes
        .iter()
        .map(|r| {
            (
                r.operation.machine.clone(),
                r.operation.target.to_string(),
                r.operation.class,
            )
        })
        .collect();
    probed.sort();
    assert_eq!(
        probed,
        [
            (
                "genetor".to_owned(),
                "admin@192.168.1.109".to_owned(),
                MachineClass::Darwin
            ),
            (
                "luna".to_owned(),
                "admin@192.168.1.99:2222".to_owned(),
                MachineClass::Nixos
            ),
            (
                "terra".to_owned(),
                "root@192.168.1.10".to_owned(),
                MachineClass::Nixos
            ),
        ],
        "one probe each, the override winning, and none for the refused target"
    );
    let ids: BTreeSet<RequestId> = shell.probes.iter().map(|r| r.operation.id).collect();
    assert_eq!(ids.len(), 3, "each probe has its own id");

    // The refused machine is settled as the domain says, before anything answers.
    let broken = shell.row("broken");
    assert_eq!(broken.status, Badge::Unreachable);
    assert_eq!(broken.badges, [Badge::Unreachable]);
    assert!(broken.reason.unwrap().starts_with("invalid target"));
    assert!(broken.target_error.is_some());
    assert_eq!(shell.view().refresh.pending_machines, 3);

    // Evaluation does not need ssh, so the refused machine is evaluated too.
    let eval = shell.take_eval();
    assert_eq!(
        eval.operation.machines,
        [
            EvalMachine {
                name: "broken".to_owned(),
                class: MachineClass::Nixos
            },
            EvalMachine {
                name: "genetor".to_owned(),
                class: MachineClass::Darwin
            },
            EvalMachine {
                name: "luna".to_owned(),
                class: MachineClass::Nixos
            },
            EvalMachine {
                name: "terra".to_owned(),
                class: MachineClass::Nixos
            },
        ]
    );
    assert_eq!(shell.prometheus.len(), 0, "Prometheus is not configured");
    assert!(
        shell
            .view()
            .report
            .machines
            .iter()
            .all(|m| m.name != "ghost"),
        "an override for a machine the inventory lacks adds no row"
    );
}

// ---------------------------------------------------------------------------------------
// Order independence
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Answer {
    Probe,
    Eval,
    Prometheus,
}

fn permutations() -> Vec<[Answer; 3]> {
    use Answer::{Eval as E, Probe as P, Prometheus as M};
    vec![
        [P, E, M],
        [P, M, E],
        [E, P, M],
        [E, M, P],
        [M, P, E],
        [M, E, P],
    ]
}

#[test]
fn answers_in_any_order_give_the_same_report() {
    let now = path('0', "nixos-system-terra-a");
    let next = path('1', "nixos-system-terra-b");
    let mut reports = Vec::new();
    for order in permutations() {
        let mut shell = Shell::new();
        shell.start_with(full_config(), THREE);
        let mut eval = shell.take_eval();
        for step in order {
            match step {
                Answer::Probe => shell.probe("terra", observed(&now, "")),
                Answer::Eval => shell.eval_item(&mut eval, "terra", evaluated("terra", &next)),
                Answer::Prometheus => {
                    let mut prom = shell.take_prometheus();
                    let samples = BTreeMap::from([("terra".to_owned(), sample(7, 5))]);
                    shell.resolve(&mut prom, Ok(samples));
                }
            }
        }
        // The cache question goes out once both the probe and the toplevel are known,
        // whichever came last.
        assert_eq!(shell.caches.len(), 1, "{order:?}");
        let mut cache = shell.take_cache("terra");
        assert_eq!(cache.operation.store_path.as_str(), next);
        shell.resolve(&mut cache, NarCache::Cached);
        reports.push((order, shell.row("terra")));
    }
    let (_, first) = &reports[0];
    assert_eq!(first.status, Badge::UpdatePending);
    assert_eq!(first.badges, [Badge::UpdatePending, Badge::Cached]);
    assert_eq!(
        first.health.uptime_seconds.unwrap().source,
        HealthSource::Prometheus
    );
    assert_eq!(first.health.load.unwrap().source, HealthSource::Ssh);
    for (order, report) in &reports {
        assert_eq!(report, first, "{order:?}");
    }
}

#[test]
fn one_answer_changes_only_its_own_row() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    let before = shell.view().report.machines;
    shell.probe("luna", unreachable("Connection refused"));
    let after = shell.view().report.machines;
    for (b, a) in before.iter().zip(&after) {
        if a.name == "luna" {
            assert_eq!(a.status, Badge::Unreachable);
        } else {
            assert_eq!(a, b, "{} changed", a.name);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Generations and cancellation
// ---------------------------------------------------------------------------------------

#[test]
fn an_answer_from_an_older_generation_changes_nothing() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    let mut old_probe = shell.take_probe("terra");
    let mut old_eval = shell.take_eval();
    let mut old_prom = shell.take_prometheus();

    shell.send(Event::Refresh);
    let mut list = shell.take_inventory();
    assert_eq!(list.operation.generation.0, 2);
    shell.resolve(&mut list, Ok(inventory(THREE)));
    let before = shell.view();

    let now = path('0', "nixos-system-terra-a");
    shell.resolve(&mut old_probe, observed(&now, ""));
    shell.resolve(
        &mut old_eval,
        EvalAnswer::Machine {
            machine: "terra".to_owned(),
            outcome: evaluated("terra", &now),
        },
    );
    shell.resolve(&mut old_eval, EvalAnswer::Finished { error: None });
    shell.resolve(&mut old_prom, Err("down".to_owned()));
    // An answer event naming the old generation directly, as a buggy shell might send.
    // A probe of the *current* generation, so only the generation can refuse it: an old
    // probe's id was cancelled and would be refused for that alone.
    let current_probe = shell
        .probes
        .iter()
        .find(|r| r.operation.generation == before.refresh.generation)
        .unwrap()
        .operation
        .id;
    shell.send(Event::ProbeAnswered {
        generation: auspex_core::Generation(1),
        id: current_probe,
        outcome: unreachable("stale"),
    });
    let current_eval = shell.evals[0].operation.id;
    shell.send(Event::EvalAnswered {
        generation: auspex_core::Generation(1),
        id: current_eval,
        answer: EvalAnswer::Finished { error: None },
    });

    assert_eq!(shell.view(), before, "the whole view model is unchanged");
    assert_eq!(
        shell.caches.len(),
        0,
        "and nothing was asked because of them"
    );
}

#[test]
fn a_refresh_cancels_exactly_the_outstanding_requests() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    let now = path('0', "nixos-system-terra-a");
    let next = path('1', "nixos-system-terra-b");
    shell.probe("terra", observed(&now, ""));
    let mut eval = shell.take_eval();
    shell.eval_item(&mut eval, "terra", evaluated("terra", &next));
    assert_eq!(shell.caches.len(), 1, "terra's cache question is out");
    shell.probe("luna", unreachable("No route to host"));

    // Answered: the inventory, terra's and luna's probes. Outstanding: sol's probe, the
    // evaluation stream (one item in, not finished), Prometheus, terra's cache question.
    let outstanding: BTreeSet<RequestId> = [
        shell.probes[0].operation.id,
        eval.operation.id,
        shell.prometheus[0].operation.id,
        shell.caches[0].operation.id,
    ]
    .into();
    assert_eq!(shell.cancels.len(), 0);

    shell.send(Event::Refresh);
    let cancelled: BTreeSet<RequestId> = shell.cancels.iter().copied().collect();
    assert_eq!(cancelled.len(), shell.cancels.len(), "each cancelled once");
    assert_eq!(
        cancelled, outstanding,
        "every outstanding request, and no others"
    );
    assert_eq!(shell.view().refresh.phase, Phase::ListingMachines);
    assert_eq!(shell.view().report.machines.len(), 0);
}

#[test]
fn a_late_answer_after_its_cancel_changes_nothing() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    let mut probe = shell.take_probe("sol");
    let mut eval = shell.take_eval();

    // Cancelled by a quit, within the same generation.
    shell.send(Event::Quit);
    assert!(shell.cancels.contains(&probe.operation.id));
    assert!(shell.cancels.contains(&eval.operation.id));
    let before = shell.view();
    assert!(before.quit);
    shell.resolve(&mut probe, unreachable("too late"));
    shell.eval_item(&mut eval, "sol", evaluated("sol", &path('3', "x")));
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });
    assert_eq!(shell.view(), before);

    // Cancelled by the deadline.
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    let mut probe = shell.take_probe("sol");
    let generation = shell.view().refresh.generation;
    shell.send(Event::DeadlinePassed { generation });
    assert!(shell.cancels.contains(&probe.operation.id));
    let before = shell.view();
    shell.resolve(&mut probe, observed(&path('0', "a"), ""));
    assert_eq!(shell.view(), before);
    assert_eq!(shell.row("sol").reason.as_deref(), Some(PROBE_TIMED_OUT));
}

#[test]
fn request_ids_are_never_reused() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    for _ in 0..3 {
        shell.send(Event::Refresh);
        let mut list = shell.inventory.pop().unwrap();
        shell.resolve(&mut list, Ok(inventory(THREE)));
    }
    let unique: BTreeSet<RequestId> = shell.issued.iter().copied().collect();
    assert_eq!(unique.len(), shell.issued.len());
    assert_eq!(shell.issued.len(), 4 * (1 + 3 + 1 + 1));
}

// ---------------------------------------------------------------------------------------
// Settling
// ---------------------------------------------------------------------------------------

/// Answers everything for `machine` as healthy and in sync.
fn settle(
    shell: &mut Shell,
    eval: &mut crux_core::Request<auspex_core::effects::EvalRequest>,
    machine: &str,
) {
    let sys = path('0', &format!("nixos-system-{machine}"));
    shell.probe(machine, observed(&sys, ""));
    shell.eval_item(eval, machine, evaluated(machine, &sys));
}

#[test]
fn a_host_that_never_answers_delays_nobody_until_the_deadline() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    let mut eval = shell.take_eval();
    settle(&mut shell, &mut eval, "terra");
    settle(&mut shell, &mut eval, "luna");
    // sol's probe never answers; its evaluation does, and the evaluator finishes.
    let sys = path('0', "nixos-system-sol");
    shell.eval_item(&mut eval, "sol", evaluated("sol", &sys));
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });

    let view = shell.view();
    for m in &view.report.machines {
        if m.name == "sol" {
            assert_eq!(m.status, Badge::Checking);
        } else {
            assert_eq!(m.status, Badge::InSync, "{} is settled", m.name);
        }
    }
    assert_eq!(view.refresh.pending_machines, 1);
    assert!(!view.refresh.settled);
    assert!(!view.report.settled);
    assert_eq!(view.refresh.phase, Phase::Scanning);

    let sol = shell.probes[0].operation.id;
    shell.send(Event::DeadlinePassed {
        generation: view.refresh.generation,
    });
    assert_eq!(
        shell.cancels,
        [sol],
        "the one outstanding request is cancelled"
    );
    let view = shell.view();
    assert!(view.refresh.settled);
    assert!(view.refresh.timed_out);
    assert_eq!(view.refresh.phase, Phase::Settled);
    assert!(
        !view.report.settled,
        "the --json contract says false when the timeout passed first"
    );
    let sol = shell.row("sol");
    assert_eq!(sol.status, Badge::Unreachable);
    assert_eq!(sol.reason.as_deref(), Some(PROBE_TIMED_OUT));
    assert_eq!(shell.row("terra").status, Badge::InSync);
}

#[test]
fn a_deadline_for_an_older_generation_is_ignored() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    let old = shell.view().refresh.generation;
    shell.send(Event::Refresh);
    let mut list = shell.take_inventory();
    shell.resolve(&mut list, Ok(inventory(THREE)));
    let before = shell.view();
    let cancels = shell.cancels.len();
    shell.send(Event::DeadlinePassed { generation: old });
    assert_eq!(shell.view(), before);
    assert_eq!(shell.cancels.len(), cancels);
}

#[test]
fn a_fleet_that_answers_settles_without_a_deadline() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    let mut eval = shell.take_eval();
    for m in ["luna", "sol", "terra"] {
        settle(&mut shell, &mut eval, m);
    }
    assert!(
        !shell.view().refresh.settled,
        "the evaluation stream has not said it is finished"
    );
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });
    let view = shell.view();
    assert!(view.refresh.settled);
    assert!(view.report.settled);
    assert!(!view.refresh.timed_out);
}

#[test]
fn a_finished_evaluator_settles_the_machines_it_skipped() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    let mut eval = shell.take_eval();
    for m in ["luna", "sol", "terra"] {
        let sys = path('0', m);
        shell.probe(m, observed(&sys, ""));
    }
    shell.eval_item(&mut eval, "luna", evaluated("luna", &path('0', "luna")));
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });
    assert!(shell.view().refresh.settled);
    assert_eq!(shell.row("sol").status, Badge::EvalError);
    assert_eq!(shell.row("sol").reason.as_deref(), Some(EVAL_MISSING));

    // An evaluator that failed as a whole: its reason on the rows and at fleet level.
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    let mut eval = shell.take_eval();
    shell.resolve(
        &mut eval,
        EvalAnswer::Finished {
            error: Some("error: flake.lock is not up to date".to_owned()),
        },
    );
    assert_eq!(
        shell.row("terra").reason.as_deref(),
        Some("error: flake.lock is not up to date")
    );
    assert_eq!(
        shell.view().report.errors,
        [FleetError::new(
            FleetErrorKind::EvaluationFailed,
            "error: flake.lock is not up to date"
        )]
    );
}

#[test]
fn the_deadline_settles_every_state_it_can_arrive_in() {
    // Before the list, and with the list empty.
    let mut shell = Shell::new();
    shell.send(Event::Start(bare_config()));
    shell.send(Event::DeadlinePassed {
        generation: shell.view().refresh.generation,
    });
    let view = shell.view();
    assert!(view.refresh.settled);
    assert_eq!(
        view.report.errors,
        [FleetError::new(
            FleetErrorKind::InventoryTimedOut,
            INVENTORY_TIMED_OUT
        )]
    );

    let mut shell = Shell::new();
    shell.start_with(full_config(), "{}");
    assert!(
        shell.view().refresh.settled,
        "an empty fleet has nothing to wait for"
    );
    assert_eq!(shell.waiting(), 0);

    // After a quit cancelled everything, rows still spinning.
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    shell.send(Event::Quit);
    assert!(!shell.view().refresh.settled);
    shell.send(Event::DeadlinePassed {
        generation: shell.view().refresh.generation,
    });
    let view = shell.view();
    assert!(view.refresh.settled);
    assert_eq!(view.refresh.pending_machines, 0);
}

// ---------------------------------------------------------------------------------------
// Prometheus
// ---------------------------------------------------------------------------------------

#[test]
fn prometheus_unavailable_means_every_value_from_ssh() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    for m in ["luna", "sol", "terra"] {
        shell.probe(m, observed(&path('0', m), ""));
    }
    let mut prom = shell.take_prometheus();
    shell.resolve(&mut prom, Err("connection refused".to_owned()));

    let view = shell.view();
    assert_eq!(
        view.prometheus,
        PrometheusStatus::Unavailable {
            reason: "connection refused".to_owned()
        }
    );
    assert_eq!(view.report.errors.len(), 0, "not an error");
    for m in &view.report.machines {
        let h = m.health;
        assert_eq!(
            h.uptime_seconds.unwrap().source,
            HealthSource::Ssh,
            "{}",
            m.name
        );
        assert_eq!(h.uptime_seconds.unwrap().value, 100);
        assert_eq!(h.load.unwrap().source, HealthSource::Ssh);
        assert_eq!(h.root_disk.unwrap().source, HealthSource::Ssh);
        assert_eq!(h.memory, None);
    }
}

#[test]
fn a_stale_sample_makes_that_machine_alone_fall_back() {
    let mut shell = Shell::new();
    let mut config = full_config();
    config.prometheus = Some(PrometheusConfig {
        instances: BTreeMap::from([("terra".to_owned(), "terra:9100".to_owned())]),
        max_age_seconds: 60,
    });
    shell.start_with(config, THREE);
    for m in ["luna", "sol", "terra"] {
        shell.probe(m, observed(&path('0', m), ""));
    }
    let mut prom = shell.take_prometheus();
    assert_eq!(
        prom.operation.instances,
        ["luna", "sol", "terra:9100"],
        "instance labels from the config, the name otherwise"
    );
    let samples = BTreeMap::from([
        ("terra:9100".to_owned(), sample(5000, 10)),
        ("luna".to_owned(), sample(6000, 61)), // stale: older than 60 s
        ("sol".to_owned(), sample(7000, 60)),  // exactly the limit: fresh
    ]);
    shell.resolve(&mut prom, Ok(samples));

    let source = |m: &str| shell.row(m).health.uptime_seconds.unwrap();
    assert_eq!(source("terra").source, HealthSource::Prometheus);
    assert_eq!(source("terra").value, 5000);
    assert_eq!(source("sol").source, HealthSource::Prometheus);
    assert_eq!(source("luna").source, HealthSource::Ssh);
    assert_eq!(source("luna").value, 100);
    assert_eq!(shell.view().prometheus, PrometheusStatus::Available);
}

// ---------------------------------------------------------------------------------------
// The cache question
// ---------------------------------------------------------------------------------------

#[test]
fn the_cache_question_is_asked_only_for_a_pending_toplevel_with_a_cache() {
    let a = path('0', "nixos-system-a");
    let b = path('1', "nixos-system-b");
    let cases = [
        // (cache configured, current, evaluated, asked)
        (true, observed(&a, ""), evaluated("terra", &b), true),
        (false, observed(&a, ""), evaluated("terra", &b), false),
        (true, observed(&a, ""), evaluated("terra", &a), false), // in sync
        (true, observed("", ""), evaluated("terra", &b), false), // not installed
        (true, unreachable("down"), evaluated("terra", &b), false),
        (
            true,
            observed(&a, ""),
            eval_failed("terra", &["error: x"]),
            false,
        ),
    ];
    for (i, (cache, probe, outcome, asked)) in cases.into_iter().enumerate() {
        let mut shell = Shell::new();
        let config = Config {
            binary_cache: cache,
            ..bare_config()
        };
        shell.start_with(config, THREE);
        shell.probe("terra", probe);
        let mut eval = shell.take_eval();
        shell.eval_item(&mut eval, "terra", outcome);
        assert_eq!(shell.caches.len(), usize::from(asked), "case {i}");
        if asked {
            let mut req = shell.take_cache("terra");
            assert_eq!(shell.row("terra").badges, [Badge::UpdatePending]);
            shell.resolve(&mut req, NarCache::Cached);
            assert_eq!(
                shell.row("terra").badges,
                [Badge::UpdatePending, Badge::Cached]
            );
        }
        // Repeated answers do not ask again.
        shell.eval_item(&mut eval, "terra", evaluated("terra", &path('2', "c")));
        assert_eq!(shell.caches.len(), 0, "case {i}: asked twice");
    }
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

#[test]
fn an_evaluation_error_stays_on_its_row_with_its_reason() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    for m in ["luna", "sol", "terra"] {
        shell.probe(m, observed(&path('0', m), ""));
    }
    let mut eval = shell.take_eval();
    shell.eval_item(
        &mut eval,
        "luna",
        eval_failed(
            "luna",
            &[
                "error: attribute 'foo' missing",
                "at /fleet/machines/luna.nix:12:5:",
            ],
        ),
    );
    shell.eval_item(&mut eval, "sol", evaluated("sol", &path('0', "sol")));
    let luna = shell.row("luna");
    assert_eq!(luna.status, Badge::EvalError);
    assert_eq!(
        luna.reason.as_deref(),
        Some("error: attribute 'foo' missing")
    );
    assert_eq!(shell.row("sol").status, Badge::InSync);
    assert_eq!(shell.row("terra").status, Badge::Evaluating);
    assert_eq!(shell.view().report.errors.len(), 0);
}

#[test]
fn a_machine_list_failure_is_a_fleet_error_with_its_reason() {
    let mut shell = Shell::new();
    shell.send(Event::Start(full_config()));
    let mut list = shell.take_inventory();
    shell.resolve(
        &mut list,
        Err("error: flake '/fleet' does not provide clanInternals".to_owned()),
    );
    let view = shell.view();
    assert_eq!(
        view.report.errors,
        [FleetError::new(
            FleetErrorKind::InventoryFailed,
            "error: flake '/fleet' does not provide clanInternals"
        )]
    );
    assert_eq!(view.report.machines.len(), 0);
    assert!(view.refresh.settled, "nothing else will come");
    assert_eq!(shell.waiting(), 0, "nothing else was asked");

    // A refresh clears it.
    shell.send(Event::Refresh);
    assert_eq!(shell.view().report.errors.len(), 0);
}

// ---------------------------------------------------------------------------------------
// Address resolution
// ---------------------------------------------------------------------------------------

#[test]
fn hostname_targets_are_resolved_and_collisions_reported() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{
          "cerberus": {"deploy": {"targetHost": "root@192.168.1.2"}},
          "mars":     {"deploy": {"targetHost": "root@mars.ugalabugala.org"}},
          "luna":     {}
        }"#,
    );
    assert_eq!(shell.resolves.len(), 1);
    let mut req = shell.resolves.remove(0);
    assert_eq!(
        req.operation.hosts,
        BTreeMap::from([
            ("luna".to_owned(), "luna".to_owned()),
            ("mars".to_owned(), "mars.ugalabugala.org".to_owned()),
        ])
    );
    let wildcard: IpAddr = "192.168.1.2".parse().unwrap();
    shell.resolve(
        &mut req,
        BTreeMap::from([
            ("mars".to_owned(), vec![wildcard]),
            ("luna".to_owned(), vec![]),
            ("ghost".to_owned(), vec![wildcard]),
        ]),
    );
    let shared = shell.view().report.shared_ips;
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].machines, ["cerberus", "mars"], "and no ghost");
}

// ---------------------------------------------------------------------------------------
// View state through the core
// ---------------------------------------------------------------------------------------

fn selected(shell: &Shell) -> String {
    let view = shell.view();
    view.report.machines[view.selected.unwrap()].name.clone()
}

#[test]
fn selection_follows_the_machine_across_refreshes() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{"alfrost": {}, "cerberus": {}, "luna": {}, "mars": {}, "sol": {}}"#,
    );
    assert_eq!(selected(&shell), "alfrost", "the first row by default");
    shell.send(Event::MoveDown(2));
    assert_eq!(selected(&shell), "luna");

    // Reordered, with a new machine before it: still luna.
    shell.send(Event::Refresh);
    assert_eq!(shell.view().selected, None, "no rows while the list is out");
    let mut list = shell.take_inventory();
    let mut machines =
        inventory(r#"{"sol": {}, "luna": {}, "aaa": {}, "mars": {}, "cerberus": {}}"#);
    machines.reverse();
    shell.resolve(&mut list, Ok(machines));
    assert_eq!(selected(&shell), "luna");
    assert_eq!(shell.view().selected, Some(2));

    // luna removed: the row that took its place.
    shell.send(Event::Refresh);
    let mut list = shell.take_inventory();
    shell.resolve(
        &mut list,
        Ok(inventory(r#"{"sol": {}, "aaa": {}, "mars": {}}"#)),
    );
    assert_eq!(selected(&shell), "mars");

    // Counts clamp at both ends.
    shell.send(Event::MoveDown(99));
    assert_eq!(selected(&shell), "sol");
    shell.send(Event::MoveUp(99));
    assert_eq!(selected(&shell), "aaa");
    shell.send(Event::HalfPageDown(4));
    assert_eq!(selected(&shell), "sol");
    shell.send(Event::HalfPageUp(1));
    assert_eq!(
        selected(&shell),
        "mars",
        "half of a one-row page is still one row"
    );
    shell.send(Event::MoveTop);
    assert_eq!(selected(&shell), "aaa");
    shell.send(Event::MoveBottom);
    assert_eq!(selected(&shell), "sol");
}

#[test]
fn search_next_and_previous_wrap() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{"alfrost": {}, "cerberus": {}, "luna": {}, "mars": {}, "terra": {}}"#,
    );
    shell.send(Event::MoveDown(2)); // luna
    shell.send(Event::Search("R".to_owned()));
    let view = shell.view();
    let search = view.search.unwrap();
    assert_eq!(search.matches, [0, 1, 3, 4]);
    assert_eq!(selected(&shell), "mars");
    shell.send(Event::SearchNext);
    assert_eq!(selected(&shell), "terra");
    shell.send(Event::SearchNext);
    assert_eq!(selected(&shell), "alfrost");
    shell.send(Event::SearchPrevious);
    assert_eq!(selected(&shell), "terra");
    // A query whose only match is above the cursor wraps there.
    shell.send(Event::Search("cer".to_owned()));
    assert_eq!(selected(&shell), "cerberus");
    shell.send(Event::Search(String::new()));
    assert_eq!(shell.view().search, None);
}

#[test]
fn help_and_quit() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    shell.send(Event::ToggleHelp);
    assert!(shell.view().help_open);
    shell.send(Event::ToggleHelp);
    assert!(!shell.view().help_open);
    shell.send(Event::ToggleHelp);
    shell.send(Event::CloseHelp);
    assert!(!shell.view().help_open);

    let outstanding: BTreeSet<RequestId> = shell
        .probes
        .iter()
        .map(|r| r.operation.id)
        .chain(shell.evals.iter().map(|r| r.operation.id))
        .collect();
    shell.send(Event::Quit);
    assert!(shell.view().quit);
    assert_eq!(
        shell.cancels.iter().copied().collect::<BTreeSet<_>>(),
        outstanding
    );
}

// ---------------------------------------------------------------------------------------
// Review round 1: reproductions
// ---------------------------------------------------------------------------------------

/// Both reviews, finding 1: a failed list must not serialise as a settled, empty fleet.
#[test]
fn a_failed_list_does_not_serialise_as_an_empty_fleet() {
    let mut failed = Shell::new();
    failed.send(Event::Start(bare_config()));
    let mut list = failed.take_inventory();
    failed.resolve(
        &mut list,
        Err("error: flake does not provide clanInternals".to_owned()),
    );
    let mut empty = Shell::new();
    empty.start_with(bare_config(), "{}");

    assert!(failed.view().refresh.settled, "headless stops waiting");
    let f = serde_json::to_value(failed.view().report).unwrap();
    let e = serde_json::to_value(empty.view().report).unwrap();
    assert_ne!(f, e);
    assert_eq!(f["settled"], false);
    assert_eq!(
        f["errors"],
        serde_json::json!([{
            "kind": "inventory_failed",
            "reason": "error: flake does not provide clanInternals"
        }])
    );
    assert_eq!(e["settled"], true);
    assert_eq!(e["errors"], serde_json::json!([]));
}

/// Sol's variant of finding 1: an evaluator failure after every row already has a result.
#[test]
fn an_evaluator_failure_reaches_the_report() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{"terra": {"deploy": {"targetHost": "192.168.1.10"}}}"#,
    );
    let sys = path('0', "nixos-system-terra");
    shell.probe("terra", observed(&sys, ""));
    let mut eval = shell.take_eval();
    shell.eval_item(&mut eval, "terra", evaluated("terra", &sys));
    shell.resolve(
        &mut eval,
        EvalAnswer::Finished {
            error: Some("error: evaluator crashed".to_owned()),
        },
    );
    let view = shell.view();
    assert!(view.refresh.settled);
    assert_eq!(
        shell.row("terra").status,
        Badge::InSync,
        "answered rows stand"
    );
    let report = serde_json::to_value(&view.report).unwrap();
    assert_eq!(report["settled"], false);
    assert_eq!(report["errors"][0]["kind"], "evaluation_failed");
    assert_eq!(report["errors"][0]["reason"], "error: evaluator crashed");
}

/// The deadline before the list: the report says so, with a reason.
#[test]
fn a_list_timeout_reaches_the_report() {
    let mut shell = Shell::new();
    shell.send(Event::Start(bare_config()));
    shell.send(Event::DeadlinePassed {
        generation: shell.view().refresh.generation,
    });
    let report = serde_json::to_value(shell.view().report).unwrap();
    assert_eq!(report["settled"], false);
    assert_eq!(report["errors"][0]["kind"], "inventory_timed_out");
    assert_ne!(report["errors"][0]["reason"], "");
}

const RESOLVE_FLEET: &str = r#"{
  "cerberus": {"deploy": {"targetHost": "root@192.168.1.2"}},
  "mars":     {"deploy": {"targetHost": "root@mars.example.org"}},
  "broken":   {"deploy": {"targetHost": "-oProxyCommand=sh"}}
}"#;

/// Opus finding 2a: an answer for a refused machine, never asked about, must not collide.
#[test]
fn an_address_for_a_machine_not_asked_about_is_ignored() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), RESOLVE_FLEET);
    let before = shell.row("cerberus");
    let mut req = shell.resolves.remove(0);
    assert_eq!(req.operation.hosts.keys().collect::<Vec<_>>(), ["mars"]);
    shell.resolve(
        &mut req,
        BTreeMap::from([
            ("mars".to_owned(), vec!["10.9.9.9".parse().unwrap()]),
            ("broken".to_owned(), vec!["192.168.1.2".parse().unwrap()]),
        ]),
    );
    assert_eq!(shell.row("cerberus"), before);
    assert_eq!(shell.view().report.shared_ips.len(), 0);
}

/// Opus finding 2b: an answer must not overwrite a literal IP's known address.
#[test]
fn an_answer_cannot_overwrite_a_literal_address() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{
          "cerberus": {"deploy": {"targetHost": "root@192.168.1.2"}},
          "mars":     {"deploy": {"targetHost": "root@mars.example.org"}}
        }"#,
    );
    let mut req = shell.resolves.remove(0);
    shell.resolve(
        &mut req,
        BTreeMap::from([
            ("mars".to_owned(), vec!["192.168.1.2".parse().unwrap()]),
            ("cerberus".to_owned(), vec!["10.9.9.9".parse().unwrap()]),
        ]),
    );
    let shared = shell.view().report.shared_ips;
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].machines, ["cerberus", "mars"]);
}

/// Sol finding 2: an empty unrequested entry must not erase the real collision either.
#[test]
fn an_unrequested_empty_answer_does_not_hide_a_shared_ip() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{
          "terra": {"deploy": {"targetHost": "192.168.1.10"}},
          "luna":  {"deploy": {"targetHost": "luna.example"}}
        }"#,
    );
    let mut dns = shell.resolves.remove(0);
    assert_eq!(
        dns.operation
            .hosts
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["luna"]
    );
    shell.resolve(
        &mut dns,
        BTreeMap::from([
            ("luna".to_owned(), vec!["192.168.1.10".parse().unwrap()]),
            ("terra".to_owned(), vec![]),
        ]),
    );
    assert_eq!(shell.view().report.shared_ips.len(), 1);
}

/// Opus finding 4: with Prometheus configured, the status never reads `NotConfigured`.
#[test]
fn prometheus_is_not_reported_unconfigured_when_it_is_configured() {
    let mut failed = Shell::new();
    failed.send(Event::Start(full_config()));
    assert_eq!(failed.view().prometheus, PrometheusStatus::Pending);
    let mut list = failed.take_inventory();
    failed.resolve(&mut list, Err("boom".to_owned()));
    assert_eq!(failed.view().prometheus, PrometheusStatus::NotAsked);

    let mut empty = Shell::new();
    empty.start_with(full_config(), "{}");
    assert_eq!(empty.view().prometheus, PrometheusStatus::NotAsked);

    let mut bare = Shell::new();
    bare.start_with(bare_config(), "{}");
    assert_eq!(bare.view().prometheus, PrometheusStatus::NotConfigured);
}

// ---------------------------------------------------------------------------------------
// Review round 1: guards no test protected (Opus mutations)
// ---------------------------------------------------------------------------------------

/// Answers every request of the current generation: every probe in sync, every eval, the
/// evaluator finished, Prometheus with `samples`, and any address and cache questions.
fn complete(shell: &mut Shell, samples: BTreeMap<String, auspex_domain::observe::PromSample>) {
    let mut eval = shell.take_eval();
    let names: Vec<String> = eval
        .operation
        .machines
        .iter()
        .map(|m| m.name.clone())
        .collect();
    for name in &names {
        let sys = path('0', &format!("nixos-system-{name}"));
        if shell.probes.iter().any(|r| r.operation.machine == *name) {
            shell.probe(name, observed(&sys, ""));
        }
        shell.eval_item(&mut eval, name, evaluated(name, &sys));
    }
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });
    if !shell.prometheus.is_empty() {
        let mut prom = shell.take_prometheus();
        shell.resolve(&mut prom, Ok(samples));
    }
    while let Some(mut req) = shell.resolves.pop() {
        shell.resolve(&mut req, BTreeMap::new());
    }
}

/// Refreshes and answers the new list with `json`.
fn refresh_with(shell: &mut Shell, json: &str) {
    shell.send(Event::Refresh);
    shell.drop_cancelled();
    let mut list = shell.take_inventory();
    shell.resolve(&mut list, Ok(inventory(json)));
}

fn deadline(shell: &mut Shell) {
    let generation = shell.view().refresh.generation;
    shell.send(Event::DeadlinePassed { generation });
}

/// M4: a second deadline for the same refresh changes nothing (it would otherwise find
/// nothing outstanding and clear `timed_out`).
#[test]
fn a_second_deadline_changes_nothing() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    deadline(&mut shell);
    let before = shell.view();
    assert!(before.refresh.timed_out);
    assert!(!before.report.settled);
    let cancels = shell.cancels.len();
    deadline(&mut shell);
    assert_eq!(shell.view(), before);
    assert_eq!(shell.cancels.len(), cancels);
}

/// M25: a refresh forgets the last generation's Prometheus samples.
#[test]
fn a_refresh_forgets_the_old_samples() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    complete(
        &mut shell,
        BTreeMap::from([("terra".to_owned(), sample(9_999, 1))]),
    );
    assert_eq!(
        shell.row("terra").health.uptime_seconds.unwrap().source,
        HealthSource::Prometheus
    );
    refresh_with(&mut shell, THREE);
    shell.probe("terra", observed(&path('0', "x"), ""));
    let uptime = shell.row("terra").health.uptime_seconds.unwrap();
    assert_eq!(
        uptime.source,
        HealthSource::Ssh,
        "not the last refresh's sample"
    );
    assert_eq!(uptime.value, 100);
}

/// M26: a refresh forgets the last generation's addresses, so a machine that has gone
/// cannot collide with one that took its address.
#[test]
fn a_refresh_forgets_the_old_addresses() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{"old": {"deploy": {"targetHost": "root@192.168.1.50"}}}"#,
    );
    refresh_with(
        &mut shell,
        r#"{"new": {"deploy": {"targetHost": "root@192.168.1.50"}}}"#,
    );
    assert_eq!(
        shell.view().report.shared_ips.len(),
        0,
        "no ghost collision"
    );
}

/// M27 and M28: a deadline in one refresh does not settle, or mark as timed out, the next.
#[test]
fn a_deadline_does_not_carry_into_the_next_refresh() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    deadline(&mut shell);
    assert!(shell.view().refresh.timed_out);

    refresh_with(&mut shell, THREE);
    let view = shell.view();
    assert!(
        !view.refresh.settled,
        "the new refresh has answers to wait for"
    );
    assert!(!view.refresh.timed_out);

    complete(&mut shell, BTreeMap::new());
    let view = shell.view();
    assert!(view.refresh.settled);
    assert!(!view.refresh.timed_out);
    assert!(view.report.settled, "a clean second run reports settled");
}

/// M33: Prometheus still out at the deadline is reported unavailable.
#[test]
fn the_deadline_marks_prometheus_unavailable() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    let prom = shell.prometheus[0].operation.id;
    deadline(&mut shell);
    assert!(shell.cancels.contains(&prom));
    assert_eq!(
        shell.view().prometheus,
        PrometheusStatus::Unavailable {
            reason: auspex_core::PROMETHEUS_TIMED_OUT.to_owned()
        }
    );
}

/// M36: a deadline before `Start` (generation 0) is ignored.
#[test]
fn a_deadline_before_start_is_ignored() {
    let mut shell = Shell::new();
    let before = shell.view();
    assert_eq!(before.refresh.generation.0, 0);
    deadline(&mut shell);
    assert_eq!(shell.view(), before);
    assert!(!shell.view().refresh.settled);
}

/// M41 and M45: a deadline after the fleet settled by itself changes nothing, a refused
/// target included (it was never probed, and must not be marked as timed out).
#[test]
fn a_deadline_after_settling_changes_nothing() {
    let mut shell = Shell::new();
    shell.start_with(
        bare_config(),
        r#"{
          "terra":  {"deploy": {"targetHost": "root@192.168.1.10"}},
          "broken": {"deploy": {"targetHost": "-oProxyCommand=sh"}}
        }"#,
    );
    complete(&mut shell, BTreeMap::new());
    let before = shell.view();
    assert!(before.refresh.settled);
    assert!(before.report.settled);
    deadline(&mut shell);
    let after = shell.view();
    assert!(!after.refresh.timed_out);
    assert_eq!(after.report, before.report);
}

/// M8: after a quit cancelled everything, the deadline still marks the run as timed out,
/// because the rows it marks were cut short.
#[test]
fn a_deadline_after_a_quit_is_a_timeout() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    shell.send(Event::Quit);
    deadline(&mut shell);
    let view = shell.view();
    assert!(view.refresh.timed_out);
    assert!(!view.report.settled);
}

/// M15: a machine the adapter lists twice is one row, probed once.
#[test]
fn a_machine_listed_twice_is_one_row() {
    let mut shell = Shell::new();
    shell.send(Event::Start(bare_config()));
    let mut list = shell.take_inventory();
    let mut machines = inventory(THREE);
    machines.push(machines[0].clone());
    shell.resolve(&mut list, Ok(machines));
    assert_eq!(shell.view().report.machines.len(), 3);
    assert_eq!(shell.probes.len(), 3);
}

/// M37: a refresh resets the Prometheus status of the last run.
#[test]
fn a_refresh_resets_the_prometheus_status() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    let mut prom = shell.take_prometheus();
    shell.resolve(&mut prom, Err("down".to_owned()));
    shell.send(Event::Refresh);
    assert_eq!(shell.view().prometheus, PrometheusStatus::Pending);
}

/// M42: `/` from a row that already matches stays on it, as vim's incremental search does.
#[test]
fn a_search_that_matches_the_selection_stays_there() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), r#"{"luna": {}, "mars": {}, "terra": {}}"#);
    shell.send(Event::MoveDown(1)); // mars
    // "a" matches luna, mars and terra: the one at the cursor wins over the next.
    shell.send(Event::Search("a".to_owned()));
    assert_eq!(selected(&shell), "mars");
}

// ---------------------------------------------------------------------------------------
// Review round 1b: the Sol review's surviving mutations
// ---------------------------------------------------------------------------------------

/// M02: an answer of the wrong kind for a live request id changes nothing, and the request
/// it named is still answerable afterwards.
#[test]
fn a_wrong_kind_answer_for_a_live_id_changes_nothing() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    let generation = shell.view().refresh.generation;
    let probe = shell.probes[0].operation.id;
    let prom = shell.prometheus[0].operation.id;
    let before = shell.view();
    shell.send(Event::CacheAnswered {
        generation,
        id: probe,
        cache: NarCache::Cached,
    });
    shell.send(Event::InventoryAnswered {
        generation,
        id: prom,
        answer: Ok(inventory("{}")),
    });
    shell.send(Event::PrometheusAnswered {
        generation,
        id: probe,
        answer: Err("wrong kind".to_owned()),
    });
    assert_eq!(shell.view(), before);

    // The named requests are still outstanding: their real answers land.
    let name = shell.probes[0].operation.machine.clone();
    shell.probe(&name, unreachable("down"));
    assert_eq!(shell.row(&name).status, Badge::Unreachable);
    let mut p = shell.take_prometheus();
    shell.resolve(&mut p, Err("down".to_owned()));
    assert_eq!(
        shell.view().prometheus,
        PrometheusStatus::Unavailable {
            reason: "down".to_owned()
        }
    );
}

/// M03: once every probe and evaluation is in, each of Prometheus, the cache question and
/// address resolution still holds the fleet unsettled until it answers.
#[test]
fn secondary_requests_hold_the_fleet_unsettled() {
    let fleet = r#"{
      "terra": {"deploy": {"targetHost": "root@192.168.1.10"}},
      "luna":  {"deploy": {"targetHost": "root@luna.example"}}
    }"#;
    for held in ["prometheus", "cache", "resolve"] {
        let mut shell = Shell::new();
        shell.start_with(full_config(), fleet);
        let mut eval = shell.take_eval();
        for m in ["luna", "terra"] {
            shell.probe(m, observed(&path('0', m), ""));
            // terra's toplevel differs, so its cache question goes out.
            let out = if m == "terra" {
                path('1', m)
            } else {
                path('0', m)
            };
            shell.eval_item(&mut eval, m, evaluated(m, &out));
        }
        shell.resolve(&mut eval, EvalAnswer::Finished { error: None });
        let mut prom = shell.take_prometheus();
        let mut cache = shell.take_cache("terra");
        let mut dns = shell.resolves.remove(0);
        if held != "prometheus" {
            shell.resolve(&mut prom, Ok(BTreeMap::new()));
        }
        if held != "cache" {
            shell.resolve(&mut cache, NarCache::NotCached);
        }
        if held != "resolve" {
            shell.resolve(&mut dns, BTreeMap::new());
        }
        assert_eq!(shell.view().refresh.pending_machines, 0, "{held}");
        assert!(!shell.view().refresh.settled, "{held} still out");
        match held {
            "prometheus" => shell.resolve(&mut prom, Ok(BTreeMap::new())),
            "cache" => shell.resolve(&mut cache, NarCache::NotCached),
            _ => shell.resolve(&mut dns, BTreeMap::new()),
        }
        assert!(shell.view().refresh.settled, "{held} answered");
    }
}

/// M04: a cache answer updates its own machine's row and no other.
#[test]
fn a_cache_answer_updates_only_its_own_row() {
    let mut shell = Shell::new();
    shell.start_with(bare_config_with_cache(), THREE);
    let mut eval = shell.take_eval();
    for m in ["luna", "terra"] {
        shell.probe(m, observed(&path('0', m), ""));
        shell.eval_item(&mut eval, m, evaluated(m, &path('1', m)));
    }
    assert_eq!(shell.caches.len(), 2);
    let mut terra = shell.take_cache("terra");
    shell.resolve(&mut terra, NarCache::Cached);
    assert_eq!(
        shell.row("terra").badges,
        [Badge::UpdatePending, Badge::Cached]
    );
    assert_eq!(shell.row("luna").badges, [Badge::UpdatePending]);
}

fn bare_config_with_cache() -> Config {
    Config {
        binary_cache: true,
        ..bare_config()
    }
}

/// M05: a count of zero moves nothing, either way.
#[test]
fn a_zero_count_moves_nothing() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    shell.send(Event::MoveDown(1));
    let at = selected(&shell);
    shell.send(Event::MoveDown(0));
    assert_eq!(selected(&shell), at);
    shell.send(Event::MoveUp(0));
    assert_eq!(selected(&shell), at);
}

/// M06: `CloseHelp` closes the overlay, and on a closed one does nothing.
#[test]
fn close_help_never_opens_it() {
    let mut shell = Shell::new();
    shell.start_with(bare_config(), THREE);
    shell.send(Event::CloseHelp);
    assert!(!shell.view().help_open);
    shell.send(Event::ToggleHelp);
    shell.send(Event::CloseHelp);
    shell.send(Event::CloseHelp);
    assert!(!shell.view().help_open);
}
