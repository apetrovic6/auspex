//! What the operator sees and presses around act mode, `u`, `b` and the Log tab, driven
//! through the keymap against the scripted demo with time paused: observe mode saying so,
//! the confirmations, the status bar, the Log tab following the tail and letting go of it,
//! stdout and stderr in different colours, and the snapshots at 40 and 120 columns.
//!
//! Real processes (fake `clan` and `nix`), the action log and quitting are in `actions.rs`.

mod common;

use std::collections::BTreeMap;

use auspex_core::{ActionStatus, ConfirmKind, Event, Mode};
use auspex_tui::keymap::Keymap;
use auspex_tui::output::{KEEP_LINES, Output, Stream};
use auspex_tui::theme::{Role, Theme};
use auspex_tui::ui::Ui;
use common::{
    draw, expect_file, paste, render, render_with, settled_demo, type_keys, type_keys_seen,
    until_asked,
};

/// Waits (time is paused, so this is instant) until the update has been requested: after the
/// `y` the shell reads the freeze state before the core asks for it to run.
async fn until_action_starts(shell: &std::sync::Arc<auspex_tui::shell::Shell>) {
    while shell.view().action.is_none() {
        shell.changed().await;
    }
}

/// Waits (time is paused, so this is instant) until the action is over.
async fn until_action_ends(shell: &std::sync::Arc<auspex_tui::shell::Shell>) {
    while shell
        .view()
        .action
        .as_ref()
        .is_none_or(auspex_core::ActionView::active)
    {
        shell.changed().await;
    }
}

fn remapped(act: &str) -> Keymap {
    let mut warnings = Vec::new();
    Keymap::with_overrides(
        &BTreeMap::from([("act".to_owned(), vec![act.to_owned()])]),
        &mut warnings,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------
// Observe mode, the keys
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn in_observe_mode_u_says_act_mode_is_off_and_nothing_else_happens() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "u");
    assert_eq!(ui.input.message.as_deref(), Some("act mode is off (A)"));
    let view = shell.view();
    assert!(view.confirm.is_none(), "no question");
    assert!(view.action.is_none(), "no action");
    assert_eq!(view.mode, Mode::Observe);
    let frame = render(&view, &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(frame.contains("act mode is off (A)"), "{frame}");
}

#[tokio::test(start_paused = true)]
async fn the_observe_message_names_the_key_the_operator_bound() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = remapped("X");
    type_keys_seen(&shell, &mut ui, &km, "u");
    assert_eq!(ui.input.message.as_deref(), Some("act mode is off (X)"));
    // And the remapped key is the one that asks.
    type_keys_seen(&shell, &mut ui, &km, "X");
    assert!(shell.view().confirm.is_some());
}

#[tokio::test(start_paused = true)]
async fn the_keys_do_nothing_but_answer_while_a_question_is_open() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "A");
    assert!(shell.view().confirm.is_some());
    let selected = shell.view().selected;
    // `j` moves nothing and answers no; it is not a yes.
    type_keys_seen(&shell, &mut ui, &km, "j");
    assert_eq!(shell.view().selected, selected, "the list did not move");
    assert!(shell.view().confirm.is_none());
    assert_eq!(shell.view().mode, Mode::Observe, "any key but y is a no");

    type_keys_seen(&shell, &mut ui, &km, "A");
    type_keys_seen(&shell, &mut ui, &km, "\x1b");
    assert_eq!(shell.view().mode, Mode::Observe, "Esc is a no");
    type_keys_seen(&shell, &mut ui, &km, "A");
    type_keys_seen(&shell, &mut ui, &km, "y");
    assert_eq!(shell.view().mode, Mode::Act);
    type_keys_seen(&shell, &mut ui, &km, "A");
    assert_eq!(shell.view().mode, Mode::Observe, "A turns it off at once");
}

