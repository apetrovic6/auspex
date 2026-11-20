//! What the operator sees and presses for `U`: the key, the plan's question (snapshots at 40 and
//! 120 columns), a plan that does not fit not being asked at all, and the status bar and the Log
//! tab while a roll runs. Driven through the keymap against the scripted demo with time paused.
//!
//! The demo's pending machines are `luna`, `sol` and `cypramundi` (public, so left alone by `U`).

mod common;

use std::sync::Arc;

use auspex_core::{ConfirmKind, Mode, RollState, RollStatus};
use auspex_tui::keymap::Keymap;
use auspex_tui::shell::Shell;
use auspex_tui::ui::Ui;
use common::{elapse, expect_file, frame, render, render_with, settled_demo, type_keys_seen};

/// Waits (time is paused, so this is instant) until the plan is the open question: first the
/// shell looks at the fleet repository (and the cluster, when a group uses one), then the question
/// opens; then a frame draws it and the dwell passes, so a key counts for it.
async fn until_plan(shell: &Arc<Shell>, ui: &mut Ui, km: &Keymap) {
    while shell
        .view()
        .confirm
        .as_ref()
        .is_none_or(|c| c.kind != ConfirmKind::UpdateAll)
    {
        shell.changed().await;
    }
    frame(shell, ui, km);
    elapse(auspex_tui::ui::QUESTION_DWELL);
}

#[tokio::test(start_paused = true)]
async fn in_observe_mode_u_says_act_mode_is_off() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "U");
    assert_eq!(ui.input.message.as_deref(), Some("act mode is off (A)"));
    assert!(shell.view().confirm.is_none());
    assert!(shell.view().roll.is_none());
}

#[tokio::test(start_paused = true)]
async fn the_plan_at_120_and_40_columns() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay");
    assert_eq!(shell.view().mode, Mode::Act);
    type_keys_seen(&shell, &mut ui, &km, "U");
    until_plan(&shell, &mut ui, &km).await;
    let confirm = shell.view().confirm.expect("the plan");
    assert_eq!(confirm.kind, ConfirmKind::UpdateAll);
    assert_eq!(confirm.question, "update 2 machines as listed above? y/N");

    let wide = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    for line in [
        "update 2 machines:",
        "in parallel, at most 2 at a time: luna, sol",
        "skipped cypramundi: marked public: its name has to be typed, so update it",
        "update 2 machines as listed above? y/N",
        "y confirms; any other key cancels",
    ] {
        assert!(wide.contains(line), "{line}\n{wide}");
    }
    expect_file("snapshots", "confirm_update_all_120.txt", &wide);

    let narrow = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 34);
    assert!(
        narrow.contains("y/N") && narrow.contains("skipped cypramundi"),
        "{narrow}"
    );
    expect_file("snapshots", "confirm_update_all_40.txt", &narrow);
}

/// What `y` agrees to is the plan, so a plan that does not fit is not asked: the box says the
/// terminal is too small, and the question is not counted as drawn, so no key can answer it.
#[tokio::test(start_paused = true)]
async fn a_plan_that_does_not_fit_is_not_asked() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay");
    type_keys_seen(&shell, &mut ui, &km, "U");
    until_plan(&shell, &mut ui, &km).await;
    let view = shell.view();
    let mut small = Ui::default();
    let frame = render_with(&view, &mut small, &km, &[], shell.output(), 40, 12);
    assert!(frame.contains("too small to ask"), "{frame}");
    assert!(
        !frame.contains("update 2 machines:"),
        "no part of the plan is shown:\n{frame}"
    );
    assert!(small.confirm_seen.is_none(), "not drawn, so not answerable");
}

#[tokio::test(start_paused = true)]
async fn the_status_bar_and_the_log_follow_a_roll() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay");
    type_keys_seen(&shell, &mut ui, &km, "U");
    until_plan(&shell, &mut ui, &km).await;
    type_keys_seen(&shell, &mut ui, &km, "y");
    // The `y` takes the fleet's lock first (a task of the shell), and then the roll begins.
    while shell.view().roll.is_none() {
        shell.changed().await;
    }
    // Running: the bar names who is doing what.
    let running = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 140, 30);
    let status = running.lines().last().unwrap();
    assert!(status.contains("update all: 0/2 over"), "{status}");
    // Until it ends (time is paused: the scripted updates pass instantly).
    while shell
        .view()
        .roll
        .as_ref()
        .is_none_or(|r| r.status != RollStatus::Ended)
    {
        shell.changed().await;
    }
    let done = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 140, 30);
    let status = done.lines().last().unwrap();
    assert!(status.contains("update all: ended, 2 done"), "{status}");
    for m in ["luna", "sol"] {
        let state = shell
            .view()
            .roll
            .unwrap()
            .machines
            .into_iter()
            .find(|x| x.name == m)
            .unwrap()
            .state;
        assert_eq!(state, RollState::Done, "{m}");
    }
    // The Log tab (L): the roll's run first, then a run for each machine.
    type_keys_seen(&shell, &mut ui, &km, "L");
    let tab = render_with(&shell.view(), &mut ui, &km, &[], shell.output(), 100, 40);
    assert!(tab.contains("update all"), "{tab}");
    assert!(
        tab.contains("luna: done") || tab.contains("update luna"),
        "{tab}"
    );
}
