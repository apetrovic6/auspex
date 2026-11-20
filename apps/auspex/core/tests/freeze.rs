//! Freezing (`f`), decided by the core alone: a hand-driven shell, no process, no file.
//!
//! The properties, each with a test that fails when its rule is broken (the report lists how
//! each was checked): a freeze is applied at once and asked of the shell to keep; an unfreeze
//! is asked about by name and applied only once kept; `u` refuses a frozen machine, at the key
//! and again at the `y`; a freeze in the config cannot be undone by `f`; and when it is not
//! known which machines are frozen nothing is updated, and `f` changes nothing.

mod support;

use std::collections::BTreeSet;

use auspex_core::effects::{
    ActionEnd, ActionKind, ActionRequest, ActionResult, EvalAnswer, SaveFreezeRequest,
};
use auspex_core::{Config, ConfirmKind, Event, FreezeConfig, Mode, Phase, SavedFreezes};
use auspex_domain::status::Badge;
use crux_core::Request;
use support::{Shell, THREE, clean_preflight, evaluated, observed, path};

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn config(declared: &[&str], saved: SavedFreezes) -> Config {
    Config {
        flake: "/fleet".to_owned(),
        allow_act: true,
        local_system: "x86_64-linux".to_owned(),
        freeze: FreezeConfig {
            declared: set(declared),
            saved,
        },
        ..Config::default()
    }
}

fn known(names: &[&str]) -> SavedFreezes {
    SavedFreezes::Known(set(names))
}

fn old(m: &str) -> String {
    path('a', &format!("nixos-system-{m}-old"))
}

fn new(m: &str) -> String {
    path('b', &format!("nixos-system-{m}-new"))
}

/// luna, sol and terra, scanned to the end, each with an update pending. luna is selected.
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
    s
}

fn act_on(s: &mut Shell) {
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().mode, Mode::Act);
}

fn select(s: &mut Shell, name: &str) {
    s.send(Event::Search(name.to_owned()));
    let v = s.view();
    assert_eq!(
        v.report.machines[v.selected.expect("a selection")].name,
        name
    );
}

fn notice(s: &Shell) -> String {
    s.view().notice.expect("a notice")
}

fn frozen(s: &Shell, m: &str) -> bool {
    s.row(m).frozen
}

fn has_snowflake(s: &Shell, m: &str) -> bool {
    s.row(m).badges.contains(&Badge::Frozen)
}

fn saved(s: &mut Shell, request: &mut Request<SaveFreezeRequest>, names: &[&str]) {
    s.resolve(request, Ok(set(names)));
}

/// The shell's answer to the checks before an update (the one request `u` made): clean.
fn answer_checks(s: &mut Shell) {
    let mut request = s.take_preflight();
    s.resolve(&mut request, clean_preflight());
}

const fn end() -> ActionEnd {
    ActionEnd {
        result: ActionResult::Succeeded,
        exit_code: Some(0),
        summary: Vec::new(),
        explanation: None,
    }
}

// ---------------------------------------------------------------------------------------
// f
// ---------------------------------------------------------------------------------------

#[test]
fn f_freezes_the_selected_machine_at_once_without_act_mode_or_a_question() {
    let mut s = settled(config(&[], known(&[])));
    assert_eq!(s.view().mode, Mode::Observe);
    assert!(!frozen(&s, "luna"));
    s.send(Event::ToggleFreeze);
    // Applied before the save is answered, so `u` is refused from this moment.
    assert!(frozen(&s, "luna"), "the report says frozen");
    assert!(has_snowflake(&s, "luna"), "and the row shows ❄");
    assert!(
        !frozen(&s, "sol") && !frozen(&s, "terra"),
        "only that machine"
    );
    assert!(s.view().confirm.is_none(), "freezing asks nothing");
    let request = s.take_freeze();
    assert_eq!(request.operation.machine, "luna");
    assert!(request.operation.frozen);
    assert!(s.view().freeze.saving);
}