// ---------------------------------------------------------------------------------------
// Snapshots: the status bar, the confirmations
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn act_mode_in_the_status_bar_at_120_columns() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay/luna\n");
    assert_eq!(shell.view().mode, Mode::Act);
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    let status = frame.lines().last().unwrap();
    assert!(status.starts_with("ACT │ "), "{status}");
    expect_file("snapshots", "act_mode_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn the_act_word_is_in_the_warning_colour() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay");
    let out = Output::default();
    let buf = draw(&shell.view(), &mut ui, &km, &[], &out, 120, 30);
    let theme = Theme::Ansi;
    let warning = theme.style(Role::Warning);
    let status_y = 29;
    for (x, c) in "ACT".chars().enumerate() {
        let cell = &buf[(u16::try_from(x).unwrap(), status_y)];
        assert_eq!(cell.symbol(), c.to_string());
        assert_eq!(cell.fg, warning.fg.unwrap(), "{c}");
    }
    // In observe mode the same cells are the ordinary title colour.
    type_keys_seen(&shell, &mut ui, &km, "A");
    let buf = draw(&shell.view(), &mut ui, &km, &[], &out, 120, 30);
    assert_eq!(buf[(0, status_y)].symbol(), "O");
    assert_ne!(buf[(0, status_y)].fg, warning.fg.unwrap());
}

#[tokio::test(start_paused = true)]
async fn the_confirmations_at_40_and_120_columns() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();

    type_keys_seen(&shell, &mut ui, &km, "A");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(frame.contains("turn on act mode?"), "{frame}");
    expect_file("snapshots", "confirm_act_120.txt", &frame);
    type_keys_seen(&shell, &mut ui, &km, "y");

    type_keys_seen(&shell, &mut ui, &km, "/luna\nu");
    until_asked(&shell, &mut ui, &km).await;
    let q = shell.view().confirm.unwrap().question;
    assert!(
        q.starts_with("update luna to g1j4m7qa nixos-system-luna-") && q.ends_with("? y/N"),
        "{q}"
    );
    let wide = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(
        wide.contains(&q),
        "the whole question is on screen:\n{wide}"
    );
    expect_file("snapshots", "confirm_update_120.txt", &wide);
    let narrow = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 24);
    // At 40 columns the question wraps; "y/N" is still there, and so is the machine.
    assert!(narrow.contains("y/N"), "{narrow}");
    assert!(narrow.contains("luna"), "{narrow}");
    expect_file("snapshots", "confirm_update_40.txt", &narrow);
}

// ---------------------------------------------------------------------------------------
// An update through the demo: the status bar and the Log tab
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_update_streams_into_the_log_tab_and_the_status_bar_says_how_it_ended() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay/luna\nu");
    until_asked(&shell, &mut ui, &km).await;
    type_keys_seen(&shell, &mut ui, &km, "y");
    until_action_starts(&shell).await;
    // Running: the bar names it.
    let running = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(
        running
            .lines()
            .last()
            .unwrap()
            .contains("update luna: running"),
        "{running}"
    );
    until_action_ends(&shell).await;
    let view = shell.view();
    let ended = render_with(
        &view,
        &mut ui,
        &km,
        &shell.log.lines(),
        shell.output(),
        120,
        30,
    );
    assert!(
        ended
            .lines()
            .last()
            .unwrap()
            .contains("update luna: succeeded"),
        "{ended}"
    );
    assert!(matches!(
        view.action.unwrap().status,
        ActionStatus::Ended(_)
    ));

    type_keys_seen(&shell, &mut ui, &km, "L");
    let wide = render_with(
        &shell.view(),
        &mut ui,
        &km,
        &shell.log.lines(),
        shell.output(),
        120,
        30,
    );
    assert!(wide.contains("── update luna ──"), "{wide}");
    assert!(
        wide.contains(
            "building on this machine (deploy.buildHost = localhost), then copying to root@192.168.1.11"
        ),
        "{wide}"
    );
    assert!(
        wide.contains("activating the new configuration..."),
        "{wide}"
    );
    assert!(wide.contains("── succeeded, exit 0 ──"), "{wide}");
    expect_file("snapshots", "log_tab_120.txt", &wide);
    let narrow = render_with(
        &shell.view(),
        &mut ui,
        &km,
        &shell.log.lines(),
        shell.output(),
        40,
        24,
    );
    expect_file("snapshots", "log_tab_40.txt", &narrow);

    // Tab: the adapters' notes instead. Esc closes the tab.
    type_keys_seen(&shell, &mut ui, &km, "\t");
    let notes = render_with(
        &shell.view(),
        &mut ui,
        &km,
        &shell.log.lines(),
        shell.output(),
        120,
        30,
    );
    assert!(notes.contains("log: notes"), "{notes}");
    type_keys_seen(&shell, &mut ui, &km, "\x1b");
    assert!(!ui.input.log_open);
}

