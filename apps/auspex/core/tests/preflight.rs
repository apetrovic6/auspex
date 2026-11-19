//! The checks between `u` and the confirmation, decided by the core alone: a hand-driven
//! shell, no process, no clock.
//!
//! `u` asks the shell for what the checks need (the fleet repository's state and the pending
//! closure's size), and the answer opens **one dialog** that carries everything: refusals
//! first (no way to proceed), then warnings, then the question. These tests drive that dialog
//! and assert on what the operator would see and on whether an action request was ever made.
//!
//! Each rule has a test that fails when the rule is broken; the report lists how each was
//! checked.

mod support;

use std::collections::BTreeSet;
use std::net::IpAddr;

use auspex_core::effects::{ActionRequest, EvalAnswer};
use auspex_core::{Config, ConfirmKind, ConfirmView, Event, Mode, PreflightAnswer};
use auspex_domain::preflight::{TreeState, Upstream};
use crux_core::Request;
use support::{Shell, clean_preflight, evaluated, inventory, observed, path, sized};

const GIB_KIB: u64 = 1 << 20;

fn config() -> Config {
    Config {
        flake: "/fleet".to_owned(),
        allow_act: true,
        local_system: "x86_64-linux".to_owned(),
        preflight_timeout_seconds: 90,
        ..Config::default()
    }
}

fn public(machines: &[&str]) -> Config {
    Config {
        public_machines: machines
            .iter()
            .map(|m| (*m).to_owned())
            .collect::<BTreeSet<_>>(),
        ..config()
    }
}

const FLEET: &str = r#"{
  "terra": {"deploy": {"targetHost": "root@192.168.1.10"}},
  "luna":  {"deploy": {"targetHost": "root@192.168.1.11"}},
  "sol":   {"deploy": {"targetHost": "root@192.168.1.12"}}
}"#;

fn old(m: &str) -> String {
    path('a', &format!("nixos-system-{m}-old"))
}

fn new(m: &str) -> String {
    path('b', &format!("nixos-system-{m}-new"))
}

/// A probe whose `/nix/store` filesystem has `free_gib` GiB available (`None`: the probe
/// did not report it).
fn probe_with_store(m: &str, free_gib: Option<u64>) -> auspex_domain::observe::ProbeOutcome {
    let extra = free_gib.map_or_else(String::new, |g| {
        format!(
            "store_disk_kb={} {} {}\n",
            1000 * GIB_KIB,
            (1000 - g) * GIB_KIB,
            g * GIB_KIB
        )
    });
    observed(&old(m), &extra)
}

/// luna, sol and terra, scanned to the end, luna selected and act mode on. Each machine's
/// store has `free_gib` free.
fn ready(config: Config, json: &str, free_gib: Option<u64>) -> Shell {
    let mut s = Shell::new();
    s.start_with(config, json);
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        s.probe(m, probe_with_store(m, free_gib));
    }
    // Not asserted settled: a fleet with a hostname target settles only once it is resolved.
    assert_eq!(s.view().refresh.pending_machines, 0);
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().mode, Mode::Act);
    s
}

fn answer(s: &mut Shell, answer: PreflightAnswer) {
    let mut r = s.take_preflight();
    s.resolve(&mut r, answer);
}

/// `u`, then the shell's answer: the dialog the operator sees.
fn dialog(s: &mut Shell, answer_of_shell: PreflightAnswer) -> ConfirmView {
    s.send(Event::UpdateSelected);
    answer(s, answer_of_shell);
    s.view().confirm.expect("a dialog")
}

fn tree(changed: usize, upstream: Upstream) -> PreflightAnswer {
    PreflightAnswer {
        tree: Ok(TreeState {
            changed_files: changed,
            upstream,
        }),
        ..clean_preflight()
    }
}

fn tracking(ahead: u64) -> Upstream {
    Upstream::Tracking {
        name: "origin/main".to_owned(),
        ahead,
        behind: 0,
    }
}

fn closure(gib: u64) -> PreflightAnswer {
    PreflightAnswer {
        closure: sized(gib << 30),
        ..clean_preflight()
    }
}

fn notice(s: &Shell) -> String {
    s.view().notice.expect("a notice")
}

fn nothing_runs(s: &Shell) {
    assert!(s.actions.is_empty(), "no action request may have been made");
    assert!(s.view().action.is_none());
}

fn proceeds(s: &mut Shell) -> Request<ActionRequest> {
    s.send(Event::ConfirmAnswer(true));
    s.take_action()
}

// ---------------------------------------------------------------------------------------
// The request itself
// ---------------------------------------------------------------------------------------

#[test]
fn u_asks_the_shell_for_exactly_what_the_checks_need_and_decides_nothing_yet() {
    let mut s = ready(config(), FLEET, Some(100));
    s.send(Event::UpdateSelected);
    let request = s.take_preflight();
    assert_eq!(request.operation.machine, "luna");
    assert_eq!(request.operation.flake, "/fleet");
    assert_eq!(request.operation.store_path.as_str(), new("luna"));
    assert_eq!(request.operation.timeout_seconds, 90);
    let view = s.view();
    let confirm = view.confirm.expect("a dialog while the checks run");
    assert_eq!(confirm.kind, ConfirmKind::Checking);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert_eq!(confirm.warnings, Vec::<String>::new());
    nothing_runs(&s);
    // `y` is not a yes to a question nobody has seen: it cancels the checks.
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert!(s.view().confirm.is_none());
}

