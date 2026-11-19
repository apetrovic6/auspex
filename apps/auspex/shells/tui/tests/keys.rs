//! Keys to core events: counts, `gg`, the `/` and `:` prompts, Esc's precedence, remapping
//! (the help overlay follows the live keymap) and unbinding.
//!
//! [`Input`] is pure, so most of these assert on the events it returns; the remapping tests
//! also draw the help overlay and run the events through the demo's core.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use auspex_core::{ConfirmKind, Event};
use auspex_tui::input::{Input, Prompt, Screen};
use auspex_tui::keymap::{Key, Keymap};
use auspex_tui::ui::{Ui, help_lines};
use common::{render, settled_demo, type_keys};
use crossterm::event::KeyCode;

/// No events.
const NOTHING: [String; 0] = [];

const SCREEN: Screen = Screen {
    help_open: false,
    page: 20,
    matches: 3,
    listing_for: None,
    rows: 18,
    act: false,
    confirm: None,
};

const fn ctrl(c: char) -> Key {
    Key {
        code: KeyCode::Char(c),
        ctrl: true,
    }
}

const fn esc() -> Key {
    Key::plain(KeyCode::Esc)
}

/// Feeds `keys` and returns every event, as `Debug` text (`Event` has no `PartialEq`).
fn feed(input: &mut Input, keymap: &Keymap, screen: Screen, keys: &[Key]) -> Vec<String> {
    keys.iter()
        .flat_map(|k| input.key(*k, screen, keymap, "prev"))
        .map(|e| format!("{e:?}"))
        .collect()
}

fn chars(s: &str) -> Vec<Key> {
    s.chars().map(Key::char).collect()
}

fn ev(events: &[Event]) -> Vec<String> {
    events.iter().map(|e| format!("{e:?}")).collect()
}

#[test]
fn a_count_multiplies_a_move() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("5j")),
        ev(&[Event::MoveDown(5)])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("12k")),
        ev(&[Event::MoveUp(12)])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("j")),
        ev(&[Event::MoveDown(1)]),
        "the count was used up"
    );
    // A leading 0 is not a count; 0 after a digit is.
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("0j")),
        ev(&[Event::MoveDown(1)])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("10j")),
        ev(&[Event::MoveDown(10)])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("2n")),
        ev(&[Event::SearchNext, Event::SearchNext])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &[Key::char('3'), ctrl('d')]),
        ev(&[Event::MoveDown(30)]),
        "three half pages are one event"
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("999999999j")),
        ev(&[Event::MoveDown(99_999)])
    );
}

#[test]
fn gg_and_g_go_to_the_ends_or_to_a_counted_row() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("g")),
        NOTHING,
        "g alone waits"
    );
    assert_eq!(i.pending, chars("g"));
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("g")),
        ev(&[Event::MoveTop])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("G")),
        ev(&[Event::MoveBottom])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("5G")),
        ev(&[Event::MoveTop, Event::MoveDown(4)])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("3gg")),
        ev(&[Event::MoveTop, Event::MoveDown(2)])
    );
    // g then something unbound is nothing, and forgets the g.
    assert_eq!(feed(&mut i, &km, SCREEN, &chars("gx")), NOTHING);
    assert_eq!(i.pending, Vec::<Key>::new());
    assert_eq!(
        feed(&mut i, &km, SCREEN, &[ctrl('u')]),
        ev(&[Event::HalfPageUp(20)])
    );
}

#[test]
fn the_search_prompt_is_incremental_and_esc_restores_the_previous_query() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    assert_eq!(feed(&mut i, &km, SCREEN, &chars("/")), NOTHING);
    assert!(matches!(i.prompt, Some(Prompt::Search { .. })));
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("lu")),
        ev(&[Event::Search("l".into()), Event::Search("lu".into())])
    );
    // In the prompt, keys are text: `j` and `q` are typed, not actions.
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("q")),
        ev(&[Event::Search("luq".into())])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &[Key::plain(KeyCode::Backspace)]),
        ev(&[Event::Search("lu".into())])
    );
    assert_eq!(
        feed(&mut i, &km, SCREEN, &[Key::plain(KeyCode::Enter)]),
        NOTHING,
        "Enter keeps the query"
    );
    assert_eq!(i.prompt, None);

    feed(&mut i, &km, SCREEN, &chars("/xy"));
    assert_eq!(
        feed(&mut i, &km, SCREEN, &[esc()]),
        ev(&[Event::Search("prev".into())])
    );
    assert_eq!(i.prompt, None);
}

