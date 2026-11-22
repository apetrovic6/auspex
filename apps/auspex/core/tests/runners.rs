//! The runners view's sequencing: when the core asks the forges, how their answers join what the
//! probes and Prometheus said, and that nothing about it delays a row (or a row, it).
//!
//! Driven the way a shell drives the core: requests kept, resolved by hand. The names are
//! `forge.example.org`, `alpha` and `beta`, as everywhere in this change.

mod support;

use std::collections::BTreeMap;

use auspex_core::{Config, Event, Generation, Phase, RUNNERS_TIMED_OUT, RunnerForge, RunnersSetup};
use auspex_domain::observe::{Memory, PromSample};
use auspex_domain::runners::{
    CHECKING, ForgeAnswer, ForgeRunner, HostCaps, RunnerBadge, RunnerRow, RunnerStatus,
    RunnersConfig,
};
use support::*;

const TWO: &str = r#"{
  "alpha": {"deploy": {"targetHost": "root@192.168.1.10"}},
  "beta":  {"deploy": {"targetHost": "root@192.168.1.11"}}
}"#;

fn setup(forges: &[&str]) -> RunnersSetup {
    RunnersSetup {
        forges: forges
            .iter()
            .map(|f| RunnerForge {
                forge: (*f).to_owned(),
                scope: "admin".to_owned(),
                site: (*f).to_owned(),
            })
            .collect(),
        join: RunnersConfig::default(),
    }
}

fn config(forges: &[&str]) -> Config {
    Config {
        runners: Some(Box::new(setup(forges))),
        ..bare_config()
    }
}

fn runner(name: &str, status: &str) -> ForgeRunner {
    ForgeRunner::new(name, status, &["docker".to_owned()], Some("v9.0.0"))
}

const fn answer(runners: Vec<ForgeRunner>, queued: u64) -> ForgeAnswer {
    ForgeAnswer {
        runners: Ok(runners),
        queued: Ok(queued),
    }
}

fn failure(why: &str) -> ForgeAnswer {
    ForgeAnswer {
        runners: Err(why.to_owned()),
        queued: Err(why.to_owned()),
    }
}

fn rows(shell: &Shell) -> Vec<RunnerRow> {
    shell.view().report.runners
}

fn row(shell: &Shell, name: &str) -> RunnerRow {
    rows(shell)
        .into_iter()
        .find(|r| r.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no row for {name}"))
}

/// A NixOS probe that saw these runner units (`unit active sub`, joined by `;`).
fn with_units(machine: &str, units: &str) -> auspex_domain::observe::ProbeOutcome {
    observed(
        &path('0', &format!("nixos-system-{machine}")),
        &format!("runner_units={units}\n"),
    )
}

fn settle_rows(shell: &mut Shell, units: &[(&str, &str)]) {
    let mut eval = shell.take_eval();
    for (machine, u) in units {
        let sys = path('0', &format!("nixos-system-{machine}"));
        shell.probe(machine, with_units(machine, u));
        shell.eval_item(&mut eval, machine, evaluated(machine, &sys));
    }
    shell.resolve(
        &mut eval,
        auspex_core::effects::EvalAnswer::Finished { error: None },
    );
}

// ---------------------------------------------------------------------------------------
// Off, and asking
// ---------------------------------------------------------------------------------------

#[test]
fn without_runners_nothing_is_asked() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), TWO);
    assert!(shell.runner_requests.is_empty());
    let view = shell.view();
    assert!(view.report.runners.is_empty() && view.report.runner_forges.is_empty());
    // It does not hold the scan unsettled either.
    shell.send(Event::DeadlinePassed {
        generation: view.refresh.generation,
    });
    assert!(shell.view().refresh.settled);
}

#[test]
fn every_forge_is_asked_with_the_list_and_not_after_it() {
    let mut shell = Shell::new();
    shell.send(Event::Start(config(&[
        "forge.example.org",
        "other.example.org",
    ])));
    // Beside the machine list, which has not answered, and beside each other.
    assert_eq!(shell.inventory.len(), 1);
    assert_eq!(shell.runner_requests.len(), 2);
    let forges: Vec<_> = shell
        .runner_requests
        .iter()
        .map(|r| r.operation.forge)
        .collect();
    assert_eq!(forges, [0, 1]);
    // While they are out each forge has a placeholder row, which is not an error.
    let view = shell.view();
    let r = &view.report.runners;
    assert_eq!(r.len(), 2);
    assert!(
        r.iter()
            .all(|row| row.name.is_none() && row.badges.is_empty())
    );
    assert_eq!(r[0].error.as_deref(), Some(CHECKING));
    assert!(view.report.runner_forges.iter().all(|f| f.checking));
    assert!(!view.report.settled && !view.refresh.settled);
}