#[test]
fn build_runs_none_of_the_checks() {
    let mut s = ready(config(), FLEET, Some(100));
    s.send(Event::BuildSelected);
    assert!(s.preflights.is_empty(), "b is not an update");
    assert_eq!(s.actions.len(), 1);
}

#[test]
fn cancelling_the_checks_cancels_the_shells_work_and_a_late_answer_opens_nothing() {
    let mut s = ready(config(), FLEET, Some(100));
    s.send(Event::UpdateSelected);
    let mut request = s.take_preflight();
    let id = request.operation.id;
    s.send(Event::ConfirmAnswer(false));
    assert!(s.cancels.contains(&id), "the shell is told to stop");
    assert!(s.view().confirm.is_none());
    // The shell answers anyway.
    s.resolve(&mut request, clean_preflight());
    assert!(
        s.view().confirm.is_none(),
        "no dialog for a cancelled check"
    );
    nothing_runs(&s);
}

#[test]
fn quitting_while_the_checks_run_cancels_them() {
    let mut s = ready(config(), FLEET, Some(100));
    s.send(Event::UpdateSelected);
    let id = s.take_preflight().operation.id;
    s.send(Event::Quit);
    assert!(s.cancels.contains(&id));
    assert!(s.view().quit);
}

#[test]
fn an_answer_for_another_request_is_ignored() {
    let mut s = ready(config(), FLEET, Some(100));
    s.send(Event::UpdateSelected);
    let mut first = s.take_preflight();
    // `u` again (the list may be redrawn, the key pressed twice): a new check replaces it.
    s.send(Event::UpdateSelected);
    assert!(s.cancels.contains(&first.operation.id));
    let second = s.take_preflight();
    s.resolve(&mut first, clean_preflight());
    assert_eq!(
        s.view().confirm.expect("still checking").kind,
        ConfirmKind::Checking,
        "the first request's answer is not the second's"
    );
    assert_ne!(first.operation.id, second.operation.id);
}

// ---------------------------------------------------------------------------------------
// 1. The fleet repository
// ---------------------------------------------------------------------------------------

#[test]
fn a_clean_pushed_tree_with_room_has_nothing_to_show_and_y_runs_the_update() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert!(confirm.warnings.is_empty(), "{:?}", confirm.warnings);
    assert_eq!(
        confirm.question,
        "update luna to b1234567 nixos-system-luna-new? y/N"
    );
    nothing_runs(&s);
    let request = proceeds(&mut s);
    assert_eq!(request.operation.machine, "luna");
}

#[test]
fn a_dirty_tree_warns_and_still_proceeds_with_y() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, tree(3, tracking(0)));
    assert_eq!(
        confirm.kind,
        ConfirmKind::Update,
        "a warning does not refuse"
    );
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert_eq!(confirm.warnings.len(), 1);
    assert!(
        confirm.warnings[0].contains("uncommitted changes to 3 tracked files"),
        "{:?}",
        confirm.warnings
    );
    nothing_runs(&s);
    let _ = proceeds(&mut s);
}

#[test]
fn unpushed_commits_warn_and_still_proceed() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, tree(0, tracking(2)));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.warnings.len(), 1);
    assert!(
        confirm.warnings[0].contains("2 commits ahead of origin/main"),
        "{:?}",
        confirm.warnings
    );
    let _ = proceeds(&mut s);
}

#[test]
fn a_dirty_and_unpushed_tree_is_two_warnings() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, tree(1, tracking(1)));
    assert_eq!(confirm.warnings.len(), 2, "{:?}", confirm.warnings);
}

#[test]
fn no_upstream_is_said_and_is_not_a_failure() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, tree(0, Upstream::None));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert!(
        confirm.warnings[0].contains("no upstream"),
        "{:?}",
        confirm.warnings
    );
    let _ = proceeds(&mut s);
}

#[test]
fn a_tree_check_that_could_not_run_is_a_warning_not_a_refusal() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(
        &mut s,
        PreflightAnswer {
            tree: Err("git status exited 128: fatal: not a git repository".to_owned()),
            ..clean_preflight()
        },
    );
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert!(
        confirm.warnings[0].contains("not a git repository"),
        "{:?}",
        confirm.warnings
    );
    let _ = proceeds(&mut s);
}

// ---------------------------------------------------------------------------------------
// 2. Free disk
// ---------------------------------------------------------------------------------------

#[test]
fn enough_free_disk_passes() {
    // 4 GiB closure + 1 GiB margin = 5 GiB: exactly 5 free is enough.
    let mut s = ready(config(), FLEET, Some(5));
    let confirm = dialog(&mut s, closure(4));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert_eq!(confirm.warnings, Vec::<String>::new());
    let _ = proceeds(&mut s);
}

#[test]
fn too_little_free_disk_refuses_and_y_does_nothing() {
    let mut s = ready(config(), FLEET, Some(4));
    let confirm = dialog(&mut s, closure(4));
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert_eq!(confirm.refusals.len(), 1);
    assert!(
        confirm.refusals[0].contains("luna's /nix/store has 4.0 GiB free")
            && confirm.refusals[0].contains("needs 5.0 GiB"),
        "{:?}",
        confirm.refusals
    );
    assert!(!confirm.question.contains("y/N"), "{}", confirm.question);
    // There is no way to proceed: `y` closes the dialog and nothing runs.
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert!(s.view().confirm.is_none());
    assert!(
        s.view().notice.is_none(),
        "closed by the dialog itself, not refused again by the answer"
    );
    // Nor any other key, nor the typed-name events.
    let confirm = dialog(&mut s, closure(4));
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    for event in [
        Event::ConfirmChar('l'),
        Event::ConfirmSubmit,
        Event::ConfirmBackspace,
    ] {
        s.send(event);
    }
    nothing_runs(&s);
}

