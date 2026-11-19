//! Act mode, `u` and `b`, and quitting while an action runs, decided by the core alone: a
//! hand-driven shell, no process, no clock.
//!
//! The properties are the safety ones, each with a test that fails when its rule is broken
//! (the report lists how each was checked): observe is the default and headless cannot leave
//! it; an update is asked about by name and runs nothing until `y`; the machine is the
//! selected row's, validated against the domain's name rule and the inventory; one action at
//! a time; a build never starts for another system; quitting waits for the process.

mod support;

use auspex_core::effects::{ActionEnd, ActionKind, ActionRequest, ActionResult, EvalAnswer};
use auspex_core::{ActionStatus, Config, ConfirmKind, Event, Mode, Phase};
use auspex_domain::eval::{EvalOutcome, EvalSuccess};
use auspex_domain::fleet::{Machine, MachineClass};
use auspex_domain::observe::StorePath;
use auspex_domain::status::Badge;
use crux_core::Request;
use support::{Shell, THREE, clean_preflight, evaluated, inventory, observed, path, unreachable};

/// Act mode allowed, no Prometheus and no cache: a scan is probes and one evaluation.
fn config() -> Config {
    Config {
        flake: "/fleet".to_owned(),
        allow_act: true,
        local_system: "x86_64-linux".to_owned(),
        ..Config::default()
    }
}

fn old(m: &str) -> String {
    path('a', &format!("nixos-system-{m}-old"))
}

fn new(m: &str) -> String {
    path('b', &format!("nixos-system-{m}-new"))
}

/// luna, sol and terra, scanned to the end: each runs `old` and evaluates to `new`. luna is
/// selected.
fn settled(config: Config) -> Shell {
    let mut s = Shell::new();
    s.start_with(config, THREE);
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        s.probe(m, observed(&old(m), ""));
    }
    assert_eq!(s.view().refresh.phase, Phase::Settled);
    assert_eq!(s.row("luna").status, Badge::UpdatePending);
    s
}

fn act_on(s: &mut Shell) {
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().mode, Mode::Act);
}

/// The shell's answer to the checks before an update (the one request `u` made).
fn answer_checks(s: &mut Shell, answer: auspex_core::PreflightAnswer) {
    let mut request = s.take_preflight();
    s.resolve(&mut request, answer);
}

/// `u`, a clean answer from the checks, and `y` on the selected machine: the request the core
/// made.
fn update(s: &mut Shell) -> Request<ActionRequest> {
    s.send(Event::UpdateSelected);
    assert!(s.actions.is_empty(), "nothing runs before the answer");
    answer_checks(s, clean_preflight());
    assert!(s.actions.is_empty(), "nothing runs before the answer");
    s.send(Event::ConfirmAnswer(true));
    s.take_action()
}

fn end(result: ActionResult, code: Option<i32>) -> ActionEnd {
    ActionEnd {
        result,
        exit_code: code,
        summary: vec!["the last line".to_owned()],
        explanation: None,
    }
}

fn machine(name: &str) -> Machine {
    Machine {
        name: name.to_owned(),
        class: MachineClass::Nixos,
        tags: Vec::new(),
        target_host: Some("root@192.168.1.50".to_owned()),
        build_host: None,
    }
}

fn notice(s: &Shell) -> String {
    s.view().notice.expect("a notice")
}

// ---------------------------------------------------------------------------------------
// Act mode
// ---------------------------------------------------------------------------------------

#[test]
fn it_starts_in_observe_mode_and_observe_blocks_update() {
    let mut s = settled(config());
    assert_eq!(s.view().mode, Mode::Observe);
    s.send(Event::UpdateSelected);
    assert_eq!(notice(&s), "act mode is off (A)");
    assert!(s.view().confirm.is_none(), "no question is asked");
    assert!(s.actions.is_empty(), "nothing is requested");
    // Still nothing after an answer nobody asked for.
    s.send(Event::ConfirmAnswer(true));
    assert!(s.actions.is_empty());
}