#[test]
fn the_command_prompt_knows_q_and_refresh_only() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    let enter = Key::plain(KeyCode::Enter);
    let cmd = |i: &mut Input, s: &str| {
        let mut keys = chars(":");
        keys.extend(chars(s));
        keys.push(enter);
        feed(i, &km, SCREEN, &keys)
    };
    assert_eq!(cmd(&mut i, "q"), ev(&[Event::Quit]));
    assert_eq!(cmd(&mut i, "refresh"), ev(&[Event::Refresh]));
    assert_eq!(cmd(&mut i, "update terra"), NOTHING);
    assert!(
        i.message.as_deref().unwrap().contains(":update terra"),
        "{:?}",
        i.message
    );
    // Esc in the prompt types nothing and runs nothing.
    assert_eq!(
        feed(
            &mut i,
            &km,
            SCREEN,
            &[Key::char(':'), Key::char('q'), esc()]
        ),
        NOTHING
    );
    assert_eq!(i.prompt, None);
}

#[test]
fn esc_closes_a_prompt_then_help_then_the_log_then_the_detail_then_a_count_and_only_then_quits() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    let help = Screen {
        help_open: true,
        ..SCREEN
    };
    // A prompt beats an open overlay.
    i.prompt = Some(Prompt::Command {
        input: String::new(),
    });
    i.log_open = true;
    i.detail_open = true;
    assert_eq!(feed(&mut i, &km, help, &[esc()]), NOTHING);
    assert_eq!(i.prompt, None);
    assert_eq!(feed(&mut i, &km, help, &[esc()]), ev(&[Event::CloseHelp]));
    assert_eq!(feed(&mut i, &km, SCREEN, &[esc()]), NOTHING);
    assert!(!i.log_open && i.detail_open);
    assert_eq!(feed(&mut i, &km, SCREEN, &[esc()]), NOTHING);
    assert!(!i.detail_open);
    assert_eq!(feed(&mut i, &km, SCREEN, &chars("5")), NOTHING);
    assert_eq!(feed(&mut i, &km, SCREEN, &[esc()]), NOTHING);
    assert_eq!(i.count, None, "Esc dropped the count");
    assert_eq!(feed(&mut i, &km, SCREEN, &[esc()]), ev(&[Event::Quit]));
    // With help open, moving keys do nothing; q and ctrl-c still quit.
    assert_eq!(feed(&mut i, &km, help, &chars("j")), NOTHING);
    assert_eq!(feed(&mut i, &km, help, &[ctrl('c')]), ev(&[Event::Quit]));
}