#[test]
fn a_kept_freeze_adopts_the_set_the_file_now_holds() {
    let mut s = settled(config(&[], known(&[])));
    s.send(Event::ToggleFreeze);
    let mut r = s.take_freeze();
    // Another running auspex froze sol meanwhile: the answer is the file's whole set.
    saved(&mut s, &mut r, &["luna", "sol"]);
    assert!(frozen(&s, "luna") && frozen(&s, "sol") && !frozen(&s, "terra"));
    assert!(!s.view().freeze.saving);
    assert_eq!(s.view().freeze.frozen(), BTreeSet::from(["luna", "sol"]));
    assert_eq!(notice(&s), "luna frozen");
}

#[test]
fn a_freeze_that_could_not_be_kept_stays_for_this_run_and_says_so() {
    let mut s = settled(config(&[], known(&[])));
    s.send(Event::ToggleFreeze);
    let mut r = s.take_freeze();
    s.resolve(&mut r, Err("the disk is full".to_owned()));
    assert!(
        frozen(&s, "luna"),
        "a freeze is not undone by a failed save"
    );
    let n = notice(&s);
    assert!(
        n.contains("this run only") && n.contains("the disk is full"),
        "{n}"
    );
    assert!(!s.view().freeze.saving, "the next f may try again");
}

#[test]
fn a_save_answered_without_the_machine_does_not_unfreeze_it() {
    let mut s = settled(config(&[], known(&[])));
    s.send(Event::ToggleFreeze);
    let mut r = s.take_freeze();
    saved(&mut s, &mut r, &[]);
    assert!(frozen(&s, "luna"), "what was asked for is never forgotten");
}

#[test]
fn an_answer_for_another_request_changes_nothing() {
    let mut s = settled(config(&[], known(&[])));
    s.send(Event::ToggleFreeze);
    let mut r = s.take_freeze();
    s.send(Event::FreezeSaved {
        id: auspex_core::RequestId(9_999),
        answer: Ok(set(&["terra"])),
    });
    assert!(!frozen(&s, "terra"), "a stale answer is ignored");
    assert!(s.view().freeze.saving, "and the real one is still awaited");
    saved(&mut s, &mut r, &["luna"]);
    assert!(!s.view().freeze.saving);
}

#[test]
fn only_one_save_is_out_at_a_time() {
    let mut s = settled(config(&[], known(&[])));
    s.send(Event::ToggleFreeze);
    select(&mut s, "sol");
    s.send(Event::ToggleFreeze);
    assert!(notice(&s).contains("still being saved"), "{}", notice(&s));
    assert!(!frozen(&s, "sol"));
    assert_eq!(s.freezes.len(), 1, "no second request");
}

#[test]
fn a_freeze_survives_a_refresh() {
    let mut s = settled(config(&[], known(&[])));
    s.send(Event::ToggleFreeze);
    let mut r = s.take_freeze();
    saved(&mut s, &mut r, &["luna"]);
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(support::inventory(THREE)));
    assert!(frozen(&s, "luna"), "the rows are new, the freeze is not");
}

// ---------------------------------------------------------------------------------------
// Unfreezing
// ---------------------------------------------------------------------------------------

/// luna frozen with `f` in an earlier run.
fn luna_frozen() -> Shell {
    settled(config(&[], known(&["luna"])))
}

#[test]
fn a_freeze_from_an_earlier_run_is_there_at_the_start() {
    let s = luna_frozen();
    assert!(frozen(&s, "luna") && has_snowflake(&s, "luna"));
    assert!(!frozen(&s, "sol"));
}

#[test]
fn unfreezing_asks_by_name_and_nothing_changes_before_the_yes() {
    let mut s = luna_frozen();
    s.send(Event::ToggleFreeze);
    let confirm = s.view().confirm.expect("a question");
    assert_eq!(confirm.kind, ConfirmKind::Unfreeze);
    assert!(
        confirm.question.contains("unfreeze luna"),
        "{}",
        confirm.question
    );
    assert!(confirm.question.ends_with("y/N"), "{}", confirm.question);
    assert!(s.freezes.is_empty(), "nothing is asked of the shell yet");
    assert!(frozen(&s, "luna"));
    // Anything but yes is no.
    s.send(Event::ConfirmAnswer(false));
    assert!(s.freezes.is_empty() && frozen(&s, "luna"));
    assert!(s.view().confirm.is_none());
}