#[test]
fn unknown_disk_inputs_are_warnings() {
    // The probe did not report /nix/store.
    let mut s = ready(config(), FLEET, None);
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert!(
        confirm.warnings[0].contains("free space on luna's /nix/store is not known"),
        "{:?}",
        confirm.warnings
    );
    let _ = proceeds(&mut s);

    // The closure could not be sized.
    let mut s = ready(config(), FLEET, Some(1));
    let confirm = dialog(
        &mut s,
        PreflightAnswer {
            closure: Err("closure size query timed out after 90s".to_owned()),
            ..clean_preflight()
        },
    );
    assert_eq!(
        confirm.kind,
        ConfirmKind::Update,
        "unknown is not too little"
    );
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert!(
        confirm.warnings[0].contains("size of the pending closure is not known")
            && confirm.warnings[0].contains("timed out"),
        "{:?}",
        confirm.warnings
    );
    let _ = proceeds(&mut s);
}

#[test]
fn the_free_space_is_the_stores_and_not_the_roots() {
    // The probe of `observed` says `/` has 600 KiB free; only `/nix/store`'s line counts, so a
    // root of tmpfs size neither refuses nor passes the update by itself.
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, closure(50));
    assert_eq!(
        confirm.kind,
        ConfirmKind::Update,
        "100 GiB free for a 50 GiB closure"
    );
    let mut s = ready(config(), FLEET, None);
    let confirm = dialog(&mut s, closure(0));
    assert_eq!(
        confirm.kind,
        ConfirmKind::Update,
        "the 600 KiB free on / is not what the store has"
    );
    assert!(
        confirm.warnings[0].contains("not known"),
        "{:?}",
        confirm.warnings
    );
}

// ---------------------------------------------------------------------------------------
// 4. Duplicate address
// ---------------------------------------------------------------------------------------

const SHARED: &str = r#"{
  "terra": {"deploy": {"targetHost": "root@192.168.1.10"}},
  "luna":  {"deploy": {"targetHost": "root@192.168.1.11"}},
  "sol":   {"deploy": {"targetHost": "root@192.168.1.11"}}
}"#;

#[test]
fn a_machine_sharing_an_address_is_refused_and_the_machines_are_named() {
    let mut s = ready(config(), SHARED, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert_eq!(confirm.refusals.len(), 1);
    assert!(
        confirm.refusals[0].contains("192.168.1.11")
            && confirm.refusals[0].contains("sol also resolves to"),
        "{:?}",
        confirm.refusals
    );
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    // terra has its own address: not affected by luna and sol.
    s.send(Event::MoveDown(2));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
}

#[test]
fn a_hostname_that_resolves_to_another_machines_address_is_refused() {
    let json = r#"{
      "luna": {"deploy": {"targetHost": "root@luna.lab.example"}},
      "sol":  {"deploy": {"targetHost": "root@192.168.1.12"}},
      "terra": {"deploy": {"targetHost": "root@192.168.1.10"}}
    }"#;
    let mut s = Shell::new();
    s.start_with(config(), json);
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        s.probe(m, probe_with_store(m, Some(100)));
    }
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));

    // Resolution has not answered: that is not "unknown", it is not here yet, and `u` is
    // refused (nothing is asked, nothing runs).
    s.send(Event::UpdateSelected);
    assert!(
        notice(&s).contains("still resolving addresses"),
        "{}",
        notice(&s)
    );
    assert!(s.preflights.is_empty() && s.view().confirm.is_none());

    // The wildcard record answers with sol's address.
    let mut resolve = s.resolves.remove(0);
    let sol: IpAddr = "192.168.1.12".parse().unwrap();
    s.resolve(
        &mut resolve,
        std::iter::once(("luna".to_owned(), vec![sol])).collect(),
    );
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert!(
        confirm.refusals[0].contains("192.168.1.12")
            && confirm.refusals[0].contains("sol also resolves to"),
        "{:?}",
        confirm.refusals
    );
}

#[test]
fn a_hostname_with_its_own_address_passes_and_an_empty_answer_refuses() {
    let json = r#"{
      "luna": {"deploy": {"targetHost": "root@luna.lab.example"}},
      "sol":  {"deploy": {"targetHost": "root@192.168.1.12"}},
      "terra": {"deploy": {"targetHost": "root@192.168.1.10"}}
    }"#;
    let setup = || {
        let mut s = Shell::new();
        s.start_with(config(), json);
        let mut eval = s.take_eval();
        for m in ["luna", "sol", "terra"] {
            s.eval_item(&mut eval, m, evaluated(m, &new(m)));
        }
        s.resolve(&mut eval, EvalAnswer::Finished { error: None });
        for m in ["luna", "sol", "terra"] {
            s.probe(m, probe_with_store(m, Some(100)));
        }
        s.send(Event::ToggleAct);
        s.send(Event::ConfirmAnswer(true));
        s
    };
    let mut s = setup();
    let mut resolve = s.resolves.remove(0);
    let own: IpAddr = "192.168.1.99".parse().unwrap();
    s.resolve(
        &mut resolve,
        std::iter::once(("luna".to_owned(), vec![own])).collect(),
    );
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);

    // The name did not resolve at all: nothing to tell it apart by.
    let mut s = setup();
    let mut resolve = s.resolves.remove(0);
    s.resolve(&mut resolve, std::collections::BTreeMap::new());
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Refused);
}

