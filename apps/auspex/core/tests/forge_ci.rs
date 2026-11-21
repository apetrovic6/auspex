//! The CI badge's sequencing: when the core reads `HEAD`, when it asks the forge, what each
//! answer makes of the badge, and that nothing about it delays a row (or a row, it).
//!
//! Driven the way a shell drives the core: requests kept, resolved by hand. The names are
//! `forge.example.org` and `owner/fleet`, as everywhere in this change.

mod support;

use auspex_core::{CI_CHECKING, CI_TIMED_OUT, Config, Event, ForgeConfig, Generation, Phase};
use auspex_domain::ci::{CiState, CiStatus, HeadFacts, Remote};
use auspex_domain::preflight::Upstream;
use support::*;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn forge_config(repo: Option<&str>) -> Config {
    Config {
        forge: Some(ForgeConfig {
            host: "forge.example.org".to_owned(),
            repo: repo.map(str::to_owned),
        }),
        ..bare_config()
    }
}

fn tracking(ahead: u64) -> Upstream {
    Upstream::Tracking {
        name: "origin/main".to_owned(),
        ahead,
        behind: 0,
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "fills the `origin` field of `HeadFacts`"
)]
fn origin(host: &str) -> Option<Result<Remote, String>> {
    Some(Ok(Remote {
        host: host.to_owned(),
        repo: "owner/fleet".to_owned(),
    }))
}

fn head(commit: &str, upstream: Upstream, origin: Option<Result<Remote, String>>) -> HeadFacts {
    HeadFacts {
        commit: commit.to_owned(),
        upstream,
        origin,
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "fills the answer of `ForgeStatusRequest`"
)]
fn status(state: CiState) -> Result<CiStatus, String> {
    Ok(CiStatus {
        state,
        url: Some("https://ci.example.org/owner/fleet/actions/runs/4".to_owned()),
    })
}

fn ci(shell: &Shell) -> auspex_domain::ci::FleetCi {
    shell.view().report.fleet_ci
}

/// Answers every probe and evaluation so all three rows settle.
fn settle_rows(shell: &mut Shell) {
    let mut eval = shell.take_eval();
    for machine in ["terra", "luna", "sol"] {
        let sys = path('0', &format!("nixos-system-{machine}"));
        shell.probe(machine, observed(&sys, ""));
        shell.eval_item(&mut eval, machine, evaluated(machine, &sys));
    }
    shell.resolve(
        &mut eval,
        auspex_core::effects::EvalAnswer::Finished { error: None },
    );
}

// ---------------------------------------------------------------------------------------
// Off
// ---------------------------------------------------------------------------------------

#[test]
fn without_forge_nothing_is_read_or_asked() {
    let mut shell = Shell::new();
    shell.start_with(full_config(), THREE);
    assert!(shell.forge_heads.is_empty() && shell.forge_statuses.is_empty());
    let view = shell.view();
    assert!(!view.refresh.ci_checking);
    assert_eq!(view.report.fleet_ci.state, CiState::Off);
    assert_eq!(view.report.fleet_ci.commit, None);
    // It does not hold the scan unsettled either.
    shell.send(Event::DeadlinePassed {
        generation: view.refresh.generation,
    });
    assert!(shell.view().refresh.settled);
    assert_eq!(shell.view().report.fleet_ci.state, CiState::Off);
}

// ---------------------------------------------------------------------------------------
// Asking
// ---------------------------------------------------------------------------------------

#[test]
fn head_is_requested_with_the_list_and_not_after_it() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    // Beside the machine list, before it has answered.
    assert_eq!(shell.inventory.len(), 1);
    assert_eq!(shell.forge_heads.len(), 1);
    let req = &shell.forge_heads[0].operation;
    assert_eq!(req.flake, "/fleet");
    assert!(
        req.want_origin,
        "forge.repo is not set, so the remote is read"
    );
    assert!(
        shell.forge_statuses.is_empty(),
        "the forge is asked only after HEAD is read"
    );
    let view = shell.view();
    assert!(view.refresh.ci_checking);
    assert_eq!(view.report.fleet_ci.state, CiState::Unknown);
    assert_eq!(view.report.fleet_ci.reason.as_deref(), Some(CI_CHECKING));
    assert!(!view.report.settled && !view.refresh.settled);
}

#[test]
fn the_remote_is_not_read_when_forge_repo_is_set() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(Some("owner/other"))));
    assert!(!shell.forge_heads[0].operation.want_origin);
    let mut h = shell.take_forge_head();
    shell.resolve(&mut h, Ok(head(SHA, tracking(0), None)));
    let ask = shell.take_forge_status();
    assert_eq!(ask.operation.repo, "owner/other");
    assert_eq!(ask.operation.commit, SHA);
}

