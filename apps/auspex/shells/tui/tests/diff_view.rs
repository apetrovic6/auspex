//! The package preview on screen: the detail panel for a machine whose diff is computed,
//! `not_cached` and `failed` (luna, cypramundi and sol in the demo), at 40 and 120 columns,
//! and the `d` view: its scrolling, and `q`/`Esc` closing it instead of quitting.
//!
//! The expected frames are in `tests/fixtures/snapshots/`; set `AUSPEX_UPDATE_SNAPSHOTS=1`
//! to rewrite them after a deliberate change, and review the diff. Time is paused, so the
//! demo's scripted delays elapse at once and nothing drawn depends on the clock.

mod common;

use auspex_core::ViewModel;
use auspex_domain::diff::DiffState;
use auspex_tui::keymap::Keymap;
use auspex_tui::ui::Ui;
use common::{expect_file, render, settled_demo, type_keys};

fn machine<'a>(view: &'a ViewModel, name: &str) -> &'a auspex_domain::report::MachineReport {
    view.report
        .machines
        .iter()
        .find(|m| m.name == name)
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_demo_has_one_of_each_state() {
    let shell = settled_demo().await;
    let view = shell.view();
    let state = |n: &str| machine(&view, n).package_diff.as_ref().map(|d| d.state);
    assert_eq!(state("luna"), Some(DiffState::Computed));
    assert_eq!(state("cypramundi"), Some(DiffState::NotCached));
    assert_eq!(state("sol"), Some(DiffState::Failed));
    assert_eq!(state("terra"), None, "in sync: nothing to preview");
}

#[tokio::test(start_paused = true)]
async fn a_computed_diff_at_120_columns() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/luna\n");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    // The order the ticket asks for: headline, critical highlights (kernel first), a count of
    // the other major-version changes, then the counts.
    let at = |needle: &str| {
        frame
            .find(needle)
            .unwrap_or_else(|| panic!("{needle}\n{frame}"))
    };
    // Health first (failed units matter most before an update), then the preview.
    assert!(at("failed    none") < at("diff      linux"));
    assert!(at("diff      linux") < at("critical  linux 6.12.38 → 6.12.41"));
    assert!(at("critical  linux") < at("systemd 256.10 → 257.2"));
    assert!(at("systemd 256.10") < at("glibc"));
    assert!(at("glibc") < at("nix rebuilt"));
    assert!(at("nix rebuilt") < at("4 other major-version changes; +3 −2 ~12"));
    // The headline once: the reason line no longer carries it.
    assert_eq!(
        frame.matches("linux 6.12.38 → 6.12.41, +3").count(),
        1,
        "{frame}"
    );
    expect_file("snapshots", "diff_computed_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn a_not_cached_diff_at_120_columns() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/cypramundi\n");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(
        frame.contains("not cached: the binary cache does not have the pending"),
        "{frame}"
    );
    expect_file("snapshots", "diff_not_cached_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn a_failed_diff_at_120_columns() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/sol\n");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(
        frame.contains("failed: deployed closure query timed out after 120s"),
        "{frame}"
    );
    expect_file("snapshots", "diff_failed_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn the_three_states_at_40_columns() {
    let shell = settled_demo().await;
    let km = Keymap::defaults();
    for (name, file) in [
        ("luna", "diff_computed_40.txt"),
        ("cypramundi", "diff_not_cached_40.txt"),
        ("sol", "diff_failed_40.txt"),
    ] {
        let mut ui = Ui::default();
        // Search for the machine, then Enter opens the narrow layout's detail view.
        type_keys(&shell, &mut ui, &km, &format!("/{name}\n\n"));
        assert!(ui.input.detail_open);
        let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 40);
        expect_file("snapshots", file, &frame);
    }
}

#[tokio::test(start_paused = true)]
async fn d_opens_the_full_list_and_q_closes_it_without_quitting() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/luna\n");
    type_keys(&shell, &mut ui, &km, "d");
    assert!(ui.input.diff_open);
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    for needle in [
        "package diff: luna",
        "changed (12)",
        "added (3)",
        "removed (2)",
        "linux  6.12.38 → 6.12.41",
        "ripgrep  14.1.1",
        "htop  3.3.0",
        "firefox  133.0 → 134.0 (+ unwrapped)",
        "gnome-settings-daemon-gsettings-schemas  47.2 → 48.0",
    ] {
        assert!(frame.contains(needle), "{needle}\n{frame}");
    }
    assert!(
        !frame.contains("firefox-unwrapped"),
        "folded into firefox: {frame}"
    );
    expect_file("snapshots", "diff_list_120.txt", &frame);

    // `q` is quit by default; with the list open it closes the list.
    type_keys(&shell, &mut ui, &km, "q");
    assert!(!ui.input.diff_open);
    assert!(!shell.view().quit, "q closed the diff, it did not quit");

    // So does Esc, which is quit by default too.
    type_keys(&shell, &mut ui, &km, "d\x1b");
    assert!(!ui.input.diff_open);
    assert!(!shell.view().quit);

    // With nothing open, q quits again.
    type_keys(&shell, &mut ui, &km, "q");
    assert!(shell.view().quit);
}