// ---------------------------------------------------------------------------------------
// Joining
// ---------------------------------------------------------------------------------------

#[test]
fn runners_on_two_forges_are_listed_with_online_and_busy() {
    let mut shell = Shell::new();
    shell.start_with(config(&["forge.example.org", "other.example.org"]), TWO);
    settle_rows(
        &mut shell,
        &[
            ("alpha", "gitea-runner-alpha.service active running"),
            ("beta", "gitea-runner-beta.service active running"),
        ],
    );
    let mut a = shell.take_runners(0);
    let mut b = shell.take_runners(1);
    shell.resolve(
        &mut a,
        answer(vec![runner("alpha", "idle"), runner("beta", "active")], 3),
    );
    shell.resolve(&mut b, answer(vec![runner("alpha", "offline")], 0));
    let all = rows(&shell);
    let shown: Vec<_> = all
        .iter()
        .map(|r| (r.forge.as_str(), r.name.as_deref().unwrap(), r.status))
        .collect();
    assert_eq!(
        shown,
        [
            ("forge.example.org", "alpha", RunnerStatus::Idle),
            ("forge.example.org", "beta", RunnerStatus::Active),
            ("other.example.org", "alpha", RunnerStatus::Offline),
        ]
    );
    // Joined with the machines' units.
    assert_eq!(all[0].machine.as_deref(), Some("alpha"));
    assert_eq!(
        all[0].unit.as_ref().unwrap().unit,
        "gitea-runner-alpha.service"
    );
    let facts = &shell.view().report.runner_forges;
    assert_eq!((facts[0].queued, facts[1].queued), (Some(3), Some(0)));
    assert!(shell.view().refresh.settled);
}

#[test]
fn a_stopped_unit_turns_its_row_red_within_one_refresh() {
    let mut shell = Shell::new();
    shell.start_with(config(&["forge.example.org"]), TWO);
    settle_rows(
        &mut shell,
        &[
            ("alpha", "gitea-runner-alpha.service active running"),
            ("beta", "gitea-runner-beta.service active running"),
        ],
    );
    let mut ask = shell.take_runners(0);
    shell.resolve(
        &mut ask,
        answer(vec![runner("alpha", "idle"), runner("beta", "idle")], 0),
    );
    assert!(
        rows(&shell).iter().all(|r| !r.is_red()),
        "all fine to begin with"
    );

    // The unit on alpha stops. The forge has not noticed yet: it still says idle.
    shell.send(Event::Refresh);
    let mut list = shell.take_inventory();
    shell.resolve(&mut list, Ok(inventory(TWO)));
    settle_rows(
        &mut shell,
        &[
            ("alpha", "gitea-runner-alpha.service inactive dead"),
            ("beta", "gitea-runner-beta.service active running"),
        ],
    );
    let mut ask = shell.take_runners(0);
    shell.resolve(
        &mut ask,
        answer(vec![runner("alpha", "idle"), runner("beta", "idle")], 0),
    );
    let alpha = row(&shell, "alpha");
    assert_eq!(alpha.badges, [RunnerBadge::ForgeUpUnitDown]);
    assert!(alpha.is_red());
    assert!(!row(&shell, "beta").is_red());
    assert!(shell.view().refresh.settled);
}

#[test]
fn an_unreachable_machine_has_no_unit_facts_and_no_unit_badge() {
    let mut shell = Shell::new();
    shell.start_with(config(&["forge.example.org"]), TWO);
    let mut eval = shell.take_eval();
    shell.probe("alpha", unreachable("Connection refused"));
    shell.probe(
        "beta",
        with_units("beta", "gitea-runner-beta.service active running"),
    );
    shell.eval_item(&mut eval, "alpha", evaluated("alpha", &path('0', "x")));
    shell.eval_item(
        &mut eval,
        "beta",
        evaluated("beta", &path('0', "nixos-system-beta")),
    );
    shell.resolve(
        &mut eval,
        auspex_core::effects::EvalAnswer::Finished { error: None },
    );
    let mut ask = shell.take_runners(0);
    shell.resolve(&mut ask, answer(vec![runner("alpha", "idle")], 0));
    let alpha = row(&shell, "alpha");
    assert!(alpha.unit.is_none() && alpha.badges.is_empty(), "{alpha:?}");
    assert_eq!(alpha.machine.as_deref(), Some("alpha"));
}