#[test]
fn a_yes_asks_the_shell_to_unfreeze_and_applies_it_only_once_kept() {
    let mut s = luna_frozen();
    s.send(Event::ToggleFreeze);
    s.send(Event::ConfirmAnswer(true));
    let mut r = s.take_freeze();
    assert_eq!(r.operation.machine, "luna");
    assert!(!r.operation.frozen);
    // Still frozen until the file says otherwise: a restart believes the file.
    assert!(frozen(&s, "luna"));
    saved(&mut s, &mut r, &[]);
    assert!(!frozen(&s, "luna") && !has_snowflake(&s, "luna"));
    assert_eq!(notice(&s), "luna unfrozen");
}

#[test]
fn an_unfreeze_that_could_not_be_kept_leaves_the_machine_frozen() {
    let mut s = luna_frozen();
    s.send(Event::ToggleFreeze);
    s.send(Event::ConfirmAnswer(true));
    let mut r = s.take_freeze();
    s.resolve(&mut r, Err("permission denied".to_owned()));
    assert!(frozen(&s, "luna"));
    let n = notice(&s);
    assert!(
        n.contains("still frozen") && n.contains("permission denied"),
        "{n}"
    );
}

#[test]
fn another_auspex_refreezing_during_an_unfreeze_wins() {
    let mut s = luna_frozen();
    s.send(Event::ToggleFreeze);
    s.send(Event::ConfirmAnswer(true));
    let mut r = s.take_freeze();
    // The file, after our write, still holds luna: someone else froze it again.
    saved(&mut s, &mut r, &["luna"]);
    assert!(frozen(&s, "luna"));
}

// ---------------------------------------------------------------------------------------
// u on a frozen machine
// ---------------------------------------------------------------------------------------

#[test]
fn u_refuses_a_frozen_machine_and_asks_nothing() {
    let mut s = luna_frozen();
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    assert_eq!(notice(&s), "luna is frozen; f to unfreeze");
    assert!(
        s.view().confirm.is_none(),
        "no question about a refused update"
    );
    assert!(s.actions.is_empty(), "and nothing runs");
    // A machine that is not frozen is still asked about.
    select(&mut s, "sol");
    s.send(Event::UpdateSelected);
    answer_checks(&mut s);
    assert_eq!(
        s.view().confirm.expect("a question").kind,
        ConfirmKind::Update
    );
}

#[test]
fn u_refuses_a_machine_the_moment_it_is_frozen() {
    let mut s = settled(config(&[], known(&[])));
    act_on(&mut s);
    s.send(Event::ToggleFreeze);
    // The save has not been answered: the freeze already counts.
    s.send(Event::UpdateSelected);
    assert_eq!(notice(&s), "luna is frozen; f to unfreeze");
    assert!(s.actions.is_empty());
}

#[test]
fn a_freeze_made_while_the_update_question_is_open_stops_the_yes() {
    let mut s = settled(config(&[], known(&[])));
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    answer_checks(&mut s);
    assert_eq!(
        s.view().confirm.expect("a question").kind,
        ConfirmKind::Update
    );
    // The shell never sends `f` while a question is open, but the core must not rely on that:
    // the machine is checked again at the `y`.
    s.send(Event::ToggleFreeze);
    s.send(Event::ConfirmAnswer(true));
    assert!(
        s.actions.is_empty(),
        "the yes is to a question about a frozen machine"
    );
    assert_eq!(notice(&s), "luna is frozen; f to unfreeze");
}