#[tokio::test(start_paused = true)]
async fn the_list_scrolls_and_never_past_its_end() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/luna\nd");
    // A short window: the list does not fit.
    let top = render(&shell.view(), &mut ui, &km, &[], 60, 12);
    assert!(top.contains("package diff: luna"), "{top}");
    assert!(top.contains("1–"), "the position is shown: {top}");
    assert!(!top.contains("htop"), "the end is not visible yet: {top}");
    expect_file("snapshots", "diff_list_top_60.txt", &top);

    type_keys(&shell, &mut ui, &km, "3j");
    let down = render(&shell.view(), &mut ui, &km, &[], 60, 12);
    assert_eq!(ui.input.diff_scroll, 3);
    assert_ne!(top, down);

    type_keys(&shell, &mut ui, &km, "G");
    let bottom = render(&shell.view(), &mut ui, &km, &[], 60, 12);
    assert!(bottom.contains("htop"), "{bottom}");
    expect_file("snapshots", "diff_list_bottom_60.txt", &bottom);
    let clamped = ui.input.diff_scroll;
    assert!(
        clamped < usize::MAX,
        "the drawing writes the clamped value back"
    );

    // Further down does not move, and up moves from where the list ends.
    type_keys(&shell, &mut ui, &km, "5j");
    render(&shell.view(), &mut ui, &km, &[], 60, 12);
    assert_eq!(ui.input.diff_scroll, clamped);
    type_keys(&shell, &mut ui, &km, "k");
    assert_eq!(ui.input.diff_scroll, clamped - 1);

    // gg goes back to the top; the keys that move the machine list do nothing unseen.
    type_keys(&shell, &mut ui, &km, "gg");
    assert_eq!(ui.input.diff_scroll, 0);
    let selected = shell.view().selected;
    type_keys(&shell, &mut ui, &km, "jjr");
    assert_eq!(shell.view().selected, selected);
}

#[tokio::test(start_paused = true)]
async fn d_on_a_machine_without_a_diff_says_why() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    for (name, says) in [
        ("terra", "terra is in sync: there is no update to preview"),
        ("cypramundi", "not cached: the binary cache does not have"),
        (
            "sol",
            "the preview failed: deployed closure query timed out",
        ),
    ] {
        type_keys(&shell, &mut ui, &km, &format!("/{name}\nd"));
        let frame = render(&shell.view(), &mut ui, &km, &[], 120, 30);
        assert!(frame.contains(says), "{name}: {frame}");
        type_keys(&shell, &mut ui, &km, "q");
    }
}

#[tokio::test(start_paused = true)]
async fn the_help_lists_the_key_and_the_key_can_be_rebound() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "?");
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 36);
    assert!(frame.contains("package diff (q closes)"), "{frame}");
    type_keys(&shell, &mut ui, &km, "?");
    assert!(!shell.view().help_open);

    let mut warnings = Vec::new();
    let rebound = Keymap::with_overrides(
        &std::collections::BTreeMap::from([("diff".to_owned(), vec!["p".to_owned()])]),
        &mut warnings,
    )
    .unwrap();
    assert_eq!(warnings, Vec::<String>::new());
    let mut ui = Ui::default();
    type_keys(&shell, &mut ui, &rebound, "d");
    assert!(!ui.input.diff_open, "d is no longer bound");
    type_keys(&shell, &mut ui, &rebound, "p");
    assert!(ui.input.diff_open);
}

/// The review's finding F2: the preview used to push systemd, failed units and the health
/// values off a 24-row panel. Now they come first and the preview is capped below them.
#[tokio::test(start_paused = true)]
async fn failed_units_and_systemd_stay_visible_on_a_24_row_panel() {
    let shell = settled_demo().await;
    let km = Keymap::defaults();
    for (width, file) in [
        (120, "diff_computed_120x24.txt"),
        (100, "diff_computed_100x24.txt"),
        (40, "diff_computed_40x24.txt"),
    ] {
        let mut ui = Ui::default();
        // The narrow layout shows the detail only after Enter.
        let keys = if width < 100 { "/luna\n\n" } else { "/luna\n" };
        type_keys(&shell, &mut ui, &km, keys);
        let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), width, 24);
        // Re-review N4: however short the panel, one row says a preview exists and how to open it.
        assert!(frame.contains("+3 −2 ~12 packages (d)"), "{width}\n{frame}");
        if width >= 100 {
            for needle in ["systemd   running", "failed    none"] {
                assert!(frame.contains(needle), "{width}: {needle}\n{frame}");
            }
        }
        expect_file("snapshots", file, &frame);
    }
    // The other two states, at 100x24 where the block below health is cut off entirely.
    for (name, says) in [("cypramundi", "not cached (d)"), ("sol", "failed (d)")] {
        let mut ui = Ui::default();
        type_keys(&shell, &mut ui, &km, &format!("/{name}\n"));
        let frame = render(&shell.view(), &mut ui, &km, &[], 100, 24);
        assert!(frame.contains(says), "{name}\n{frame}");
    }
}