#[test]
fn another_machines_unresolved_hostname_is_a_warning_not_a_refusal() {
    let json = r#"{
      "luna": {"deploy": {"targetHost": "root@192.168.1.11"}},
      "sol":  {"deploy": {"targetHost": "root@sol.lab.example"}},
      "terra": {"deploy": {"targetHost": "root@192.168.1.10"}}
    }"#;
    let mut s = ready(config(), json, Some(100));
    // Resolution finished and sol's name did not resolve: a warning for luna.
    let mut resolve = s.resolves.remove(0);
    s.resolve(&mut resolve, std::collections::BTreeMap::new());
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert!(
        confirm.warnings[0].contains("1 of the other machines have no known address"),
        "{:?}",
        confirm.warnings
    );
}

// ---------------------------------------------------------------------------------------
// 5. The dialog: refusals first, then warnings
// ---------------------------------------------------------------------------------------

#[test]
fn the_dialog_carries_refusals_and_warnings_together_with_no_way_to_proceed() {
    let mut s = ready(config(), SHARED, Some(3));
    let confirm = dialog(
        &mut s,
        PreflightAnswer {
            closure: sized(4 << 30),
            ..tree(2, tracking(1))
        },
    );
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    // Two refusals (disk, address), two warnings (dirty, unpushed), all at once.
    assert_eq!(confirm.refusals.len(), 2, "{:?}", confirm.refusals);
    assert_eq!(confirm.warnings.len(), 2, "{:?}", confirm.warnings);
    assert!(
        confirm
            .refusals
            .iter()
            .any(|r| r.contains("/nix/store has"))
    );
    assert!(confirm.refusals.iter().any(|r| r.contains("192.168.1.11")));
    assert!(confirm.warnings.iter().any(|w| w.contains("uncommitted")));
    assert!(confirm.warnings.iter().any(|w| w.contains("ahead of")));
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
}

#[test]
fn warnings_alone_keep_the_question_and_show_the_warnings_with_it() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, tree(1, Upstream::None));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.warnings.len(), 2);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert!(confirm.question.ends_with("y/N"));
    assert!(confirm.typed.is_none());
    // Any other answer cancels.
    s.send(Event::ConfirmAnswer(false));
    nothing_runs(&s);
}

// ---------------------------------------------------------------------------------------
// 3. Typed confirmation
// ---------------------------------------------------------------------------------------

fn type_text(s: &mut Shell, text: &str) {
    for c in text.chars() {
        s.send(Event::ConfirmChar(c));
    }
}

fn typed_text(s: &Shell) -> String {
    s.view().confirm.and_then(|c| c.typed).expect("typed").text
}

#[test]
fn a_public_machine_asks_for_its_name_and_a_y_does_not_confirm() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::UpdateTyped);
    assert!(!confirm.question.contains("y/N"), "{}", confirm.question);
    assert!(
        confirm.question.contains("type luna"),
        "{}",
        confirm.question
    );
    assert_eq!(confirm.typed.as_ref().unwrap().expected, "luna");
    // `y` is a letter here, not an answer.
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert_eq!(
        s.view().confirm.expect("still open").kind,
        ConfirmKind::UpdateTyped
    );
    s.send(Event::ConfirmChar('y'));
    s.send(Event::ConfirmSubmit);
    nothing_runs(&s);
}

#[test]
fn typed_confirmation_accepts_only_the_exact_name() {
    // The wrong name, prefixes, other cases, the name with something after it: none runs.
    for wrong in [
        "", "l", "lun", "Luna", "LUNA", "lunaa", "sol", "luna-", "_luna",
    ] {
        let mut s = ready(public(&["luna"]), FLEET, Some(100));
        dialog(&mut s, clean_preflight());
        type_text(&mut s, wrong);
        s.send(Event::ConfirmSubmit);
        nothing_runs(&s);
        let confirm = s.view().confirm.expect("still open");
        assert_eq!(confirm.kind, ConfirmKind::UpdateTyped, "{wrong:?}");
        assert!(
            confirm.typed.unwrap().mismatch,
            "{wrong:?} is said not to match"
        );
    }
    // The exact name does.
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    type_text(&mut s, "luna");
    s.send(Event::ConfirmSubmit);
    let request = s.take_action();
    assert_eq!(request.operation.machine, "luna");
    assert!(s.view().confirm.is_none());
}

#[test]
fn typing_can_be_corrected_and_only_name_characters_are_taken() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    type_text(&mut s, "lunx");
    s.send(Event::ConfirmBackspace);
    s.send(Event::ConfirmChar('a'));
    assert_eq!(typed_text(&s), "luna");
    // Spaces, control characters, path separators and the like never become part of it.
    for c in [' ', '\n', '\u{1b}', '/', ';', '\t', 'é'] {
        s.send(Event::ConfirmChar(c));
    }
    assert_eq!(typed_text(&s), "luna");
    // Backspace past the start is harmless.
    for _ in 0..10 {
        s.send(Event::ConfirmBackspace);
    }
    assert_eq!(typed_text(&s), "");
    // And what is typed has a limit: a machine name's.
    for _ in 0..200 {
        s.send(Event::ConfirmChar('a'));
    }
    assert_eq!(typed_text(&s).len(), 63);
}

