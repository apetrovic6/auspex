//! No drawing function may loop or panic at any terminal size, in any view state, whatever
//! the text: review finding 1 was `ui::wrap` looping forever at width 1 on a two-cell
//! character, which hung the TUI inside `terminal.draw` where no signal is looked at.
//!
//! The text here is injected straight into the view model, bypassing the domain's
//! sanitiser, so the drawing code is tested against the worst it could ever be handed:
//! wide, combining, zero-width and bidi characters, emoji, and very long words. Every test
//! runs on a thread under a time bound, so a regression fails instead of hanging CI.

mod common;

use std::sync::mpsc;
use std::time::Duration;

use auspex_core::ViewModel;
use auspex_tui::input::Prompt;
use auspex_tui::keymap::Keymap;
use auspex_tui::ui::{Ui, wrap};
use common::{render, settled_demo};

/// Generous for a debug build on one slow CPU; a hang never finishes.
const BOUND: Duration = Duration::from_secs(120);

fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(BOUND)
        .unwrap_or_else(|_| panic!("{what} did not finish within {BOUND:?}: a loop, or a panic"))
}

const NASTY: &[&str] = &[
    "日本語のホスト名が長い",
    "ssh: 接続がタイムアウトしました",
    "e\u{301}\u{301}\u{301}\u{301} combining",
    "zero\u{200b}width\u{200d}joiner\u{feff}",
    "\u{202e}reversed\u{202c} bidi \u{2066}isolate\u{2069}",
    "🦀🦀🦀 crab 🏳️‍🌈",
    "ＦＵＬＬＷＩＤＴＨ　ＳＰＡＣＥ",
    "",
    " ",
];

fn long_word() -> String {
    "日x".repeat(300)
}

fn poison(view: &mut ViewModel) {
    let long = long_word();
    for (i, m) in view.report.machines.iter_mut().enumerate() {
        let n = NASTY[i % NASTY.len()];
        m.name = format!("{n}{i}");
        m.reason = Some(format!("{n} {long} {n}"));
        m.target = Some(format!("{n}@{long}"));
        m.tags = vec![n.to_owned(), long.clone()];
        m.build_host = Some(n.to_owned());
        m.target_error = Some(n.to_owned());
        m.deployed_revision = Some(long.clone());
        m.failed_units = Some(vec![n.to_owned(), long.clone()]);
        m.system_state = Some(n.to_owned());
    }
    for e in &mut view.report.errors {
        e.reason = format!("{long} {}", NASTY[0]);
    }
}

#[derive(Clone, Copy, Debug)]
enum State {
    List,
    Detail,
    Help,
    Search,
    Command,
    Log,
    Message,
}

const STATES: [State; 7] = [
    State::List,
    State::Detail,
    State::Help,
    State::Search,
    State::Command,
    State::Log,
    State::Message,
];

fn ui_for(state: State) -> (Ui, bool) {
    let mut ui = Ui::default();
    let text = format!("{} {}", NASTY.join(""), long_word());
    let mut help = false;
    match state {
        State::List => {}
        State::Detail => ui.input.detail_open = true,
        State::Help => help = true,
        State::Search => {
            ui.input.prompt = Some(Prompt::Search {
                input: text,
                previous: String::new(),
            });
        }
        State::Command => ui.input.prompt = Some(Prompt::Command { input: text }),
        State::Log => ui.input.log_open = true,
        State::Message => ui.input.message = Some(text),
    }
    (ui, help)
}

fn sizes() -> Vec<(u16, u16)> {
    let mut s = Vec::new();
    for w in 0..=20 {
        for h in 0..=10 {
            s.push((w, h));
        }
    }
    for w in (21..=200).step_by(7) {
        for h in [1, 2, 3, 10, 40] {
            s.push((w, h));
        }
    }
    s.push((200, 60));
    s
}

#[test]
fn wrap_takes_at_least_one_character_per_line() {
    let lines = bounded("wrap", || wrap("日本", 1));
    assert_eq!(lines, ["日", "本"]);
    let lines = bounded("wrap", || wrap("a日b", 2));
    assert_eq!(lines, ["a", "日", "b"]);
    assert_eq!(bounded("wrap", || wrap("ab", 1)), ["a", "b"]);
    assert_eq!(bounded("wrap", || wrap("", 1)), [""]);
    // Zero-width characters cost no cells but still move forward.
    let z = bounded("wrap", || wrap("\u{200b}\u{200b}日", 1));
    assert!(z.concat().contains('日'), "{z:?}");
}

#[tokio::test(start_paused = true)]
async fn every_view_at_every_size_draws_and_returns() {
    let shell = settled_demo().await;
    let mut view = shell.view();
    poison(&mut view);
    // The last row selected, so scrolling is exercised too.
    view.selected = Some(view.report.machines.len() - 1);
    let log: Vec<String> = NASTY
        .iter()
        .map(|n| format!("{n} {}", long_word()))
        .collect();
    bounded("rendering", move || {
        let km = Keymap::defaults();
        let mut frames = 0usize;
        for state in STATES {
            for (w, h) in sizes() {
                let (mut ui, help) = ui_for(state);
                let mut v = view.clone();
                v.help_open = help;
                // A spinner frame other than 0, so the pending glyphs are drawn too.
                ui.frame = u64::from(w) * 3;
                let _ = render(&v, &mut ui, &km, &log, w, h);
                frames += 1;
            }
        }
        frames
    });
}

#[tokio::test(start_paused = true)]
async fn a_mid_scan_view_with_spinners_draws_at_every_size() {
    let shell = common::started_demo();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut view = shell.view();
    poison(&mut view);
    bounded("rendering", move || {
        let km = Keymap::defaults();
        for (w, h) in sizes() {
            for detail in [false, true] {
                let mut ui = Ui::default();
                ui.input.detail_open = detail;
                let _ = render(&view, &mut ui, &km, &[], w, h);
            }
        }
    });
}