#[tokio::test(start_paused = true)]
async fn a_failed_update_says_where_it_failed_at_ssh_and_the_fix() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    // mars is the demo's dead host: its scripted update fails with ssh's own line.
    type_keys_seen(&shell, &mut ui, &km, "Ay/mars\nu");
    until_asked(&shell, &mut ui, &km).await;
    type_keys_seen(&shell, &mut ui, &km, "y");
    until_action_ends(&shell).await;
    let view = shell.view();
    let ActionStatus::Ended(end) = &view.action.as_ref().unwrap().status else {
        panic!("ended");
    };
    assert_eq!(end.exit_code, Some(255));
    let why = end.explanation.as_deref().expect("an explanation");
    assert!(why.contains("failed at ssh"), "{why}");
    let frame = render_with(&view, &mut ui, &km, &[], shell.output(), 200, 30);
    let status = frame.lines().last().unwrap();
    assert!(
        status.contains("update mars: failed (exit 255)"),
        "{status}"
    );
    assert!(status.contains("failed at ssh"), "{status}");
}

#[tokio::test(start_paused = true)]
async fn b_refuses_the_darwin_host_and_builds_the_rest_in_observe_mode() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "/genetor\nb");
    let view = shell.view();
    assert!(view.action.is_none());
    assert!(
        view.notice
            .as_deref()
            .unwrap()
            .contains("not building another system's derivations"),
        "{:?}",
        view.notice
    );
    type_keys_seen(&shell, &mut ui, &km, "/luna\nb");
    assert!(
        shell.view().action.is_some(),
        "luna builds, without act mode"
    );
    until_action_ends(&shell).await;
    assert_eq!(shell.view().mode, Mode::Observe);
}

// ---------------------------------------------------------------------------------------
// The Log tab: following the tail, colours
// ---------------------------------------------------------------------------------------

fn lines_out(n: usize) -> Output {
    let out = Output::default();
    out.begin("update luna");
    for i in 0..n {
        out.push(Stream::Stdout, &format!("line {i}"));
    }
    out
}

