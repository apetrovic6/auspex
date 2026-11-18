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
    assert!(at("diff      linux") < at("critical  linux 6.12.38 → 6.12.41"));
    assert!(at("critical  linux") < at("systemd 256.10 → 257.2"));
    assert!(at("systemd 256.10") < at("glibc 2.40-66 → 2.40-77"));
    assert!(at("glibc") < at("nix rebuilt"));
    assert!(at("nix rebuilt") < at("3 other major-version changes"));
    assert!(at("other major-version changes") < at("+3 added  −2 removed  ~10 changed"));
    expect_file("snapshots", "diff_computed_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn a_not_cached_diff_at_120_columns() {
    let shell = settled_demo().await;
    let (mut ui, km) = (Ui::default(), Keymap::defaults());
    type_keys(&shell, &mut ui, &km, "/cypramundi\n");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(frame.contains("not cached: the pending closure"), "{frame}");
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
        "changed (10)",
        "added (3)",
        "removed (2)",
        "linux  6.12.38 → 6.12.41",
        "ripgrep  14.1.1",
        "htop  3.3.0",
    ] {
        assert!(frame.contains(needle), "{needle}\n{frame}");
    }
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
        ("cypramundi", "not cached: the pending closure"),
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