#[test]
fn memory_near_the_cap_comes_from_prometheus_and_the_configured_cap() {
    let gib = 1024_u64 * 1024 * 1024;
    let mut cfg = full_config();
    cfg.runners = Some(Box::new(RunnersSetup {
        forges: vec![RunnerForge {
            forge: "forge.example.org".into(),
            scope: "admin".into(),
            site: "forge.example.org".into(),
        }],
        join: RunnersConfig {
            hosts: BTreeMap::from([(
                "alpha".to_owned(),
                HostCaps {
                    cpus: Some(2),
                    memory_bytes: Some(4 * gib),
                },
            )]),
            ..RunnersConfig::default()
        },
    }));
    let mut shell = Shell::new();
    shell.start_with(cfg, TWO);
    settle_rows(&mut shell, &[("alpha", ""), ("beta", "")]);
    let mut prom = shell.take_prometheus();
    let sample = |used: u64| PromSample {
        uptime_seconds: Some(1),
        root_disk: None,
        load: Some(auspex_domain::observe::Load {
            one: 1.5,
            five: 1.0,
            fifteen: 1.0,
        }),
        memory: Some(Memory {
            total_bytes: 16 * gib,
            available_bytes: 16 * gib - used,
        }),
        age_seconds: 1,
    };
    shell.resolve(
        &mut prom,
        Ok(BTreeMap::from([
            ("alpha".to_owned(), sample(4 * gib - 100 * 1024 * 1024)),
            ("beta".to_owned(), sample(15 * gib)),
        ])),
    );
    let mut ask = shell.take_runners(0);
    shell.resolve(
        &mut ask,
        answer(vec![runner("alpha", "idle"), runner("beta", "idle")], 0),
    );
    let alpha = row(&shell, "alpha");
    assert_eq!(alpha.badges, [RunnerBadge::MemoryNearCap]);
    assert_eq!(alpha.memory.unwrap().percent, 97);
    assert_eq!(alpha.load.unwrap().cpus, 2);
    // beta has no cap in the config: however full, no figure and no badge.
    let beta = row(&shell, "beta");
    assert!(beta.memory.is_none() && beta.badges.is_empty());
}

// ---------------------------------------------------------------------------------------
// Rows and forges never wait for each other
// ---------------------------------------------------------------------------------------

#[test]
fn a_forge_still_out_delays_no_row() {
    let mut shell = Shell::new();
    shell.start_with(config(&["forge.example.org"]), TWO);
    settle_rows(&mut shell, &[("alpha", ""), ("beta", "")]);
    for m in ["alpha", "beta"] {
        assert!(shell.row(m).settled, "{m}");
    }
    let view = shell.view();
    // Only the forge holds the refresh open, and the report is not final without it.
    assert!(!view.refresh.settled && !view.report.settled);
    let mut ask = shell.take_runners(0);
    shell.resolve(&mut ask, answer(vec![], 0));
    let view = shell.view();
    assert!(view.refresh.settled && view.report.settled);
    assert_eq!(view.refresh.phase, Phase::Settled);
}

#[test]
fn a_row_still_out_delays_not_the_forge() {
    let mut shell = Shell::new();
    shell.start_with(config(&["forge.example.org"]), TWO);
    let mut ask = shell.take_runners(0);
    shell.resolve(&mut ask, answer(vec![runner("alpha", "idle")], 1));
    assert_eq!(shell.probes.len(), 2, "the probes are still out");
    assert_eq!(row(&shell, "alpha").status, RunnerStatus::Idle);
    assert!(!shell.view().refresh.settled);
    settle_rows(&mut shell, &[("alpha", ""), ("beta", "")]);
    assert!(shell.view().refresh.settled);
}