#[tokio::test(start_paused = true)]
async fn ctrl_c_quits_from_the_diff_overlay_and_d_toggles_it() {
    use auspex_tui::keymap::Key;
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/luna\nd");
    assert!(ui.input.diff_open);
    // `d` again closes it (the help says it toggles).
    type_keys(&shell, &mut ui, &km, "d");
    assert!(!ui.input.diff_open);
    assert!(!shell.view().quit);
    type_keys(&shell, &mut ui, &km, "d");
    common::press(
        &shell,
        &mut ui,
        &km,
        Key {
            code: crossterm::event::KeyCode::Char('c'),
            ctrl: true,
        },
    );
    assert!(shell.view().quit, "ctrl-c quits, overlay or not");
}

#[tokio::test(start_paused = true)]
async fn the_status_bar_counts_previews_that_are_still_collecting() {
    let shell = common::started_demo();
    // 700 ms: sol's two queries are out (the list takes 150 ms, the probe 420, the cache 120).
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    let view = shell.view();
    assert_eq!(view.refresh.diffs_pending, 1);
    let frame = render(&view, &mut ui, &km, &[], 120, 24);
    assert!(frame.contains("1 diff collecting"), "{frame}");
    while !shell.view().refresh.settled {
        shell.changed().await;
    }
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 24);
    assert!(!frame.contains("collecting"), "{frame}");
}

/// Re-review N3: with no `[cache]` the TUI and `--plain` say there is no cache to ask, not
/// that the cache lacks the path.
#[tokio::test(start_paused = true)]
async fn without_a_cache_the_wording_says_so_in_the_panel_the_list_and_plain() {
    use auspex_tui::backend::Backend;
    use auspex_tui::demo::{self, Demo};
    use auspex_tui::log::Log;
    use auspex_tui::shell::Shell;
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        std::time::Duration::from_secs(120),
    );
    let mut config = demo::config();
    config.binary_cache = false;
    shell.send(auspex_core::Event::Start(config));
    while !shell.view().refresh.settled {
        shell.changed().await;
    }
    let view = shell.view();
    assert!(!view.binary_cache);
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/luna\n");
    let panel = render(&shell.view(), &mut ui, &km, &[], 120, 40);
    assert!(panel.contains("no binary cache is configured"), "{panel}");
    assert!(!panel.contains("binary cache does not have"), "{panel}");
    type_keys(&shell, &mut ui, &km, "d");
    let list = render(&shell.view(), &mut ui, &km, &[], 160, 30);
    assert!(list.contains("no binary cache is configured"), "{list}");
    let plain = auspex_tui::headless::plain(&view, false, 120);
    assert!(
        plain.contains("packages: no binary cache configured, no preview"),
        "{plain}"
    );
    assert!(!plain.contains("not in the binary cache"), "{plain}");
}

/// Re-review N6: the overlay lets the keymap's own Quit chord through, so a rebound or
/// unbound `ctrl-c` is followed, not a hard-coded one.
#[tokio::test(start_paused = true)]
async fn the_overlay_follows_the_keymaps_quit_chord() {
    use auspex_tui::keymap::Key;
    let ctrl = |c| Key {
        code: crossterm::event::KeyCode::Char(c),
        ctrl: true,
    };
    let mut warnings = Vec::new();
    let km = Keymap::with_overrides(
        &std::collections::BTreeMap::from([(
            "quit".to_owned(),
            vec!["q".to_owned(), "ctrl-x".to_owned()],
        )]),
        &mut warnings,
    )
    .unwrap();
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    type_keys(&shell, &mut ui, &km, "/luna\nd");
    // ctrl-c is no longer a quit key: it does nothing, and the overlay stays.
    common::press(&shell, &mut ui, &km, ctrl('c'));
    assert!(!shell.view().quit && ui.input.diff_open);
    // The rebound chord quits.
    common::press(&shell, &mut ui, &km, ctrl('x'));
    assert!(shell.view().quit);
}