/// The output rows on screen (not the borders, whose counter changes as output grows).
fn tab_rows(frame: &str) -> Vec<String> {
    frame
        .lines()
        .filter_map(|l| {
            let t = l.trim_matches(|c| c == '│' || c == ' ');
            (t.starts_with("line ") || t.starts_with("more ")).then(|| t.to_owned())
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_log_tab_follows_the_tail_until_the_operator_scrolls_up() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    let out = lines_out(100);
    type_keys(&shell, &mut ui, &km, "L");
    let frame = |ui: &mut Ui, out: &Output| render_with(&shell.view(), ui, &km, &[], out, 60, 14);

    // Following: the last line is on screen.
    let f = frame(&mut ui, &out);
    assert!(f.contains("line 99"), "{f}");
    assert!(f.contains("following"), "{f}");

    // New output keeps it on the tail.
    for i in 100..110 {
        out.push(Stream::Stdout, &format!("line {i}"));
    }
    let f = frame(&mut ui, &out);
    assert!(f.contains("line 109"), "still following: {f}");

    // Scrolling up lets go.
    type_keys(&shell, &mut ui, &km, "kkk");
    let held = frame(&mut ui, &out);
    assert!(!held.contains("following"), "{held}");
    assert!(!held.contains("line 109"), "{held}");
    let held_rows = tab_rows(&held);

    // More output arrives; the view stays where the operator put it.
    for i in 110..160 {
        out.push(Stream::Stdout, &format!("line {i}"));
    }
    let still = frame(&mut ui, &out);
    assert_eq!(tab_rows(&still), held_rows, "it did not move:\n{still}");
    assert!(!still.contains("line 159"));

    // `G` follows again.
    type_keys(&shell, &mut ui, &km, "G");
    let f = frame(&mut ui, &out);
    assert!(f.contains("line 159") && f.contains("following"), "{f}");

    // Scrolling down to the end of a held view follows again, too.
    type_keys(&shell, &mut ui, &km, "kk");
    assert!(!frame(&mut ui, &out).contains("following"));
    type_keys(&shell, &mut ui, &km, "jjjjjjjjjj");
    assert!(frame(&mut ui, &out).contains("following"));
}

#[tokio::test(start_paused = true)]
async fn a_held_log_tab_keeps_its_lines_when_old_ones_are_dropped() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    let out = lines_out(KEEP_LINES);
    type_keys(&shell, &mut ui, &km, "L");
    let frame = |ui: &mut Ui, out: &Output| render_with(&shell.view(), ui, &km, &[], out, 60, 14);
    let _ = frame(&mut ui, &out);
    type_keys(&shell, &mut ui, &km, "10k");
    let before = tab_rows(&frame(&mut ui, &out));
    // 25 more lines push 25 off the front; the same text stays on screen.
    for i in 0..25 {
        out.push(Stream::Stdout, &format!("more {i}"));
    }
    let after = tab_rows(&frame(&mut ui, &out));
    assert_eq!(after, before);
}

#[tokio::test(start_paused = true)]
async fn standard_error_is_drawn_in_its_own_colour() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    let out = Output::default();
    out.begin("update luna");
    out.push(Stream::Stdout, "from stdout");
    out.push(Stream::Stderr, "from stderr");
    type_keys(&shell, &mut ui, &km, "L");
    let buf = draw(&shell.view(), &mut ui, &km, &[], &out, 60, 14);
    let theme = Theme::Ansi;
    let find = |needle: &str| {
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if let Some(at) = row.find(needle) {
                let x = u16::try_from(row[..at].chars().count()).unwrap();
                return buf[(x, y)].fg;
            }
        }
        panic!("{needle} not drawn");
    };
    let stderr = find("from stderr");
    let stdout = find("from stdout");
    assert_eq!(stderr, theme.style(Role::Stderr).fg.unwrap());
    assert_ne!(stdout, stderr, "the two streams must look different");
    assert_eq!(
        stdout,
        theme
            .style(Role::Text)
            .fg
            .unwrap_or(ratatui::style::Color::Reset)
    );
}

#[tokio::test(start_paused = true)]
async fn the_log_tab_before_any_action_says_how_to_start_one() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "L");
    let f = render(&shell.view(), &mut ui, &km, &[], 120, 24);
    assert!(f.contains("no action has run yet"), "{f}");
    // Up, down and G with nothing to scroll do not panic or hide the tab.
    type_keys(&shell, &mut ui, &km, "kjGgg");
    assert!(ui.input.log_open);
    shell.send(Event::Quit);
}

#[tokio::test(start_paused = true)]
async fn only_the_terminal_ui_may_enter_act_mode() {
    use auspex_tui::app::{core_config, for_mode};
    use auspex_tui::cli::Mode as Shown;
    use auspex_tui::config::Settings;

    let real = core_config(&Settings::default(), "/fleet".to_owned());
    for config in [real, auspex_tui::demo::config()] {
        assert!(for_mode(config.clone(), Shown::Tui).allow_act);
        assert!(!for_mode(config.clone(), Shown::Plain).allow_act, "--plain");
        assert!(!for_mode(config, Shown::Json).allow_act, "--json");
    }
    // And a core started from the headless configuration cannot be put in act mode.
    let shell = common::started_demo();
    shell.send(Event::Start(for_mode(
        auspex_tui::demo::config(),
        Shown::Plain,
    )));
    shell.send(Event::ToggleAct);
    shell.send(Event::ConfirmAnswer(true));
    assert_eq!(shell.view().mode, Mode::Observe);
    assert!(shell.view().confirm.is_none());
}