#[test]
fn a_freeze_made_while_the_checks_run_stops_the_update_before_any_question() {
    let mut s = settled(config(&[], known(&[])));
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    assert_eq!(
        s.view().confirm.expect("the checks run").kind,
        ConfirmKind::Checking
    );
    s.send(Event::ToggleFreeze);
    answer_checks(&mut s);
    let asked = s.view().confirm.map(|c| c.kind);
    assert_ne!(
        asked,
        Some(ConfirmKind::Update),
        "no question about a frozen machine"
    );
    assert!(s.actions.is_empty());
    assert_eq!(notice(&s), "luna is frozen; f to unfreeze");
}

#[test]
fn u_is_allowed_again_once_the_unfreeze_is_kept() {
    let mut s = luna_frozen();
    act_on(&mut s);
    s.send(Event::ToggleFreeze);
    s.send(Event::ConfirmAnswer(true));
    // Not yet kept: still refused.
    s.send(Event::UpdateSelected);
    assert!(notice(&s).contains("frozen"));
    let mut r = s.take_freeze();
    saved(&mut s, &mut r, &[]);
    s.send(Event::UpdateSelected);
    answer_checks(&mut s);
    assert_eq!(
        s.view().confirm.expect("a question").kind,
        ConfirmKind::Update
    );
    s.send(Event::ConfirmAnswer(true));
    let mut a: Request<ActionRequest> = s.take_action();
    assert_eq!(a.operation.kind, ActionKind::Update);
    assert_eq!(a.operation.machine, "luna");
    s.resolve(&mut a, end());
}

#[test]
fn a_build_is_not_refused_for_a_frozen_machine() {
    // `b` changes nothing on any machine, so a freeze, which only prevents changes, leaves it.
    let mut s = luna_frozen();
    s.send(Event::BuildSelected);
    let a = s.take_action();
    assert_eq!(a.operation.kind, ActionKind::Build);
    assert_eq!(a.operation.machine, "luna");
}

// ---------------------------------------------------------------------------------------
// Frozen in config.toml
// ---------------------------------------------------------------------------------------

#[test]
fn a_machine_frozen_in_the_config_is_frozen_without_any_request() {
    let s = settled(config(&["luna"], known(&[])));
    assert!(frozen(&s, "luna") && has_snowflake(&s, "luna"));
    assert!(s.freezes.is_empty());
    assert_eq!(s.view().freeze.declared, set(&["luna"]));
}

#[test]
fn f_cannot_unfreeze_what_the_config_froze() {
    let mut s = settled(config(&["luna"], known(&[])));
    s.send(Event::ToggleFreeze);
    let n = notice(&s);
    assert!(
        n.contains("config.toml") && n.contains("machines.luna.frozen") && n.contains("cannot"),
        "{n}"
    );
    assert!(
        s.view().confirm.is_none(),
        "no question: there is nothing f could do"
    );
    assert!(s.freezes.is_empty(), "and nothing is asked of the shell");
    assert!(frozen(&s, "luna"));
}

#[test]
fn a_machine_frozen_in_both_places_stays_frozen_by_the_config() {
    // Frozen with `f` in an earlier run, then declared: the declaration wins and `f` is refused.
    let mut s = settled(config(&["luna"], known(&["luna"])));
    s.send(Event::ToggleFreeze);
    assert!(s.view().confirm.is_none() && s.freezes.is_empty());
    assert!(frozen(&s, "luna"));
}

#[test]
fn u_refuses_a_machine_frozen_in_the_config_and_names_the_setting() {
    let mut s = settled(config(&["luna"], known(&[])));
    act_on(&mut s);
    s.send(Event::UpdateSelected);
    let n = notice(&s);
    assert!(
        n.contains("frozen in config.toml") && n.contains("machines.luna.frozen"),
        "{n}"
    );
    assert!(s.view().confirm.is_none() && s.actions.is_empty());
}

// ---------------------------------------------------------------------------------------
// Unknown
// ---------------------------------------------------------------------------------------

fn unknown() -> Shell {
    settled(config(
        &["terra"],
        SavedFreezes::Unknown("the state file is corrupt".to_owned()),
    ))
}