#[test]
fn a_mismatch_flag_clears_when_the_typing_changes() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    type_text(&mut s, "lun");
    s.send(Event::ConfirmSubmit);
    assert!(s.view().confirm.unwrap().typed.unwrap().mismatch);
    s.send(Event::ConfirmChar('a'));
    assert!(!s.view().confirm.unwrap().typed.unwrap().mismatch);
}

#[test]
fn escape_cancels_a_typed_confirmation_even_with_the_right_name_typed() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    type_text(&mut s, "luna");
    s.send(Event::ConfirmAnswer(false));
    nothing_runs(&s);
    assert!(s.view().confirm.is_none());
    // And the typing does not leak into the next dialog.
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.typed.unwrap().text, "");
}

#[test]
fn only_the_machines_marked_public_ask_for_their_name() {
    let mut s = ready(public(&["sol", "ghost"]), FLEET, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update, "luna is not public");
    s.send(Event::ConfirmAnswer(false));
    s.send(Event::MoveDown(1));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::UpdateTyped);
    assert_eq!(confirm.typed.unwrap().expected, "sol");
}

#[test]
fn a_public_machine_with_a_refusal_is_refused_and_not_asked_for_its_name() {
    let mut s = ready(public(&["luna"]), FLEET, Some(1));
    let confirm = dialog(&mut s, closure(4));
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert!(confirm.typed.is_none());
    type_text(&mut s, "luna");
    s.send(Event::ConfirmSubmit);
    nothing_runs(&s);
}

#[test]
fn warnings_and_a_typed_name_go_in_one_dialog() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    let confirm = dialog(&mut s, tree(1, tracking(0)));
    assert_eq!(confirm.kind, ConfirmKind::UpdateTyped);
    assert_eq!(confirm.warnings.len(), 1);
    type_text(&mut s, "luna");
    s.send(Event::ConfirmSubmit);
    let _ = s.take_action();
}

#[test]
fn typed_keys_with_no_dialog_do_nothing() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    s.send(Event::ConfirmChar('l'));
    s.send(Event::ConfirmBackspace);
    s.send(Event::ConfirmSubmit);
    assert!(s.view().confirm.is_none());
    nothing_runs(&s);
    // While the checks are still running there is nothing to type into.
    s.send(Event::UpdateSelected);
    type_text(&mut s, "luna");
    s.send(Event::ConfirmSubmit);
    nothing_runs(&s);
    assert_eq!(s.view().confirm.unwrap().kind, ConfirmKind::Checking);
}

// ---------------------------------------------------------------------------------------
// Decided again at the moment of the answer
// ---------------------------------------------------------------------------------------

/// Reads the list again, with every machine evaluated and probed with `free_gib` free.
fn rescan(s: &mut Shell, json: &str, free_gib: u64) {
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(json)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        s.probe(m, probe_with_store(m, Some(free_gib)));
    }
}

#[test]
fn the_rules_are_applied_again_when_y_is_pressed() {
    // The dialog opens with room to spare ...
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, closure(10));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    // ... the list is read again and now luna's store is nearly full ...
    rescan(&mut s, FLEET, 2);
    // ... and `y` is refused, with the reason.
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert!(
        notice(&s).contains("not updating luna") && notice(&s).contains("/nix/store has"),
        "{}",
        notice(&s)
    );
}

#[test]
fn an_address_that_became_shared_while_the_dialog_was_open_refuses_the_y() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    rescan(&mut s, SHARED, 100);
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert!(notice(&s).contains("192.168.1.11"), "{}", notice(&s));
}

#[test]
fn a_typed_confirmation_is_decided_again_too() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, closure(10));
    type_text(&mut s, "luna");
    rescan(&mut s, FLEET, 2);
    s.send(Event::ConfirmSubmit);
    nothing_runs(&s);
    assert!(notice(&s).contains("not updating luna"), "{}", notice(&s));
}