// ---------------------------------------------------------------------------------------
// A question is answered only after it has been drawn (review finding 2)
// ---------------------------------------------------------------------------------------

/// Typeahead: `Ayuy` arriving in one burst, with no frame drawn between the keys, answers
/// nothing and deploys nothing. Neither question was ever on screen.
#[tokio::test(start_paused = true)]
async fn a_burst_of_keys_cannot_turn_on_act_mode_and_update() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    type_keys(&shell, &mut ui, &km, "Ayuy");
    let view = shell.view();
    assert!(view.action.is_none(), "nothing was deployed");
    assert_eq!(view.mode, Mode::Observe, "act mode was not answered into");
    assert_eq!(
        view.confirm.map(|c| c.kind),
        Some(ConfirmKind::Act),
        "the first question is still waiting to be seen"
    );
    // Longer bursts, and the same burst with the question's own key repeated.
    for burst in ["Ayuyu", "Ayyyyuyyy", "Ay/luna\nuy"] {
        type_keys(&shell, &mut ui, &km, burst);
        assert!(shell.view().action.is_none(), "{burst:?}");
        assert_eq!(shell.view().mode, Mode::Observe, "{burst:?}");
    }
    assert!(shell.log.lines().iter().all(|l| !l.contains("update luna")));
}

/// The keys before the first frame are discarded, not queued: after the frame that shows the
/// question, the operator's own `y` is the answer, and only it.
#[tokio::test(start_paused = true)]
async fn keys_before_the_frame_are_discarded_and_the_first_key_after_it_answers() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "Ay");
    assert_eq!(shell.view().mode, Mode::Observe, "the y came too early");
    common::frame(&shell, &mut ui, &km);
    common::elapse(auspex_tui::ui::QUESTION_DWELL);
    assert_eq!(shell.view().mode, Mode::Observe, "a frame answers nothing");
    type_keys(&shell, &mut ui, &km, "y");
    assert_eq!(shell.view().mode, Mode::Act);
}

/// A paste is no, never yes and never commands: bracketed paste is on, so the terminal says
/// "this was pasted" and it does not arrive as keystrokes.
#[tokio::test(start_paused = true)]
async fn a_paste_deploys_nothing_and_answers_a_drawn_question_no() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    // With no question open, pasted text is not typed: pasted `Ayuy` starts nothing.
    paste(&shell, &mut ui, &km, "Ayuy");
    assert!(shell.view().confirm.is_none());
    assert_eq!(shell.view().mode, Mode::Observe);
    // Before the question is drawn, a paste is discarded like any key.
    type_keys(&shell, &mut ui, &km, "A");
    paste(&shell, &mut ui, &km, "y");
    assert_eq!(shell.view().mode, Mode::Observe);
    assert!(shell.view().confirm.is_some(), "still waiting to be seen");
    // After it, a paste is a no, even of `y`.
    common::frame(&shell, &mut ui, &km);
    common::elapse(auspex_tui::ui::QUESTION_DWELL);
    paste(&shell, &mut ui, &km, "y");
    assert_eq!(shell.view().mode, Mode::Observe);
    assert!(shell.view().confirm.is_none(), "answered no");

    // The same for an update's question.
    type_keys_seen(&shell, &mut ui, &km, "Ay/luna\nu");
    until_asked(&shell, &mut ui, &km).await;
    assert_eq!(shell.view().mode, Mode::Act);
    assert_eq!(
        shell.view().confirm.map(|c| c.kind),
        Some(ConfirmKind::Update)
    );
    paste(&shell, &mut ui, &km, "y\ny");
    assert!(shell.view().action.is_none(), "nothing was deployed");
    assert!(shell.view().confirm.is_none());
}

