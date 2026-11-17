//! No drawing function may loop or panic at any terminal size, in any view state, whatever
//! the text: review finding 1 was `ui::wrap` looping forever at width 1 on a two-cell
//! character, which hung the TUI inside `terminal.draw` where no signal is looked at.
//!
//! The text here is injected straight into the view model, bypassing the domain's
//! sanitiser, so the drawing code is tested against the worst it could ever be handed:
//! wide, combining, zero-width and bidi characters, emoji, and very long words. Every test
//! runs on a thread under a time bound, so a regression fails instead of hanging CI.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use auspex_core::ViewModel;
use auspex_tui::input::Prompt;
use auspex_tui::keymap::Keymap;
use auspex_tui::ui::{Ui, wrap};
use common::{render, settled_demo};

/// A backstop for a whole test. The whole fuzz takes about 2 s here and about 27 s on one
/// loaded CPU; a hang is caught much sooner by the per-case [`Watchdog`].
const BOUND: Duration = Duration::from_secs(60);

/// No fuzz case may take longer than this. Each takes milliseconds.
const PER_CASE: Duration = Duration::from_secs(5);

/// The most address space this test binary may use. A renderer that loops while
/// allocating (finding 1's grew about 40 MB/s per thread) then fails its allocation and
/// aborts instead of taking the machine's memory: the first version of this fuzz reached
/// 20 GB before its time bound (review N3). A normal run peaks at about 130 MB resident and
/// 0.9 GB of address space (thread stacks and allocator arenas), so 1.5 GiB leaves room.
const ADDRESS_SPACE: u64 = 1536 << 20;

fn limit_memory() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use rustix::process::{Resource, Rlimit, setrlimit};
        let limit = Rlimit {
            current: Some(ADDRESS_SPACE),
            maximum: Some(ADDRESS_SPACE),
        };
        setrlimit(Resource::As, limit).expect("lowering RLIMIT_AS");
    });
}

/// Aborts the whole test process when `tick` has not been called for [`PER_CASE`]: a
/// hung case fails in seconds, before it can grow, and a thread stuck in a loop is not
/// left running.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

struct Watchdog {
    last: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
    start: Instant,
}

impl Watchdog {
    fn new(what: &'static str) -> Self {
        limit_memory();
        let last = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let (l, d) = (Arc::clone(&last), Arc::clone(&done));
        std::thread::spawn(move || {
            while !d.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                let since = millis(start.elapsed()).saturating_sub(l.load(Ordering::Relaxed));
                if since > millis(PER_CASE) && !d.load(Ordering::Relaxed) {
                    eprintln!("{what}: one case took over {PER_CASE:?}: a loop; aborting");
                    std::process::abort();
                }
            }
        });
        Self { last, done, start }
    }

    fn tick(&self) {
        self.last
            .store(millis(self.start.elapsed()), Ordering::Relaxed);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    bounded_for(what, BOUND, f)
}

fn bounded_for<T: Send + 'static>(
    what: &str,
    bound: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    limit_memory();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(bound)
        .unwrap_or_else(|_| panic!("{what} did not finish within {bound:?}: a loop, or a panic"))
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
    let dog = Watchdog::new("rendering");
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
                dog.tick();
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
    let dog = Watchdog::new("rendering");
    bounded("rendering", move || {
        let km = Keymap::defaults();
        for (w, h) in sizes() {
            for detail in [false, true] {
                let mut ui = Ui::default();
                ui.input.detail_open = detail;
                let _ = render(&view, &mut ui, &km, &[], w, h);
                dog.tick();
            }
        }
    });
}

/// Review N2: `wrap` rescanned the rest of an unbroken word for every piece, so its cost
/// grew with the square of the word: 250 KB from a remote host froze the detail view for
/// about 28 s a frame. Linear now, and the detail view wraps at most
/// `ui::DETAIL_VALUE_CAP` bytes of any one value.
#[test]
fn a_megabyte_word_wraps_in_linear_time() {
    let word = "a".repeat(1_000_000);
    let lines = bounded_for(
        "wrap of 1 MB at width 1",
        Duration::from_secs(10),
        move || wrap(&word, 1).len(),
    );
    assert_eq!(lines, 1_000_000);
    let wide = "日".repeat(300_000);
    let lines = bounded_for(
        "wrap of 900 KB of wide text at width 13",
        Duration::from_secs(10),
        move || wrap(&wide, 13).len(),
    );
    assert_eq!(lines, 50_000, "six two-cell characters per 13-cell line");
}

#[tokio::test(start_paused = true)]
async fn a_megabyte_revision_draws_the_narrow_detail_quickly() {
    let shell = settled_demo().await;
    let mut view = shell.view();
    let huge = "a".repeat(1_000_000);
    for m in &mut view.report.machines {
        m.deployed_revision = Some(huge.clone());
        m.reason = Some(huge.clone());
    }
    bounded_for(
        "drawing a 1 MB value at 13 columns",
        Duration::from_secs(5),
        move || {
            let km = Keymap::defaults();
            let mut ui = Ui::default();
            ui.input.detail_open = true;
            for _ in 0..3 {
                let _ = render(&view, &mut ui, &km, &[], 13, 20);
            }
        },
    );
}

/// Review N2: a value longer than `ui::DETAIL_VALUE_CAP` is cut (with `…`) before it is
/// wrapped, so the detail panel never lays out more than a few screens of one value.
#[tokio::test(start_paused = true)]
async fn a_long_detail_value_is_cut_to_the_cap() {
    let shell = settled_demo().await;
    let mut view = shell.view();
    view.selected = Some(0);
    view.report.machines[0].deployed_revision = Some("z".repeat(100_000));
    let km = Keymap::defaults();
    let mut ui = Ui::default();
    // Wide layout, a panel tall enough for the whole value if it were not cut.
    let frame = render(&view, &mut ui, &km, &[], 120, 4000);
    let zs: usize = frame.chars().filter(|c| *c == 'z').count();
    assert!(
        zs <= auspex_tui::ui::DETAIL_VALUE_CAP,
        "{zs} characters of the value drawn"
    );
    assert!(zs > 1000, "the start of it is still shown: {zs}");
    assert!(frame.contains("z…"), "and it says it was cut");
}