fn overrides(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    pairs
        .iter()
        .map(|(a, ks)| {
            (
                (*a).to_owned(),
                ks.iter().map(|k| (*k).to_owned()).collect(),
            )
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_rebound_key_shows_in_the_help_and_the_old_one_no_longer_works() {
    let mut warnings = Vec::new();
    let km = Keymap::with_overrides(&overrides(&[("down", &["x"])]), &mut warnings).unwrap();
    assert_eq!(warnings, NOTHING);

    let help = help_lines(&km);
    let down = help.iter().find(|(_, d)| d.starts_with("down")).unwrap();
    assert_eq!(down.0, "x", "the overlay shows the new key, and only it");

    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let start = shell.view().selected;
    type_keys(&shell, &mut ui, &km, "j");
    assert_eq!(shell.view().selected, start, "j no longer moves");
    type_keys(&shell, &mut ui, &km, "x");
    assert_eq!(shell.view().selected, start.map(|s| s + 1), "x does");

    // And the overlay as drawn, not just its lines.
    type_keys(&shell, &mut ui, &km, "?");
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 36);
    let row = frame.lines().find(|l| l.contains("down (count")).unwrap();
    assert!(row.contains("│x "), "{row}");
    assert!(!row.contains("j "), "{row}");
}

#[test]
fn an_empty_list_unbinds_an_action() {
    let mut warnings = Vec::new();
    let km = Keymap::with_overrides(&overrides(&[("refresh", &[])]), &mut warnings).unwrap();
    let mut i = Input::default();
    assert_eq!(feed(&mut i, &km, SCREEN, &chars("r")), NOTHING);
    let help = help_lines(&km);
    assert!(
        help.iter()
            .any(|(k, d)| k == "(unbound)" && d == "scan again"),
        "{help:?}"
    );
    // Still reachable from the prompt.
    let mut keys = chars(":refresh");
    keys.push(Key::plain(KeyCode::Enter));
    assert_eq!(feed(&mut i, &km, SCREEN, &keys), ev(&[Event::Refresh]));
}

#[test]
fn an_explicit_key_is_taken_from_a_default_and_conflicts_are_refused() {
    let mut w = Vec::new();
    // `n` is search_next's default; given to down, search_next loses it.
    let km = Keymap::with_overrides(&overrides(&[("down", &["n"])]), &mut w).unwrap();
    assert_eq!(km.keys_for(auspex_tui::keymap::Action::SearchNext), NOTHING);
    assert!(
        Keymap::with_overrides(&overrides(&[("down", &["x"]), ("up", &["x"])]), &mut w)
            .unwrap_err()
            .contains("also bound")
    );
    let prefix = Keymap::with_overrides(&overrides(&[("refresh", &["g"])]), &mut w).unwrap_err();
    assert!(
        prefix.contains("`g`") && prefix.contains("`gg`"),
        "{prefix}"
    );
    assert!(
        Keymap::with_overrides(&overrides(&[("down", &["ctrl-"])]), &mut w)
            .unwrap_err()
            .contains("keymap.down")
    );
    let _ = Keymap::with_overrides(&overrides(&[("teleport", &["t"])]), &mut w).unwrap();
    assert!(w.iter().any(|w| w.contains("keymap.teleport")), "{w:?}");
}

#[test]
fn a_bound_digit_is_an_action_not_a_count() {
    let mut w = Vec::new();
    let km = Keymap::with_overrides(&overrides(&[("bottom", &["0"])]), &mut w).unwrap();
    let mut i = Input::default();
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("0")),
        ev(&[Event::MoveBottom])
    );
}

/// Review finding 6: a count of 99 999 on a half page or `n` expanded into that many core
/// events per key press. A half page is one event of `count` half pages; `n` and `N` step
/// at most once round the matches (the result is the same, the matches being a cycle).
#[test]
fn repeat_counts_stay_cheap() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    let mut keys = chars("99999");
    keys.push(ctrl('d'));
    assert_eq!(
        feed(&mut i, &km, SCREEN, &keys),
        ev(&[Event::MoveDown(10 * 18)]),
        "at most as many half pages as there are rows"
    );
    // 3 matches: 5n lands where 2n does; 99999n where 3n does.
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("5n")),
        ev(&[Event::SearchNext, Event::SearchNext])
    );
    assert_eq!(feed(&mut i, &km, SCREEN, &chars("99999N")).len(), 3);
    let none = Screen {
        matches: 0,
        ..SCREEN
    };
    assert_eq!(
        feed(&mut i, &km, none, &chars("99999n")),
        ev(&[Event::SearchNext])
    );
}

/// Review finding 6 and the re-review: holding `r` started (and killed) one `nix` per key
/// repeat, but dropping every `r` while the list is out meant a hung list could not be
/// restarted until `settle`. Now a refresh within `input::MIN_RESTART` of the last one is
/// dropped (key and `:` alike); after that it goes through, and the core's `Refresh`
/// cancels the hung list request and asks again.
#[test]
fn a_refresh_while_the_list_is_out_restarts_it_after_a_short_interval() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    let just_started = Screen {
        listing_for: Some(Duration::from_millis(500)),
        ..SCREEN
    };
    assert_eq!(feed(&mut i, &km, just_started, &chars("r")), NOTHING);
    assert!(i.message.is_some(), "it says why");
    let mut keys = chars(":refresh");
    keys.push(Key::plain(KeyCode::Enter));
    assert_eq!(feed(&mut i, &km, just_started, &keys), NOTHING);
    let hung = Screen {
        listing_for: Some(auspex_tui::input::MIN_RESTART + Duration::from_millis(1)),
        ..SCREEN
    };
    assert_eq!(feed(&mut i, &km, hung, &chars("r")), ev(&[Event::Refresh]));
    assert_eq!(feed(&mut i, &km, hung, &keys), ev(&[Event::Refresh]));
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("r")),
        ev(&[Event::Refresh])
    );
}