/// A pasted search is still a search: the text goes into an open prompt as typed (without a
/// newline, which would submit it).
#[tokio::test(start_paused = true)]
async fn a_paste_into_the_search_prompt_is_typed_without_its_newline() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/");
    paste(&shell, &mut ui, &km, "lu\nna");
    assert_eq!(
        shell.view().search.as_ref().map(|s| s.query.as_str()),
        Some("luna")
    );
}

/// The sequence the brief names: `A`, a frame, `y`, `u`, a frame, `y` deploys.
#[tokio::test(start_paused = true)]
async fn each_question_seen_then_answered_deploys() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "A");
    common::frame(&shell, &mut ui, &km);
    common::elapse(auspex_tui::ui::QUESTION_DWELL);
    type_keys(&shell, &mut ui, &km, "y/luna\nu");
    assert_eq!(shell.view().mode, Mode::Act);
    assert!(shell.view().action.is_none(), "the update is only asked");
    // The `y` typed with the `u` is not in the burst: it would be an answer before the frame.
    // The checks run first; the dialog they open is drawn before `y` counts.
    until_asked(&shell, &mut ui, &km).await;
    type_keys(&shell, &mut ui, &km, "y");
    until_action_starts(&shell).await;
    let action = shell.view().action.expect("the update started");
    assert_eq!(action.machine, "luna");
    until_action_ends(&shell).await;
}

/// Answering one question does not show the next one: a new question needs its own frame.
#[tokio::test(start_paused = true)]
async fn a_second_question_is_not_answered_by_the_first_ones_frame() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "A");
    common::frame(&shell, &mut ui, &km);
    common::elapse(auspex_tui::ui::QUESTION_DWELL);
    // `y` answers act mode, `/luna\nu` asks the update, and the next `y` is typeahead.
    type_keys(&shell, &mut ui, &km, "y/luna\nuy");
    assert!(
        shell.view().action.is_none(),
        "the update's question was never drawn"
    );
    // What was asked is the checking dialog, which the typeahead `y` did not cancel ...
    assert_eq!(
        shell.view().confirm.map(|c| c.kind),
        Some(ConfirmKind::Checking)
    );
    // ... and the dialog it turns into is a question of its own: undrawn, so a `y` is discarded.
    common::until_checked(&shell).await;
    assert_eq!(
        shell.view().confirm.map(|c| c.kind),
        Some(ConfirmKind::Update)
    );
    type_keys(&shell, &mut ui, &km, "y");
    assert!(shell.view().action.is_none(), "the dialog was never drawn");
    assert!(shell.view().confirm.is_some());
}

// ---------------------------------------------------------------------------------------
// A question must be readable before it is answered (re-review findings 1 and 2)
// ---------------------------------------------------------------------------------------

/// A frame at 33 ms is not enough: a `y` typed 80 to 150 ms behind the `u` by habit used to be
/// accepted. The question has to have been on screen for `QUESTION_DWELL`.
#[tokio::test(start_paused = true)]
async fn an_answer_right_behind_the_frame_is_discarded_until_the_dwell_has_passed() {
    use std::time::Duration;

    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "A");
    common::frame(&shell, &mut ui, &km);
    // The habitual `y`, one frame later.
    common::elapse(Duration::from_millis(33));
    type_keys(&shell, &mut ui, &km, "y");
    assert_eq!(shell.view().mode, Mode::Observe, "33 ms is not reading");
    common::elapse(Duration::from_millis(266));
    type_keys(&shell, &mut ui, &km, "y");
    assert_eq!(shell.view().mode, Mode::Observe, "just short of the dwell");
    assert!(
        shell.view().confirm.is_some(),
        "and the question is still open"
    );
    common::elapse(Duration::from_millis(1));
    type_keys(&shell, &mut ui, &km, "y");
    assert_eq!(shell.view().mode, Mode::Act, "the dwell has passed");
}