#[test]
fn act_mode_is_turned_on_by_a_yes_and_off_at_once() {
    let mut s = settled(config());
    s.send(Event::ToggleAct);
    let confirm = s.view().confirm.expect("a question");
    assert_eq!(confirm.kind, ConfirmKind::Act);
    assert!(confirm.question.ends_with("y/N"), "{}", confirm.question);
    assert_eq!(s.view().mode, Mode::Observe, "not on until it is answered");
    // Anything but yes is no.
    s.send(Event::ConfirmAnswer(false));
    assert_eq!(s.view().mode, Mode::Observe);
    assert!(s.view().confirm.is_none());
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().mode, Mode::Act);
    s.send(Event::ToggleAct);
    assert_eq!(s.view().mode, Mode::Observe, "off needs no question");
    assert!(s.view().confirm.is_none());
}

#[test]
fn a_headless_core_cannot_enter_act_mode() {
    let mut s = settled(Config {
        allow_act: false,
        ..config()
    });
    s.send(Event::ToggleAct);
    assert!(s.view().confirm.is_none(), "nothing to answer");
    assert!(notice(&s).contains("not available"), "{}", notice(&s));
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().mode, Mode::Observe);
    s.send(Event::UpdateSelected);
    assert!(s.actions.is_empty());
}

// ---------------------------------------------------------------------------------------
// u
// ---------------------------------------------------------------------------------------

#[test]
fn update_asks_naming_the_machine_and_its_pending_system_and_waits_for_yes() {
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    // First the checks run; nothing is asked yet.
    assert_eq!(
        s.view().confirm.expect("checking").kind,
        ConfirmKind::Checking
    );
    answer_checks(&mut s, clean_preflight());
    let confirm = s.view().confirm.expect("a question");
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(
        confirm.question,
        "update luna to b1234567 nixos-system-luna-new? y/N"
    );
    assert!(s.actions.is_empty(), "no request before the answer");
    s.send(Event::ConfirmAnswer(false));
    assert!(s.actions.is_empty(), "N asks for nothing");
    assert!(s.view().action.is_none());

    let req = update(&mut s);
    assert_eq!(req.operation.kind, ActionKind::Update);
    assert_eq!(req.operation.machine, "luna");
    assert_eq!(req.operation.flake, "/fleet");
    assert_eq!(req.operation.class, MachineClass::Nixos);
    let view = s.view();
    let action = view.action.expect("running");
    assert_eq!(action.machine, "luna");
    assert_eq!(action.status, ActionStatus::Running);
}

#[test]
fn update_is_for_the_selected_machine_only() {
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::MoveDown(1));
    let req = update(&mut s);
    assert_eq!(req.operation.machine, "sol");
}

#[test]
fn update_refuses_a_name_the_domain_does_not_allow_and_asks_nothing() {
    let mut s = Shell::new();
    s.send(Event::Start(config()));
    let mut list = s.take_inventory();
    // The inventory parser would have refused these; the core does not rely on that.
    let bad = ["-h", "--flake", "a b", "x;y", "a/b"];
    let mut machines: Vec<Machine> = bad.iter().map(|n| machine(n)).collect();
    machines.push(machine(""));
    s.resolve(&mut list, Ok(machines));
    let mut eval = s.take_eval();
    for m in bad.iter().chain(&[""]) {
        s.eval_item(&mut eval, m, evaluated(m, &new("x")));
    }
    act_on(&mut s);
    let rows = s.view().report.machines.len();
    assert_eq!(rows, 6);
    s.send(Event::MoveTop);
    for i in 0..rows {
        let name = s.view().report.machines[s.view().selected.unwrap()]
            .name
            .clone();
        s.send(Event::UpdateSelected);
        assert!(
            notice(&s).contains("not a plain machine name"),
            "{name:?}: {}",
            notice(&s)
        );
        assert!(s.view().confirm.is_none(), "{name:?}: no question");
        assert!(s.actions.is_empty(), "{name:?}: no request");
        s.send(Event::MoveDown(1));
        let _ = i;
    }
    // And a build of one of them.
    s.send(Event::BuildSelected);
    assert!(s.actions.is_empty());
}

