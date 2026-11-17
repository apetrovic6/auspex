//! Keys to core events: counts, `gg`, the `/` and `:` prompts, Esc's precedence, remapping
//! (the help overlay follows the live keymap) and unbinding.
//!
//! [`Input`] is pure, so most of these assert on the events it returns; the remapping tests
//! also draw the help overlay and run the events through the demo's core.

mod common;

use std::collections::BTreeMap;

use auspex_core::Event;
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
    listing: false,
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
        ev(&[Event::HalfPageDown(60)]),
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
        ev(&[Event::HalfPageDown(20 * 99_999)])
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

/// Review finding 6: holding `r` started (and killed) one `nix` per key repeat. While a
/// refresh is still reading the machine list, another is dropped, from the key and from `:`.
#[test]
fn a_refresh_while_one_is_starting_is_dropped() {
    let km = Keymap::defaults();
    let mut i = Input::default();
    let starting = Screen {
        listing: true,
        ..SCREEN
    };
    assert_eq!(feed(&mut i, &km, starting, &chars("r")), NOTHING);
    let mut keys = chars(":refresh");
    keys.push(Key::plain(KeyCode::Enter));
    assert_eq!(feed(&mut i, &km, starting, &keys), NOTHING);
    assert_eq!(
        feed(&mut i, &km, SCREEN, &chars("r")),
        ev(&[Event::Refresh])
    );
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