#[test]
fn a_failing_forge_shows_on_its_own_row_and_nothing_else() {
    let mut shell = Shell::new();
    shell.start_with(config(&["forge.example.org", "other.example.org"]), TWO);
    settle_rows(&mut shell, &[("alpha", ""), ("beta", "")]);
    let mut bad = shell.take_runners(1);
    shell.resolve(
        &mut bad,
        failure("forge: the forge refused the token (HTTP 403)"),
    );
    // The machine rows are as they were and the other forge is still being asked.
    assert!(shell.row("alpha").settled);
    let mut good = shell.take_runners(0);
    shell.resolve(&mut good, answer(vec![runner("alpha", "idle")], 0));
    let all = rows(&shell);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].name.as_deref(), Some("alpha"));
    assert_eq!(all[1].name, None);
    assert_eq!(all[1].forge, "other.example.org");
    assert_eq!(all[1].badges, [RunnerBadge::ForgeError]);
    assert!(all[1].error.as_deref().unwrap().contains("403"));
    let facts = &shell.view().report.runner_forges;
    assert!(facts[0].error.is_none() && facts[1].error.is_some());
    assert!(shell.view().refresh.settled);
}

#[test]
fn a_failed_machine_list_still_waits_for_the_forges() {
    let mut shell = Shell::new();
    shell.send(Event::Start(config(&["forge.example.org"])));
    let mut list = shell.take_inventory();
    shell.resolve(&mut list, Err("nix eval failed".to_owned()));
    // `--json` prints `runners` with their answer in it, list or no list.
    assert!(!shell.view().refresh.settled);
    let mut ask = shell.take_runners(0);
    shell.resolve(&mut ask, answer(vec![runner("alpha", "idle")], 0));
    let view = shell.view();
    assert!(view.refresh.settled);
    assert_eq!(view.report.runners.len(), 1);
    assert_eq!(view.report.errors.len(), 1);
}

// ---------------------------------------------------------------------------------------
// Refreshes and the deadline
// ---------------------------------------------------------------------------------------

#[test]
fn a_refresh_forgets_the_old_answer_and_drops_a_late_one() {
    let mut shell = Shell::new();
    shell.send(Event::Start(config(&["forge.example.org"])));
    let mut old = shell.take_runners(0);
    let old_id = old.operation.id;
    shell.resolve(&mut old, answer(vec![runner("alpha", "idle")], 0));
    assert_eq!(rows(&shell).len(), 1);

    shell.send(Event::Refresh);
    assert_eq!(shell.runner_requests.len(), 1, "asked again");
    // At once the old rows are gone, replaced by the forge's placeholder.
    let now = rows(&shell);
    assert_eq!((now.len(), now[0].error.as_deref()), (1, Some(CHECKING)));

    // A late answer of the old refresh changes nothing.
    let mut late = shell.take_runners(0);
    let new_id = late.operation.id;
    assert_ne!(old_id, new_id);
    shell.resolve(&mut late, answer(vec![runner("beta", "active")], 0));
    assert_eq!(row(&shell, "beta").status, RunnerStatus::Active);
}

#[test]
fn a_refresh_cancels_the_question_in_flight() {
    let mut shell = Shell::new();
    shell.send(Event::Start(config(&["forge.example.org"])));
    let id = shell.runner_requests[0].operation.id;
    shell.send(Event::Refresh);
    assert!(shell.cancels.contains(&id), "{:?}", shell.cancels);
}

#[test]
fn the_deadline_fails_a_forge_that_did_not_answer() {
    let mut shell = Shell::new();
    shell.send(Event::Start(config(&["forge.example.org"])));
    let generation = shell.view().refresh.generation;
    let id = shell.runner_requests[0].operation.id;
    shell.send(Event::DeadlinePassed { generation });
    assert!(shell.cancels.contains(&id));
    let all = rows(&shell);
    assert_eq!(all[0].error.as_deref(), Some(RUNNERS_TIMED_OUT));
    assert_eq!(all[0].badges, [RunnerBadge::ForgeError]);
    let view = shell.view();
    assert!(view.refresh.settled && view.refresh.timed_out);
}

#[test]
fn a_deadline_for_an_old_refresh_does_not_touch_the_forges() {
    let mut shell = Shell::new();
    shell.send(Event::Start(config(&["forge.example.org"])));
    shell.send(Event::Refresh);
    shell.send(Event::DeadlinePassed {
        generation: Generation(1),
    });
    assert_eq!(rows(&shell)[0].error.as_deref(), Some(CHECKING));
}