#[test]
fn update_refuses_a_machine_the_inventory_no_longer_has() {
    let gone = r#"{"sol": {"deploy": {"targetHost": "root@192.168.1.12"}}}"#;

    // The list is read again while the checks run, and luna is gone from it: the answer of the
    // checks opens no dialog.
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(gone)));
    answer_checks(&mut s, clean_preflight());
    assert!(
        s.view().confirm.is_none(),
        "no dialog for a machine that is gone"
    );
    assert!(s.actions.is_empty());
    assert!(
        notice(&s).contains("luna is not in the inventory"),
        "{}",
        notice(&s)
    );

    // Or while the dialog is open: the `y` is checked again.
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    answer_checks(&mut s, clean_preflight());
    assert!(s.view().confirm.is_some(), "the question for luna is open");
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(gone)));
    // The refresh has finished (a yes during it is refused for that reason).
    let mut eval = s.take_eval();
    s.eval_item(&mut eval, "sol", evaluated("sol", &new("sol")));
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    s.send(Event::ConfirmAnswer(true));
    assert!(
        s.actions.is_empty(),
        "luna is not in the inventory any more"
    );
    assert!(
        notice(&s).contains("luna is not in the inventory"),
        "{}",
        notice(&s)
    );
}

#[test]
fn update_with_no_rows_selects_nothing_and_asks_nothing() {
    let mut s = Shell::new();
    s.send(Event::Start(config()));
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    assert_eq!(notice(&s), "no machine selected");
    assert!(s.view().confirm.is_none());
    assert!(s.actions.is_empty());
}

#[test]
fn update_needs_an_evaluated_system_to_name() {
    let mut s = Shell::new();
    s.start_with(config(), THREE);
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    assert!(notice(&s).contains("no evaluated system"), "{}", notice(&s));
    assert!(s.view().confirm.is_none());
}

#[test]
fn turning_act_mode_off_withdraws_a_waiting_update() {
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    assert!(s.view().confirm.is_some());
    s.send(Event::ToggleAct);
    assert!(s.view().confirm.is_none());
    s.send(Event::ConfirmAnswer(true));
    assert!(s.actions.is_empty());
    assert_eq!(s.view().mode, Mode::Observe);
}

#[test]
fn the_question_shows_the_hash_prefix_so_two_systems_of_one_name_differ() {
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    answer_checks(&mut s, clean_preflight());
    let question = s.view().confirm.expect("a question").question;
    assert_eq!(
        question,
        "update luna to b1234567 nixos-system-luna-new? y/N"
    );
}

/// luna's tree is evaluated again, to `to`, and every machine probed again, while a question
/// about it is open.
fn luna_evaluates_to(s: &mut Shell, to: &str) {
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(THREE)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        let out = if m == "luna" { to.to_owned() } else { new(m) };
        s.eval_item(&mut eval, m, evaluated(m, &out));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    // The probes too: a refresh clears them, and an update is not asked about (or confirmed)
    // while its machine's probe is still out.
    for m in ["luna", "sol", "terra"] {
        s.probe(m, observed(&old(m), ""));
    }
}

#[test]
fn a_yes_to_a_system_that_is_no_longer_the_pending_one_asks_again_and_deploys_nothing() {
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    answer_checks(&mut s, clean_preflight());
    let asked = s.view().confirm.expect("a question");
    // Same name, another hash: what `clan` would deploy now is not what was shown.
    luna_evaluates_to(&mut s, &path('c', "nixos-system-luna-new"));
    s.send(Event::ConfirmAnswer(true));
    assert!(s.actions.is_empty(), "nothing runs on a stale yes");
    // The checks were about the old closure: they run again, for the new one, as a question
    // of their own.
    let again = s.view().confirm.expect("asked again");
    assert_eq!(again.kind, ConfirmKind::Checking);
    assert_ne!(
        again.serial, asked.serial,
        "a new question, to be drawn anew"
    );
    assert!(notice(&s).contains("changed"), "{}", notice(&s));
    let request = s.take_preflight();
    assert_eq!(
        request.operation.store_path.as_str(),
        path('c', "nixos-system-luna-new")
    );
    let mut request = request;
    s.resolve(&mut request, clean_preflight());
    let question = s.view().confirm.expect("the dialog for the new system");
    assert_eq!(
        question.question,
        "update luna to c1234567 nixos-system-luna-new? y/N"
    );
    assert_ne!(
        question.serial, again.serial,
        "the dialog is drawn anew too"
    );
    // A yes to the new question deploys.
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.take_action().operation.machine, "luna");
}

#[test]
fn a_yes_to_the_system_still_pending_after_a_refresh_deploys() {
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    answer_checks(&mut s, clean_preflight());
    luna_evaluates_to(&mut s, &new("luna"));
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.take_action().operation.machine, "luna");
}