#[test]
fn a_pushed_head_on_the_forge_is_asked_about_once() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(0), origin("forge.example.org"))),
    );
    // The commit is already on the badge while the forge is being asked.
    let view = shell.view();
    assert!(view.refresh.ci_checking);
    assert_eq!(view.report.fleet_ci.commit.as_deref(), Some(SHA));
    let mut ask = shell.take_forge_status();
    assert_eq!(ask.operation.repo, "owner/fleet");
    assert_eq!(ask.operation.commit, SHA);
    shell.resolve(&mut ask, status(CiState::Success));
    assert!(shell.forge_statuses.is_empty(), "one request per refresh");
    let view = shell.view();
    assert!(!view.refresh.ci_checking);
    assert_eq!(view.report.fleet_ci.state, CiState::Success);
    assert_eq!(view.report.fleet_ci.commit.as_deref(), Some(SHA));
    assert!(view.report.fleet_ci.url.is_some());
}

#[test]
fn each_state_reaches_the_report() {
    for state in [
        CiState::Success,
        CiState::Failure,
        CiState::Pending,
        CiState::None,
    ] {
        let mut shell = Shell::new();
        shell.send(Event::Start(forge_config(None)));
        let mut h = shell.take_forge_head();
        shell.resolve(
            &mut h,
            Ok(head(SHA, tracking(0), origin("forge.example.org"))),
        );
        let mut ask = shell.take_forge_status();
        shell.resolve(&mut ask, Ok(CiStatus { state, url: None }));
        assert_eq!(ci(&shell).state, state);
    }
}

#[test]
fn a_head_ahead_of_its_upstream_is_not_pushed_and_the_forge_is_not_asked() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(3), origin("forge.example.org"))),
    );
    assert!(shell.forge_statuses.is_empty());
    let badge = ci(&shell);
    assert_eq!(badge.state, CiState::NotPushed);
    assert_eq!(badge.commit.as_deref(), Some(SHA));
    assert!(!shell.view().refresh.ci_checking);
}

#[test]
fn a_remote_on_another_host_is_unknown_and_says_what_to_set() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(0), origin("mirror.example.net"))),
    );
    assert!(
        shell.forge_statuses.is_empty(),
        "nothing is sent to a host that was guessed"
    );
    let badge = ci(&shell);
    assert_eq!(badge.state, CiState::Unknown);
    assert!(badge.wants_forge_repo(), "{badge:?}");
}

#[test]
fn a_head_that_could_not_be_read_is_unknown_with_the_reason() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(&mut h, Err("git rev-parse failed".to_owned()));
    assert!(shell.forge_statuses.is_empty());
    let badge = ci(&shell);
    assert_eq!(badge.state, CiState::Unknown);
    assert_eq!(badge.commit, None);
    assert_eq!(badge.reason.as_deref(), Some("git rev-parse failed"));
}

#[test]
fn a_forge_that_failed_is_unknown_with_the_commit_and_the_reason() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(0), origin("forge.example.org"))),
    );
    let mut ask = shell.take_forge_status();
    shell.resolve(&mut ask, Err("forge: no answer within 10000 ms".to_owned()));
    let badge = ci(&shell);
    assert_eq!(badge.state, CiState::Unknown);
    assert_eq!(badge.commit.as_deref(), Some(SHA));
    assert_eq!(
        badge.reason.as_deref(),
        Some("forge: no answer within 10000 ms")
    );
}

// ---------------------------------------------------------------------------------------
// Rows and the badge never wait for each other
// ---------------------------------------------------------------------------------------

#[test]
fn a_badge_still_out_delays_no_row() {
    let mut shell = Shell::new();
    shell.start_with(forge_config(None), THREE);
    settle_rows(&mut shell);
    // Every row is final while HEAD has not been read.
    for m in ["terra", "luna", "sol"] {
        assert!(shell.row(m).settled, "{m}");
    }
    let view = shell.view();
    assert!(view.refresh.ci_checking);
    // Only the badge holds the refresh open; once it answers, the refresh is over.
    assert!(!view.refresh.settled);
    // And the report does not claim to be final: `fleet_ci` is part of it.
    assert!(!view.report.settled);
    let mut h = shell.take_forge_head();
    shell.resolve(&mut h, Ok(head(SHA, tracking(1), None)));
    let view = shell.view();
    assert!(view.refresh.settled && view.report.settled);
    assert_eq!(view.refresh.phase, Phase::Settled);
}