#[test]
fn act_mode_off_while_the_dialog_is_open_means_no_update() {
    let mut s = ready(config(), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
}

#[test]
fn turning_act_mode_off_while_checking_cancels_them() {
    let mut s = ready(config(), FLEET, Some(100));
    s.send(Event::UpdateSelected);
    let id = s.take_preflight().operation.id;
    s.send(Event::ToggleAct);
    assert!(s.cancels.contains(&id));
    assert!(s.view().confirm.is_none());
}

// ---------------------------------------------------------------------------------------
// Each stage is a question of its own (PR #24's "answered only after it was drawn")
// ---------------------------------------------------------------------------------------

#[test]
fn the_checking_dialog_the_refusal_and_the_typed_question_each_have_their_own_serial() {
    for (config, free, check, kind) in [
        (config(), Some(100), clean_preflight(), ConfirmKind::Update),
        (config(), Some(1), closure(4), ConfirmKind::Refused),
        (
            public(&["luna"]),
            Some(100),
            clean_preflight(),
            ConfirmKind::UpdateTyped,
        ),
        (
            config(),
            Some(100),
            tree(1, tracking(0)),
            ConfirmKind::Update,
        ),
    ] {
        let mut s = ready(config, FLEET, free);
        s.send(Event::UpdateSelected);
        let checking = s.view().confirm.expect("checking");
        assert_eq!(checking.kind, ConfirmKind::Checking);
        answer(&mut s, check);
        let shown = s.view().confirm.expect("the dialog");
        assert_eq!(shown.kind, kind);
        assert_ne!(
            shown.serial, checking.serial,
            "{kind:?}: a frame that drew the checking dialog is not the one that drew this"
        );
        // Typing into the dialog does not make it a new question.
        s.send(Event::ConfirmChar('l'));
        assert_eq!(s.view().confirm.expect("still open").serial, shown.serial);
    }
}

#[test]
fn a_refresh_that_changes_the_pending_system_while_the_checks_run_runs_them_again() {
    let mut s = ready(config(), FLEET, Some(100));
    s.send(Event::UpdateSelected);
    let first = s.take_preflight();
    let before = s.view().confirm.expect("checking").serial;
    // luna evaluates to another system (same name, another hash) while the shell looks.
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        let out = if m == "luna" {
            path('c', "nixos-system-luna-new")
        } else {
            new(m)
        };
        s.eval_item(&mut eval, m, evaluated(m, &out));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    let mut first = first;
    s.resolve(&mut first, clean_preflight());
    // The answer was about the old closure: nothing is shown from it, and the checks run
    // again for the new one.
    let confirm = s.view().confirm.expect("checking again");
    assert_eq!(confirm.kind, ConfirmKind::Checking);
    assert_ne!(confirm.serial, before);
    assert!(notice(&s).contains("changed"), "{}", notice(&s));
    let second = s.take_preflight();
    assert_eq!(
        second.operation.store_path.as_str(),
        path('c', "nixos-system-luna-new")
    );
    nothing_runs(&s);
}

#[test]
fn a_refused_dialog_after_a_changed_system_is_asked_again_before_y_counts() {
    // The dialog is open (a warning, not a refusal), then the pending system changes: `y`
    // deploys nothing and the checks start over.
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, tree(1, tracking(0)));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        let out = if m == "luna" {
            path('c', "nixos-system-luna-new")
        } else {
            new(m)
        };
        s.eval_item(&mut eval, m, evaluated(m, &out));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    let again = s.view().confirm.expect("asked again");
    assert_eq!(again.kind, ConfirmKind::Checking);
    assert_ne!(again.serial, confirm.serial);
}

// ---------------------------------------------------------------------------------------
// Fix round 1: pending is not unknown, and every rule is decided again at `y`
// ---------------------------------------------------------------------------------------

/// The reviewer's R1. luna's store has 2 GiB free and the closure is 4 GiB: refused. Then a
/// refresh whose evaluation is back (an eval-cache hit) and whose probes are not: `u` and `y`
/// must not deploy.
#[test]
fn r1_a_disk_refusal_survives_a_refresh_whose_probes_are_still_out() {
    let mut s = ready(config(), FLEET, Some(2));
    let confirm = dialog(&mut s, closure(4));
    assert_eq!(confirm.kind, ConfirmKind::Refused, "precondition");
    s.send(Event::ConfirmAnswer(false));
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    // `u` is refused at once: luna is being probed.
    s.send(Event::UpdateSelected);
    assert!(notice(&s).contains("still probing luna"), "{}", notice(&s));
    assert!(s.preflights.is_empty() && s.view().confirm.is_none());
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
}

/// Unknown because the probe answered without the field stays a warning (the decision of the
/// first round); pending is what is refused.
#[test]
fn a_probe_that_answered_without_the_field_is_still_only_a_warning() {
    let mut s = ready(config(), FLEET, None);
    let confirm = dialog(&mut s, closure(4));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert!(
        confirm.warnings[0].contains("not known"),
        "{:?}",
        confirm.warnings
    );
}

#[test]
fn u_right_after_start_up_before_the_first_probe_is_refused() {
    let mut s = Shell::new();
    s.send(Event::Start(config()));
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    s.send(Event::UpdateSelected);
    assert!(notice(&s).contains("still probing luna"), "{}", notice(&s));
    assert!(s.preflights.is_empty());
    // The moment the probe is in, `u` is asked.
    s.probe("luna", probe_with_store("luna", Some(100)));
    s.send(Event::UpdateSelected);
    assert_eq!(s.view().confirm.expect("asked").kind, ConfirmKind::Checking);
}

/// The dialog is open (a clear one), then a refresh: until the probe is back the `y` is
/// refused, whatever the dialog said, and it is refused for the right reason.
#[test]
fn y_after_a_refresh_whose_probe_is_still_out_is_refused() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, closure(4));
    assert_eq!(confirm.kind, ConfirmKind::Update);
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert!(notice(&s).contains("still probing luna"), "{}", notice(&s));
}

const SOL_BY_NAME: &str = r#"{
  "luna":  {"deploy": {"targetHost": "root@192.168.1.11"}},
  "sol":   {"deploy": {"targetHost": "root@sol.lab.example"}},
  "terra": {"deploy": {"targetHost": "root@192.168.1.10"}}
}"#;

/// The reviewer's R2. luna is an IP literal, sol's hostname resolves to luna's address: refused
/// once resolved. After a refresh, before resolution answers, `u` and `y` must not deploy.
#[test]
fn r2_a_shared_address_refusal_survives_a_refresh_whose_resolution_is_still_out() {
    let luna: IpAddr = "192.168.1.11".parse().unwrap();
    let mut s = ready(config(), SOL_BY_NAME, Some(100));
    let mut resolve = s.resolves.remove(0);
    s.resolve(
        &mut resolve,
        std::iter::once(("sol".to_owned(), vec![luna])).collect(),
    );
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Refused, "precondition");
    s.send(Event::ConfirmAnswer(false));
    rescan(&mut s, SOL_BY_NAME, 100); // resolution is asked again and not answered
    assert_eq!(s.resolves.len(), 1);
    s.send(Event::UpdateSelected);
    assert!(
        notice(&s).contains("still resolving addresses"),
        "{}",
        notice(&s)
    );
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
}