/// The same for the update's question, the one that deploys.
#[tokio::test(start_paused = true)]
async fn a_habitual_u_y_deploys_nothing_and_the_same_y_a_moment_later_does() {
    use std::time::Duration;

    use auspex_tui::ui::QUESTION_DWELL;
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay/luna\n");
    assert_eq!(shell.view().mode, Mode::Act);
    type_keys(&shell, &mut ui, &km, "u");
    common::frame(&shell, &mut ui, &km);
    common::elapse(Duration::from_millis(100)); // a fast typist's gap
    type_keys(&shell, &mut ui, &km, "y");
    assert!(shell.view().action.is_none(), "nothing was deployed");
    // What `u` asked first is the checks ("checking luna…"): the habitual `y` did not cancel
    // them, it was not on screen long enough to count.
    assert_eq!(
        shell.view().confirm.map(|c| c.kind),
        Some(ConfirmKind::Checking),
        "the question is still waiting to be read"
    );
    // The checks answer and open the dialog, a question of its own, which a `y` right behind
    // its frame does not answer either.
    common::until_checked(&shell).await;
    common::frame(&shell, &mut ui, &km);
    common::elapse(Duration::from_millis(100));
    type_keys(&shell, &mut ui, &km, "y");
    assert!(shell.view().action.is_none(), "nothing was deployed");
    assert_eq!(
        shell.view().confirm.map(|c| c.kind),
        Some(ConfirmKind::Update),
        "the dialog is still waiting to be read"
    );
    common::elapse(QUESTION_DWELL);
    type_keys(&shell, &mut ui, &km, "y");
    until_action_starts(&shell).await;
    assert_eq!(shell.view().action.expect("started").machine, "luna");
    until_action_ends(&shell).await;
}

/// A pane too small to show the whole question is not asking it: the box says so, the question
/// does not count as drawn, and no `y` answers it. Once the pane is large enough, the dwell
/// starts then.
#[tokio::test(start_paused = true)]
async fn a_question_that_did_not_fit_is_not_drawn_and_cannot_be_answered() {
    use auspex_tui::ui::QUESTION_DWELL;
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "Ay/luna\nu");
    common::until_asked(&shell, &mut ui, &km).await;
    // `until_asked` ended with a frame at 120×40 and the dwell: the dialog is readable.
    // Now the pane shrinks, as a resize does: the question is no longer drawn.
    for (w, h) in [(20, 4), (80, 4), (30, 5), (80, 6), (27, 40)] {
        let view = shell.view();
        let frame = common::render_with(&view, &mut ui, &km, &[], shell.output(), w, h);
        assert!(
            ui.confirm_seen.is_none(),
            "{w}x{h} shows no whole question:\n{frame}"
        );
        common::elapse(QUESTION_DWELL * 2);
        type_keys(&shell, &mut ui, &km, "y");
        assert!(shell.view().action.is_none(), "{w}x{h}: nothing deployed");
        assert!(shell.view().confirm.is_some(), "{w}x{h}: still waiting");
    }
    // What the box says in a pane that holds the sentence.
    let small = common::render_with(&shell.view(), &mut ui, &km, &[], shell.output(), 80, 6);
    assert!(small.contains("terminal too small to ask"), "{small}");
    // Large enough again: the dwell starts now, not from before the shrink.
    common::frame(&shell, &mut ui, &km);
    type_keys(&shell, &mut ui, &km, "y");
    assert!(shell.view().action.is_none(), "not yet read");
    common::elapse(QUESTION_DWELL);
    type_keys(&shell, &mut ui, &km, "y");
    until_action_starts(&shell).await;
    assert_eq!(shell.view().action.expect("started").machine, "luna");
    until_action_ends(&shell).await;
}

// ---------------------------------------------------------------------------------------
// The Log tab says where the action builds
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_first_log_line_is_auspexs_own_and_looks_unlike_clans_output() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys_seen(&shell, &mut ui, &km, "/luna\nb");
    until_action_ends(&shell).await;
    let first = shell.output().window(1, 1);
    assert_eq!(first[0].stream, Stream::Note, "{first:?}");
    assert_eq!(first[0].text, "building on this machine");
    let theme = Theme::Ansi;
    assert_ne!(theme.style(Role::Dim), theme.style(Role::Text));
    assert_ne!(theme.style(Role::Dim), theme.style(Role::Stderr));
}