#[test]
fn when_it_is_not_known_which_machines_are_frozen_u_is_refused_for_every_machine() {
    let mut s = unknown();
    act_on(&mut s);
    for m in ["luna", "sol"] {
        select(&mut s, m);
        s.send(Event::UpdateSelected);
        let n = notice(&s);
        assert!(
            n.contains("unknown") && n.contains("the state file is corrupt") && n.contains(m),
            "{n}"
        );
        assert!(s.view().confirm.is_none() && s.actions.is_empty(), "{m}");
    }
}

#[test]
fn when_it_is_not_known_f_changes_nothing() {
    let mut s = unknown();
    s.send(Event::ToggleFreeze);
    assert!(notice(&s).contains("unknown"), "{}", notice(&s));
    assert!(
        s.freezes.is_empty(),
        "a corrupt or newer file is not written over"
    );
    assert!(!frozen(&s, "luna"), "and nothing is guessed");
}

#[test]
fn unknown_is_in_the_view_and_the_declared_freezes_still_apply() {
    let s = unknown();
    let v = s.view();
    assert_eq!(
        v.freeze.unknown.as_deref(),
        Some("the state file is corrupt")
    );
    assert!(v.freeze.saved.is_empty());
    assert!(frozen(&s, "terra"), "a declaration needs no file");
    assert!(
        !frozen(&s, "luna"),
        "an unknown machine is not shown frozen: that would be a guess"
    );
}

#[test]
fn a_build_is_not_refused_when_it_is_unknown_which_machines_are_frozen() {
    let mut s = unknown();
    s.send(Event::BuildSelected);
    assert_eq!(s.take_action().operation.kind, ActionKind::Build);
}

// ---------------------------------------------------------------------------------------
// What U will need
// ---------------------------------------------------------------------------------------

#[test]
fn the_frozen_set_is_in_the_view_for_u_to_skip() {
    let s = settled(config(&["terra"], known(&["luna"])));
    let freeze = s.view().freeze;
    assert_eq!(freeze.frozen(), BTreeSet::from(["luna", "terra"]));
    assert!(freeze.is_frozen("luna") && freeze.is_frozen("terra") && !freeze.is_frozen("sol"));
    assert_eq!(freeze.declared, set(&["terra"]));
    assert_eq!(freeze.saved, set(&["luna"]));
}

#[test]
fn no_machine_selected_says_so() {
    let mut s = Shell::new();
    s.start_with(config(&[], known(&[])), "{}");
    s.send(Event::ToggleFreeze);
    assert_eq!(notice(&s), "no machine selected");
    assert!(s.freezes.is_empty());
}

// ---------------------------------------------------------------------------------------
// A question that outlives what it asked about
// ---------------------------------------------------------------------------------------

/// Starts the core again (a shell that replaces the configuration) while "unfreeze luna?" is
/// open, then answers it.
fn unfreeze_question_across(new: Config) -> Shell {
    let mut s = luna_frozen();
    s.send(Event::ToggleFreeze);
    assert_eq!(
        s.view().confirm.expect("a question").kind,
        ConfirmKind::Unfreeze
    );
    s.send(Event::Start(new));
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(support::inventory(THREE)));
    s.send(Event::ConfirmAnswer(true));
    s
}

#[test]
fn a_yes_to_unfreeze_is_checked_again_and_not_obeyed_for_a_machine_no_longer_frozen_with_f() {
    // Frozen with `f` no longer (the file was emptied between the question and the answer).
    let s = unfreeze_question_across(config(&[], known(&[])));
    assert!(
        s.freezes.is_empty(),
        "nothing to unfreeze, so nothing is asked of the shell"
    );
    assert!(
        notice(&s).contains("not frozen with f any more"),
        "{}",
        notice(&s)
    );
}

#[test]
fn a_yes_to_unfreeze_is_not_obeyed_for_a_machine_the_config_now_freezes() {
    let s = unfreeze_question_across(config(&["luna"], known(&["luna"])));
    assert!(
        s.freezes.is_empty(),
        "the config's freeze is not f's to undo"
    );
    assert!(frozen(&s, "luna"));
}