#[test]
fn a_row_still_out_delays_not_the_badge() {
    let mut shell = Shell::new();
    shell.start_with(forge_config(None), THREE);
    // The badge is complete before a single probe has answered.
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(0), origin("forge.example.org"))),
    );
    let mut ask = shell.take_forge_status();
    shell.resolve(&mut ask, status(CiState::Failure));
    assert_eq!(ci(&shell).state, CiState::Failure);
    assert_eq!(shell.probes.len(), 3, "the probes are still out");
    assert!(!shell.view().refresh.settled);
    // And the rows then settle as usual.
    settle_rows(&mut shell);
    assert!(shell.view().refresh.settled);
    assert_eq!(ci(&shell).state, CiState::Failure);
}

#[test]
fn a_failed_machine_list_still_waits_for_the_badge() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut list = shell.take_inventory();
    shell.resolve(&mut list, Err("nix eval failed".to_owned()));
    // `--json` prints `fleet_ci` with its answer in it, list or no list.
    assert!(!shell.view().refresh.settled);
    let mut h = shell.take_forge_head();
    shell.resolve(&mut h, Ok(head(SHA, tracking(2), None)));
    let view = shell.view();
    assert!(view.refresh.settled);
    assert_eq!(view.report.fleet_ci.state, CiState::NotPushed);
    assert_eq!(view.report.errors.len(), 1);
}

// ---------------------------------------------------------------------------------------
// Refreshes
// ---------------------------------------------------------------------------------------

#[test]
fn the_badge_is_asked_again_with_each_refresh_and_follows_it() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(0), origin("forge.example.org"))),
    );
    let mut ask = shell.take_forge_status();
    shell.resolve(&mut ask, status(CiState::Success));
    assert_eq!(ci(&shell).state, CiState::Success);

    // A refresh forgets the old answer at once, and asks again.
    shell.send(Event::Refresh);
    assert_eq!(shell.forge_heads.len(), 1);
    let view = shell.view();
    assert!(view.refresh.ci_checking);
    assert_eq!(view.report.fleet_ci.reason.as_deref(), Some(CI_CHECKING));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA2, tracking(0), origin("forge.example.org"))),
    );
    let mut ask = shell.take_forge_status();
    assert_eq!(ask.operation.commit, SHA2, "the new HEAD, not the old one");
    shell.resolve(&mut ask, status(CiState::Failure));
    let badge = ci(&shell);
    assert_eq!(
        (badge.state, badge.commit.as_deref()),
        (CiState::Failure, Some(SHA2))
    );
}

#[test]
fn a_refresh_cancels_the_question_in_flight_and_drops_its_late_answer() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(0), origin("forge.example.org"))),
    );
    let mut old = shell.take_forge_status();
    let old_id = old.operation.id;

    shell.send(Event::Refresh);
    assert!(shell.cancels.contains(&old_id), "{:?}", shell.cancels);
    // The old answer arrives anyway: it changes nothing.
    shell.resolve(&mut old, status(CiState::Success));
    let view = shell.view();
    assert!(view.refresh.ci_checking, "{:?}", view.report.fleet_ci);
    assert_eq!(view.report.fleet_ci.state, CiState::Unknown);
}

// ---------------------------------------------------------------------------------------
// The deadline
// ---------------------------------------------------------------------------------------

#[test]
fn the_deadline_makes_an_unread_head_unknown() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let generation = shell.view().refresh.generation;
    let id = shell.forge_heads[0].operation.id;
    shell.send(Event::DeadlinePassed { generation });
    assert!(shell.cancels.contains(&id));
    let view = shell.view();
    assert!(!view.refresh.ci_checking);
    assert_eq!(view.report.fleet_ci.state, CiState::Unknown);
    assert_eq!(view.report.fleet_ci.reason.as_deref(), Some(CI_TIMED_OUT));
    assert!(view.refresh.settled && view.refresh.timed_out);
}

#[test]
fn the_deadline_keeps_the_commit_of_a_question_that_was_not_answered() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    let mut h = shell.take_forge_head();
    shell.resolve(
        &mut h,
        Ok(head(SHA, tracking(0), origin("forge.example.org"))),
    );
    let generation = shell.view().refresh.generation;
    shell.send(Event::DeadlinePassed { generation });
    let badge = ci(&shell);
    assert_eq!(badge.state, CiState::Unknown);
    assert_eq!(badge.commit.as_deref(), Some(SHA));
    assert_eq!(badge.reason.as_deref(), Some(CI_TIMED_OUT));
}

#[test]
fn a_deadline_for_an_old_refresh_does_not_touch_the_badge() {
    let mut shell = Shell::new();
    shell.send(Event::Start(forge_config(None)));
    shell.send(Event::Refresh);
    shell.send(Event::DeadlinePassed {
        generation: Generation(1),
    });
    assert!(shell.view().refresh.ci_checking);
}