/// Review N5: a counted half page moved `(page * n) / 2` rows, not `n` half pages: with a
/// one-row page, `3 ctrl-d` moved one row. It is `n * max(page / 2, 1)` rows again, as
/// one event, and no further than the rows there are.
#[test]
fn counted_half_pages_move_n_half_pages() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    let page = |page| Screen { page, ..SCREEN };
    assert_eq!(
        feed(&mut i, &km, page(5), &[Key::char('2'), ctrl('d')]),
        ev(&[Event::MoveDown(4)])
    );
    assert_eq!(
        feed(&mut i, &km, page(1), &[Key::char('3'), ctrl('d')]),
        ev(&[Event::MoveDown(3)])
    );
    assert_eq!(
        feed(&mut i, &km, page(5), &[Key::char('3'), ctrl('u')]),
        ev(&[Event::MoveUp(6)])
    );
    assert_eq!(
        feed(&mut i, &km, page(5), &[ctrl('d')]),
        ev(&[Event::HalfPageDown(5)])
    );
}

/// The re-review's M04-M06: `Screen::from_view` is what the app uses, and nothing checked
/// it reads the list's phase, the search's matches and the row count from the view.
#[tokio::test(start_paused = true)]
async fn the_screen_is_read_from_the_view() {
    let shell = common::started_demo();
    // The demo's list answers after 150 ms.
    let starting = Screen::from_view(&shell.view(), 20, Duration::from_secs(1));
    assert_eq!(starting.listing_for, Some(Duration::from_secs(1)));
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/ar\n");
    let view = shell.view();
    let settled = Screen::from_view(&view, 20, Duration::from_secs(1));
    assert_eq!(settled.listing_for, None);
    assert_eq!(settled.matches, view.search.as_ref().unwrap().matches.len());
    assert!(settled.matches > 1);
    assert_eq!(settled.rows, 18);
    // And through the keys: 2 half pages of a 5-row page from the top is row 4.
    let mut ui = Ui {
        page: 5,
        ..Ui::default()
    };
    type_keys(&shell, &mut ui, &km, "gg2");
    common::press(&shell, &mut ui, &km, ctrl('d'));
    assert_eq!(shell.view().selected, Some(4));
}

/// Review finding 6: `quit = []` with `command = []` left no key that exits (raw mode makes
/// Ctrl-C a key). Refused, naming `keymap.quit`; either one alone is fine.
#[test]
fn a_keymap_with_no_way_to_quit_is_refused() {
    let mut w = Vec::new();
    let e =
        Keymap::with_overrides(&overrides(&[("quit", &[]), ("command", &[])]), &mut w).unwrap_err();
    assert!(e.contains("keymap.quit"), "{e}");
    assert!(Keymap::with_overrides(&overrides(&[("quit", &[])]), &mut w).is_ok());
    assert!(Keymap::with_overrides(&overrides(&[("command", &[])]), &mut w).is_ok());
}

/// Review N4: a terminal sends Ctrl-M as Enter, Ctrl-I as Tab and Ctrl-[ as Esc, so a
/// binding to those never fires, and `quit = ["ctrl-m"]` with `command = []` passed the
/// "a way to quit" check yet left none. They are refused, pointing at the key to write.
#[test]
fn keys_the_terminal_never_delivers_are_refused() {
    for (spec, instead) in [
        ("ctrl-m", "enter"),
        ("ctrl-i", "tab"),
        ("ctrl-[", "esc"),
        ("CTRL-M", "enter"),
    ] {
        let e = auspex_tui::keymap::parse_chord(spec).unwrap_err();
        assert!(e.contains(instead), "{spec}: {e}");
    }
    let mut w = Vec::new();
    let e = Keymap::with_overrides(
        &overrides(&[("quit", &["ctrl-m"]), ("command", &[])]),
        &mut w,
    )
    .unwrap_err();
    assert!(e.contains("keymap.quit"), "{e}");
    // The keys they arrive as are bindable.
    assert!(auspex_tui::keymap::parse_chord("ctrl-d").is_ok());
}