#[test]
fn every_question_has_its_own_serial() {
    let mut s = settled(config());
    s.send(Event::ToggleAct);
    let first = s.view().confirm.expect("a question").serial;
    s.send(Event::ConfirmAnswer(true));
    s.send(Event::UpdateSelected);
    let second = s.view().confirm.expect("a question").serial;
    assert_ne!(first, second);
    // The dialog the checks open is a question of its own, not the checking one drawn again.
    answer_checks(&mut s, clean_preflight());
    let third = s.view().confirm.expect("a question").serial;
    assert_ne!(second, third);
}

#[test]
fn only_one_action_runs_at_a_time() {
    let mut s = settled(config());
    act_on(&mut s);
    let _running = update(&mut s);
    s.send(Event::UpdateSelected);
    assert_eq!(notice(&s), "an update of luna is already running");
    assert!(s.view().confirm.is_none());
    s.send(Event::BuildSelected);
    assert_eq!(notice(&s), "an update of luna is already running");
    assert!(s.actions.is_empty(), "no second request");
}

#[test]
fn the_action_outlives_a_refresh_and_does_not_hold_the_fleet_unsettled() {
    let mut s = settled(config());
    act_on(&mut s);
    let req = update(&mut s);
    let id = req.operation.id;
    assert!(s.view().refresh.settled, "an action is not a scan request");
    s.send(Event::Refresh);
    assert!(
        !s.cancels.contains(&id),
        "a refresh must not cancel the update: {:?}",
        s.cancels
    );
    assert_eq!(s.view().action.unwrap().status, ActionStatus::Running);
}

// ---------------------------------------------------------------------------------------
// b
// ---------------------------------------------------------------------------------------

#[test]
fn build_needs_no_act_mode_and_no_question() {
    let mut s = settled(config());
    assert_eq!(s.view().mode, Mode::Observe);
    s.send(Event::BuildSelected);
    assert!(s.view().confirm.is_none());
    let req = s.take_action();
    assert_eq!(req.operation.kind, ActionKind::Build);
    assert_eq!(req.operation.machine, "luna");
    assert_eq!(req.operation.system.as_deref(), Some("x86_64-linux"));
}

fn evaluated_for(machine: &str, system: Option<&str>) -> EvalOutcome {
    let out = new(machine);
    EvalOutcome::Success(EvalSuccess {
        attr: machine.to_owned(),
        attr_path: vec![machine.to_owned()],
        drv_path: StorePath::new(format!("{out}.drv")).unwrap(),
        out_path: StorePath::new(out).unwrap(),
        system: system.map(str::to_owned),
    })
}

fn settled_with_system(system: Option<&str>, local: &str) -> Shell {
    let mut s = Shell::new();
    s.start_with(
        Config {
            local_system: local.to_owned(),
            ..config()
        },
        THREE,
    );
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated_for(m, system));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    s
}

#[test]
fn build_refuses_a_machine_of_another_system() {
    for system in ["aarch64-darwin", "aarch64-linux", "x86_64-darwin"] {
        let mut s = settled_with_system(Some(system), "x86_64-linux");
        s.send(Event::BuildSelected);
        assert!(
            notice(&s).contains("not building another system's derivations"),
            "{system}: {}",
            notice(&s)
        );
        assert!(s.actions.is_empty(), "{system}: nothing is requested");
    }
}

#[test]
fn build_refuses_when_either_system_is_unknown() {
    let mut s = settled_with_system(None, "x86_64-linux");
    s.send(Event::BuildSelected);
    assert!(notice(&s).contains("not known"), "{}", notice(&s));
    assert!(s.actions.is_empty());

    let mut s = settled_with_system(Some("x86_64-linux"), "");
    s.send(Event::BuildSelected);
    assert!(s.actions.is_empty(), "this machine's system is unknown too");
}

// ---------------------------------------------------------------------------------------
// After an update
// ---------------------------------------------------------------------------------------