/// A dialog open, a refresh, resolution still out: `y` is refused.
#[test]
fn y_after_a_refresh_whose_resolution_is_still_out_is_refused() {
    let mut s = ready(config(), SOL_BY_NAME, Some(100));
    let mut resolve = s.resolves.remove(0);
    s.resolve(
        &mut resolve,
        std::iter::once(("sol".to_owned(), vec!["192.168.1.12".parse().unwrap()])).collect(),
    );
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update, "precondition");
    rescan(&mut s, SOL_BY_NAME, 100);
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert!(
        notice(&s).contains("still resolving addresses"),
        "{}",
        notice(&s)
    );
}

/// The reviewer's R3. A y/N dialog is open; the config then marks luna public (`Start` again,
/// which the core's contract allows). `y` must not deploy: it asks again, as a typed one.
#[test]
fn r3_a_machine_made_public_while_its_question_is_open_is_asked_again_as_typed() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    s.send(Event::Start(public(&["luna"])));
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        s.probe(m, probe_with_store(m, Some(100)));
    }
    // `Start` replaced the configuration, act mode stays (the model keeps it).
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    let again = s.view().confirm.expect("asked again");
    assert_eq!(again.kind, ConfirmKind::UpdateTyped);
    assert_ne!(again.serial, confirm.serial, "a new question, to be drawn");
    assert!(notice(&s).contains("typing its name"), "{}", notice(&s));
    type_text(&mut s, "luna");
    s.send(Event::ConfirmSubmit);
    assert_eq!(s.take_action().operation.machine, "luna");
}

/// A typed confirmation that was given stays given: the name typed is enough, and nothing asks
/// a third time.
#[test]
fn the_typed_name_satisfies_a_typed_requirement_that_is_still_in_force() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    type_text(&mut s, "luna");
    s.send(Event::ConfirmSubmit);
    assert_eq!(s.take_action().operation.machine, "luna");
}

/// "Every rule at `y`": a warning that was not in the dialog is a new dialog, not a deploy.
#[test]
fn a_warning_that_appeared_while_the_dialog_was_open_asks_again() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.warnings, Vec::<String>::new());
    // The probe of the next scan did not report /nix/store: free space becomes unknown.
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        s.probe(m, probe_with_store(m, None));
    }
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    let again = s.view().confirm.expect("asked again");
    assert_eq!(again.kind, ConfirmKind::Update);
    assert_eq!(again.warnings.len(), 1, "{:?}", again.warnings);
    assert_ne!(again.serial, confirm.serial);
    assert!(notice(&s).contains("changed"), "{}", notice(&s));
    // A yes to the dialog as now drawn deploys.
    let _ = proceeds(&mut s);
}

/// A machine whose target was refused (an option-looking target) has no address to collide with
/// and never will, so it is not "another machine with no known address" (the reviewer's M7).
/// (A machine with no `targetHost` at all is not this case: its own name is its hostname.)
#[test]
fn a_machine_with_no_target_is_not_counted_as_unknown() {
    let json = r#"{
      "luna":  {"deploy": {"targetHost": "root@192.168.1.11"}},
      "sol":   {"deploy": {"targetHost": "-oProxyCommand=evil"}},
      "terra": {"deploy": {"targetHost": "root@192.168.1.10"}}
    }"#;
    let mut s = Shell::new();
    s.start_with(config(), json);
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        s.eval_item(&mut eval, m, evaluated(m, &new(m)));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    // sol's target was refused, so it is not probed.
    for m in ["luna", "terra"] {
        s.probe(m, probe_with_store(m, None));
    }
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    // One warning: luna's own free space is unknown (the probes reported none). Not a second one
    // about sol.
    assert_eq!(confirm.warnings.len(), 1, "{:?}", confirm.warnings);
    assert!(
        !confirm.warnings[0].contains("other machines"),
        "{:?}",
        confirm.warnings
    );
}

/// Backspace clears "that is not luna" (the reviewer's M4).
#[test]
fn backspace_clears_the_mismatch_line() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    type_text(&mut s, "lun");
    s.send(Event::ConfirmSubmit);
    assert!(s.view().confirm.unwrap().typed.unwrap().mismatch);
    s.send(Event::ConfirmBackspace);
    assert!(!s.view().confirm.unwrap().typed.unwrap().mismatch);
}

// ---------------------------------------------------------------------------------------
// Finding 4: the disk check counts what the machine does not have yet
// ---------------------------------------------------------------------------------------

fn store_path(c: char, name: &str) -> auspex_domain::observe::StorePath {
    auspex_domain::observe::StorePath::new(path(c, name)).unwrap()
}