// ---------------------------------------------------------------------------------------
// Act mode, the action keys, the confirmations, the Log tab
// ---------------------------------------------------------------------------------------

#[test]
fn a_u_and_b_are_the_act_update_and_build_keys() {
    let km = Keymap::defaults();
    let mut input = Input::default();
    assert_eq!(feed(&mut input, &km, SCREEN, &chars("A")), ["ToggleAct"]);
    assert_eq!(
        feed(&mut input, &km, SCREEN, &chars("b")),
        ["BuildSelected"]
    );
    // `u` in observe mode: no event, and the prompt row says why.
    assert_eq!(feed(&mut input, &km, SCREEN, &chars("u")), NOTHING);
    assert_eq!(input.message.as_deref(), Some("act mode is off (A)"));
    // In act mode it is the update.
    let act = Screen {
        act: true,
        ..SCREEN
    };
    let mut input = Input::default();
    assert_eq!(feed(&mut input, &km, act, &chars("u")), ["UpdateSelected"]);
    assert!(input.message.is_none());
}

#[test]
fn with_a_question_open_only_y_is_a_yes_and_nothing_else_acts() {
    let km = Keymap::defaults();
    for kind in [
        ConfirmKind::Act,
        ConfirmKind::Update,
        ConfirmKind::Quit,
        // The checks are running, or refused: any key answers, and the core ignores a yes
        // there (it was never shown the question).
        ConfirmKind::Checking,
        ConfirmKind::Refused,
    ] {
        let screen = Screen {
            confirm: Some(kind),
            ..SCREEN
        };
        for (key, yes) in [
            (Key::char('y'), true),
            (Key::char('Y'), true),
            (Key::char('n'), false),
            (Key::char('N'), false),
            (Key::char('j'), false),
            (Key::char('q'), false),
            (esc(), false),
            (Key::plain(KeyCode::Enter), false),
            (Key::char(' '), false),
        ] {
            let mut input = Input::default();
            let events = feed(&mut input, &km, screen, &[key]);
            assert_eq!(
                events,
                [format!("ConfirmAnswer({yes})")],
                "{kind:?}, {key}: exactly one answer and nothing else"
            );
        }
    }
}

/// The first Ctrl-C asks, the second answers yes, and a third (once it is stopping and no
/// question is open) is an ordinary quit, which the core takes as "force".
#[test]
fn ctrl_c_escalates_ask_then_stop_then_force() {
    let km = Keymap::defaults();
    let mut input = Input::default();
    assert_eq!(feed(&mut input, &km, SCREEN, &[ctrl('c')]), ["Quit"]);
    let asking = Screen {
        confirm: Some(ConfirmKind::Quit),
        ..SCREEN
    };
    assert_eq!(
        feed(&mut input, &km, asking, &[ctrl('c')]),
        ["ConfirmAnswer(true)"],
        "the second Ctrl-C is a yes to the quit question"
    );
    assert_eq!(feed(&mut input, &km, SCREEN, &[ctrl('c')]), ["Quit"]);
    // But not to the other questions: Ctrl-C never turns act mode on or starts an update.
    for kind in [
        ConfirmKind::Act,
        ConfirmKind::Update,
        ConfirmKind::UpdateTyped,
    ] {
        let screen = Screen {
            confirm: Some(kind),
            ..SCREEN
        };
        assert_eq!(
            feed(&mut Input::default(), &km, screen, &[ctrl('c')]),
            ["ConfirmAnswer(false)"],
            "{kind:?}"
        );
    }
}