#[test]
fn after_an_update_that_machine_is_probed_and_evaluated_again_and_no_other() {
    let mut s = settled(config());
    act_on(&mut s);
    let mut req = update(&mut s);
    assert!(s.probes.is_empty() && s.evals.is_empty());
    s.resolve(&mut req, end(ActionResult::Succeeded, Some(0)));

    assert_eq!(s.probes.len(), 1, "one probe");
    assert_eq!(s.probes[0].operation.machine, "luna");
    let mut eval = s.take_eval();
    let machines: Vec<&str> = eval
        .operation
        .machines
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(machines, ["luna"], "one evaluation, of that machine only");
    // The row reads as being checked again; the others are untouched.
    assert_eq!(s.row("luna").status, Badge::Checking);
    assert_eq!(s.row("sol").status, Badge::UpdatePending);
    assert!(matches!(
        s.view().action.unwrap().status,
        ActionStatus::Ended(ActionEnd {
            result: ActionResult::Succeeded,
            ..
        })
    ));

    // The deploy took: it now runs what the evaluation says.
    s.probe("luna", observed(&new("luna"), ""));
    s.eval_item(&mut eval, "luna", evaluated("luna", &new("luna")));
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    assert_eq!(s.row("luna").status, Badge::InSync);
    assert_eq!(s.row("sol").status, Badge::UpdatePending);
    assert!(s.view().refresh.settled);
}

#[test]
fn a_failed_update_refreshes_the_machine_too() {
    let mut s = settled(config());
    act_on(&mut s);
    let mut req = update(&mut s);
    s.resolve(&mut req, end(ActionResult::Failed, Some(1)));
    assert_eq!(s.probes.len(), 1, "it may have switched before it failed");
}

#[test]
fn the_evaluation_is_not_asked_twice_while_the_fleets_is_still_out() {
    // The update runs while the list is read again (an update is refused once the evaluation is
    // out, so the refresh starts after it was confirmed: the action outlives it).
    let mut s = settled(config());
    act_on(&mut s);
    let mut req = update(&mut s);
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(THREE)));
    let mut eval = s.take_eval();
    s.eval_item(&mut eval, "luna", evaluated("luna", &new("luna")));
    s.probe("luna", observed(&old("luna"), ""));
    s.resolve(&mut req, end(ActionResult::Succeeded, Some(0)));
    assert!(
        s.probes.iter().any(|p| p.operation.machine == "luna"),
        "the probe is asked again"
    );
    assert!(
        s.evals.is_empty(),
        "the fleet-wide evaluator will answer for luna; a second one would race it"
    );
}

#[test]
fn the_probe_after_an_update_can_say_unreachable() {
    let mut s = settled(config());
    act_on(&mut s);
    let mut req = update(&mut s);
    s.resolve(&mut req, end(ActionResult::Succeeded, Some(0)));
    s.probe(
        "luna",
        unreachable("ssh: connect to host: Connection refused"),
    );
    assert_eq!(s.row("luna").status, Badge::Unreachable);
}

#[test]
fn a_stale_action_answer_changes_nothing() {
    let mut s = settled(config());
    act_on(&mut s);
    let _req = update(&mut s);
    let before = s.view();
    s.send(Event::ActionEnded {
        id: auspex_core::RequestId(9999),
        end: end(ActionResult::Succeeded, Some(0)),
    });
    assert_eq!(s.view(), before);
    assert!(s.probes.is_empty());
}

// ---------------------------------------------------------------------------------------
// Quitting while an action runs
// ---------------------------------------------------------------------------------------

#[test]
fn quitting_with_nothing_running_is_immediate() {
    let mut s = settled(config());
    s.send(Event::Quit);
    assert!(s.view().quit);
    assert!(s.view().confirm.is_none());
}

#[test]
fn quitting_during_an_update_asks_and_n_keeps_it_running() {
    let mut s = settled(config());
    act_on(&mut s);
    let req = update(&mut s);
    s.send(Event::Quit);
    let confirm = s.view().confirm.expect("a question");
    assert_eq!(confirm.kind, ConfirmKind::Quit);
    assert_eq!(
        confirm.question,
        "an update of luna is running; quit and stop it? (an activation already under way on the machine may complete) y/N"
    );
    assert!(!s.view().quit);
    assert!(
        !s.cancels.contains(&req.operation.id),
        "nothing stopped yet"
    );
    s.send(Event::ConfirmAnswer(false));
    assert!(!s.view().quit);
    assert!(!s.cancels.contains(&req.operation.id));
    assert_eq!(s.view().action.unwrap().status, ActionStatus::Running);
}