/// luna runs `now` and evaluates to `next`; its store has `free_gib` free. With the package
/// preview on, its own closure (`now`, zlib, a huge library) and the pending one (`next`, zlib,
/// a small tool) come back from the preview; without it, only the checks' own answer is there.
fn luna_with_a_preview(preview: bool, free_gib: u64) -> Shell {
    let config = Config {
        flake: "/fleet".to_owned(),
        allow_act: true,
        local_system: "x86_64-linux".to_owned(),
        preflight_timeout_seconds: 90,
        binary_cache: preview,
        diff: preview.then_some(auspex_core::DiffConfig {
            timeout_seconds: 90,
        }),
        ..Config::default()
    };
    let now = path('0', "nixos-system-now");
    let mut s = Shell::new();
    s.start_with(config, FLEET);
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        let out = if m == "luna" {
            path('1', "nixos-system-next")
        } else {
            new(m)
        };
        s.eval_item(&mut eval, m, evaluated(m, &out));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        let extra = format!(
            "store_disk_kb={} {} {}\n",
            1000 * GIB_KIB,
            (1000 - free_gib) * GIB_KIB,
            free_gib * GIB_KIB
        );
        s.probe(m, observed(&now, &extra));
    }
    if preview {
        let mut cache = s.take_cache("luna");
        s.resolve(&mut cache, auspex_domain::status::NarCache::Cached);
        let mut deployed = s.take_deployed("luna");
        s.resolve(
            &mut deployed,
            Ok(auspex_domain::closure::ClosurePaths {
                paths: vec![
                    store_path('0', "nixos-system-now"),
                    store_path('c', "zlib-1.3.1"),
                    store_path('g', "huge-library-1.0"),
                ],
                dropped: 0,
            }),
        );
    }
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    s
}

/// What the checks sized: the system, zlib and the huge library (both of which luna has), and
/// a small new tool.
fn sized_closure() -> PreflightAnswer {
    PreflightAnswer {
        closure: Ok(vec![
            (store_path('1', "nixos-system-next"), 100 << 20),
            (store_path('c', "zlib-1.3.1"), 1 << 30),
            (store_path('g', "huge-library-1.0"), 5 << 30),
            (store_path('f', "small-tool-1.0"), 50 << 20),
        ]),
        ..clean_preflight()
    }
}

#[test]
fn with_the_machines_own_closure_known_only_the_paths_it_lacks_are_counted() {
    // 6.15 GiB closure in all, 150 MiB of it new; the store has 3 GiB free.
    let mut s = luna_with_a_preview(true, 3);
    let confirm = dialog(&mut s, sized_closure());
    assert_eq!(confirm.kind, ConfirmKind::Update, "{:?}", confirm.refusals);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    let _ = proceeds(&mut s);
}

#[test]
fn without_it_the_whole_closure_is_counted_and_the_dialog_says_so() {
    let mut s = luna_with_a_preview(false, 3);
    let confirm = dialog(&mut s, sized_closure());
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert!(
        confirm.refusals[0].contains("the whole closure's 6.1 GiB")
            && confirm.refusals[0]
                .contains("which of its paths the machine has already is not known"),
        "{:?}",
        confirm.refusals
    );
}

#[test]
fn the_refusal_names_the_paths_it_counted_when_what_is_new_is_still_too_big() {
    // The machine has the huge library, but the new part (150 MiB) does not fit in 1 GiB free
    // once the 1 GiB margin is added.
    let mut s = luna_with_a_preview(true, 1);
    let confirm = dialog(&mut s, sized_closure());
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert!(
        confirm.refusals[0]
            .contains("the 150.0 MiB of the 2 of the closure's 4 paths it does not have yet"),
        "{:?}",
        confirm.refusals
    );
}

#[test]
fn a_refresh_forgets_the_machines_closure_until_the_preview_fetches_it_again() {
    let mut s = luna_with_a_preview(true, 3);
    s.send(Event::ConfirmAnswer(false));
    s.send(Event::Refresh);
    let mut list = s.take_inventory();
    s.resolve(&mut list, Ok(inventory(FLEET)));
    let mut eval = s.take_eval();
    for m in ["luna", "sol", "terra"] {
        let out = if m == "luna" {
            path('1', "nixos-system-next")
        } else {
            new(m)
        };
        s.eval_item(&mut eval, m, evaluated(m, &out));
    }
    s.resolve(&mut eval, EvalAnswer::Finished { error: None });
    for m in ["luna", "sol", "terra"] {
        s.probe(m, probe_with_store(m, Some(3)));
    }
    // No preview answer yet: the whole closure again, and so a refusal at 3 GiB.
    let confirm = dialog(&mut s, sized_closure());
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert!(
        confirm.refusals[0].contains("the whole closure's"),
        "{:?}",
        confirm.refusals
    );
}

// ---------------------------------------------------------------------------------------
// PR #24 round 2: a yes while a refresh is running is refused, on these dialogs too
// ---------------------------------------------------------------------------------------

#[test]
fn a_yes_while_a_refresh_is_running_is_refused_on_the_checks_dialog_too() {
    let mut s = ready(config(), FLEET, Some(100));
    let confirm = dialog(&mut s, clean_preflight());
    assert_eq!(confirm.kind, ConfirmKind::Update);
    s.send(Event::Refresh);
    let _list = s.take_inventory();
    s.send(Event::ConfirmAnswer(true));
    nothing_runs(&s);
    assert_eq!(
        notice(&s),
        "a refresh is running; ask again when it has finished"
    );
    assert!(s.view().confirm.is_none(), "the question is over");
}

#[test]
fn a_typed_name_while_a_refresh_is_running_is_refused_too() {
    let mut s = ready(public(&["luna"]), FLEET, Some(100));
    dialog(&mut s, clean_preflight());
    type_text(&mut s, "luna");
    s.send(Event::Refresh);
    let _list = s.take_inventory();
    s.send(Event::ConfirmSubmit);
    nothing_runs(&s);
    assert_eq!(
        notice(&s),
        "a refresh is running; ask again when it has finished"
    );
}