#[test]
fn the_log_tab_scrolls_with_the_list_keys_and_q_closes_it_instead_of_quitting() {
    let km = Keymap::defaults();
    let mut input = Input::default();
    assert_eq!(feed(&mut input, &km, SCREEN, &chars("L")), NOTHING);
    assert!(input.log_open);
    // Following: down has nowhere to go; up lets go.
    assert_eq!(feed(&mut input, &km, SCREEN, &chars("j")), NOTHING);
    assert!(!input.log.unfollowed);
    input.log.scroll = 40;
    assert_eq!(feed(&mut input, &km, SCREEN, &chars("kk")), NOTHING);
    assert!(input.log.unfollowed);
    assert_eq!(input.log.scroll, 38);
    feed(&mut input, &km, SCREEN, &chars("j"));
    assert_eq!(input.log.scroll, 39);
    feed(&mut input, &km, SCREEN, &chars("gg"));
    assert_eq!((input.log.scroll, input.log.unfollowed), (0, true));
    feed(&mut input, &km, SCREEN, &chars("G"));
    assert!(!input.log.unfollowed, "G follows again");
    // The action keys do nothing unseen behind the tab.
    let act = Screen {
        act: true,
        ..SCREEN
    };
    assert_eq!(feed(&mut input, &km, act, &chars("u")), NOTHING);
    assert_eq!(feed(&mut input, &km, act, &chars("b")), NOTHING);
    assert_eq!(feed(&mut input, &km, SCREEN, &chars("r")), NOTHING);
    // Tab: the notes; q closes the tab and does not quit; Ctrl-C still quits.
    feed(&mut input, &km, SCREEN, &[Key::plain(KeyCode::Tab)]);
    assert!(input.log.notes);
    assert!(feed(&mut input, &km, SCREEN, &[ctrl('c')]).contains(&"Quit".to_owned()));
    assert_eq!(feed(&mut input, &km, SCREEN, &chars("q")), NOTHING);
    assert!(!input.log_open);
}

#[test]
fn the_help_lists_the_new_keys_from_the_live_keymap() {
    let lines = help_lines(&Keymap::defaults());
    for (keys, what) in [
        ("A", "toggle act mode"),
        ("u", "update the machine"),
        ("b", "build its pending system"),
    ] {
        assert!(
            lines.iter().any(|(k, d)| k == keys && d.contains(what)),
            "{keys} {what}: {lines:?}"
        );
    }
}

/// A typed confirmation takes text: characters are typed, Enter submits, Backspace deletes,
/// Esc and Ctrl-C cancel, and no other key does anything. In particular `y` is a letter, and
/// no key but Enter can confirm.
#[test]
fn a_typed_confirmation_takes_text_and_only_enter_submits() {
    let km = Keymap::defaults();
    let screen = Screen {
        confirm: Some(ConfirmKind::UpdateTyped),
        act: true,
        ..SCREEN
    };
    let feed_one = |key: Key| feed(&mut Input::default(), &km, screen, &[key]);
    for c in [
        'l', 'U', 'y', 'N', '-', '_', '7', 'q', 'j', 'A', 'u', 'b', ' ', '/',
    ] {
        assert_eq!(
            feed_one(Key::char(c)),
            [format!("ConfirmChar({c:?})")],
            "{c:?} is text, not a command"
        );
    }
    assert_eq!(feed_one(Key::plain(KeyCode::Enter)), ["ConfirmSubmit"]);
    assert_eq!(
        feed_one(Key::plain(KeyCode::Backspace)),
        ["ConfirmBackspace"]
    );
    assert_eq!(feed_one(esc()), ["ConfirmAnswer(false)"]);
    assert_eq!(feed_one(ctrl('c')), ["ConfirmAnswer(false)"]);
    // Nothing else means anything: a control chord is not a letter, an arrow is not a yes.
    for key in [
        ctrl('y'),
        ctrl('m'),
        Key::plain(KeyCode::Tab),
        Key::plain(KeyCode::Up),
        Key::plain(KeyCode::F(5)),
    ] {
        assert_eq!(feed_one(key), NOTHING, "{key}");
    }
    // And no count or pending key survives into the dialog.
    let mut input = Input::default();
    let _ = feed(&mut input, &km, SCREEN, &chars("5g"));
    let _ = feed(&mut input, &km, screen, &chars("x"));
    assert_eq!((input.count, input.pending.len()), (None, 0));
}