#[test]
fn a_yes_cancels_through_the_cancel_effect_and_quit_waits_for_the_answer() {
    let mut s = settled(config());
    act_on(&mut s);
    let mut req = update(&mut s);
    let id = req.operation.id;
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(true));
    assert!(s.cancels.contains(&id), "Effect::Cancel for the action");
    assert!(
        !s.view().quit,
        "not until the shell says the process group is gone"
    );
    assert_eq!(s.view().action.unwrap().status, ActionStatus::Stopping);

    // The shell killed and reaped it, and says so.
    s.resolve(&mut req, end(ActionResult::Cancelled, None));
    assert!(s.view().quit);
    assert!(matches!(
        s.view().action.unwrap().status,
        ActionStatus::Ended(ActionEnd {
            result: ActionResult::Cancelled,
            ..
        })
    ));
    assert!(
        s.probes.is_empty(),
        "a cancelled quit does not refresh anything"
    );
}

#[test]
fn a_second_quit_while_it_is_stopping_forces() {
    let mut s = settled(config());
    act_on(&mut s);
    let _req = update(&mut s);
    s.send(Event::Quit);
    s.send(Event::ConfirmAnswer(true));
    assert!(!s.view().quit);
    s.send(Event::Quit);
    assert!(
        s.view().quit,
        "the operator does not wait for a process that will not die"
    );
}

#[test]
fn a_termination_signal_stops_the_action_without_a_question() {
    let mut s = settled(config());
    act_on(&mut s);
    let mut req = update(&mut s);
    let id = req.operation.id;
    s.send(Event::Terminate);
    assert!(s.view().confirm.is_none(), "nobody can answer a signal");
    assert!(s.cancels.contains(&id));
    assert!(!s.view().quit);
    s.send(Event::Terminate);
    assert!(s.view().quit, "a second one forces");
    // The shell's answer, late, changes nothing it should not.
    s.resolve(&mut req, end(ActionResult::Cancelled, None));
    assert!(s.view().quit);
}

#[test]
fn a_termination_signal_with_nothing_running_quits() {
    let mut s = settled(config());
    s.send(Event::Terminate);
    assert!(s.view().quit);
}

#[test]
fn quitting_during_a_build_asks_too() {
    let mut s = settled(config());
    s.send(Event::BuildSelected);
    let _req = s.take_action();
    s.send(Event::Quit);
    assert_eq!(
        s.view().confirm.unwrap().question,
        "a build of luna is running; quit and stop it? y/N"
    );
}

// ---------------------------------------------------------------------------------------
// A yes while the machine is being evaluated again (re-review finding 4)
// ---------------------------------------------------------------------------------------

#[test]
fn a_yes_while_a_refresh_is_running_is_refused_and_asks_to_ask_again() {
    let mut s = settled(config());
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    // The tree is read again: the old evaluation stays on the row until the new one arrives,
    // so the path still matches, but auspex itself knows it is out of date.
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.send(Event::ConfirmAnswer(true));
    assert!(s.actions.is_empty(), "nothing runs while a refresh runs");
    assert_eq!(
        notice(&s),
        "a refresh is running; ask again when it has finished"
    );
    assert!(s.view().confirm.is_none(), "the question is over");

    // When it has finished, the same path deploys.
    s.resolve(&mut list, Ok(inventory(THREE)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    assert_eq!(update(&mut s).operation.machine, "luna");
}

#[test]
fn a_yes_while_the_evaluation_is_still_streaming_is_refused_though_the_machine_has_one() {
    // The fleet is being evaluated: luna's result has arrived (so `u` may ask), the stream has
    // not finished (so the tree is still being read).
    let mut s = Shell::new();
    s.start_with(config(), THREE);
    let mut eval = s.take_eval();
    s.eval_item(&mut eval, "luna", evaluated("luna", &new("luna")));
    s.probe("luna", observed(&old("luna"), ""));
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    assert!(s.view().confirm.is_some(), "the question is asked");
    s.send(Event::ConfirmAnswer(true));
    assert!(
        s.actions.is_empty(),
        "nothing runs while the evaluation runs"
    );
    assert_eq!(
        notice(&s),
        "a refresh is running; ask again when it has finished"
    );
    // Finished: the same question deploys.
    s.eval_item(&mut eval, "sol", evaluated("sol", &new("sol")));
    s.eval_item(&mut eval, "terra", evaluated("terra", &new("terra")));
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    assert_eq!(update(&mut s).operation.machine, "luna");
}
