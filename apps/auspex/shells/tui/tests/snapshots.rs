//! The plan's acceptance: `--demo` rendered with ratatui's `TestBackend` at 40 and at 120
//! columns, plus the help overlay, a search in progress, and the narrow layout's detail
//! view. The expected frames are text files in `tests/fixtures/snapshots/`; set
//! `AUSPEX_UPDATE_SNAPSHOTS=1` to rewrite them after a deliberate change, and review the diff.
//!
//! Time is paused, so the demo's scripted delays elapse at once and in a fixed order, and
//! the frame counter is fixed: nothing drawn depends on the clock.

mod common;

use auspex_tui::keymap::Keymap;
use auspex_tui::ui::Ui;
use common::{expect_file, render, settled_demo, type_keys};

#[tokio::test(start_paused = true)]
async fn demo_at_40_columns() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 24);
    expect_file("snapshots", "demo_40.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn demo_at_120_columns() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    // Select the slow host: an update pending and cached, with Prometheus and ssh values.
    type_keys(&shell, &mut ui, &km, "/luna\n");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    expect_file("snapshots", "demo_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn the_help_overlay() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "?");
    assert!(shell.view().help_open);
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 36);
    expect_file("snapshots", "help_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn a_search_in_progress() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/ar");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 24);
    expect_file("snapshots", "search_40.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn the_detail_view_in_the_narrow_layout() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    // The long name, so the title is cut; then the dead host's detail.
    type_keys(&shell, &mut ui, &km, "/mars\n\n");
    assert!(ui.input.detail_open);
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 24);
    expect_file("snapshots", "detail_40.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn the_long_name_is_cut_and_alignment_holds_at_40_columns() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 24);
    let rows: Vec<&str> = frame.lines().filter(|l| l.starts_with('│')).collect();
    let long = rows
        .iter()
        .find(|l| l.contains("mechanicus"))
        .expect("the long name is listed");
    assert!(long.contains('…'), "cut with an ellipsis: {long}");
    // Every status starts in the same column: badges and names never push it.
    let column = |l: &str| {
        [
            "in sync",
            "update pending",
            "unreachable",
            "eval error",
            "not installed",
        ]
        .iter()
        .find_map(|s| l.find(s).map(|b| l[..b].chars().count()))
    };
    let columns: Vec<usize> = rows.iter().filter_map(|l| column(l)).collect();
    assert!(columns.len() > 10, "{frame}");
    assert!(
        columns.windows(2).all(|w| w[0] == w[1]),
        "{columns:?}\n{frame}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_list_scrolls_to_keep_the_selection_visible() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    let frame = render(&shell.view(), &mut ui, &km, &[], 40, 10);
    assert!(
        frame.contains("alfrost") && !frame.contains("vulcan"),
        "{frame}"
    );
    type_keys(&shell, &mut ui, &km, "G");
    let frame = render(&shell.view(), &mut ui, &km, &[], 40, 10);
    assert!(frame.contains("›!    vulcan"), "{frame}");
    assert!(!frame.contains("alfrost"), "{frame}");
    // Back up one row: the list does not jump, only the marker moves.
    type_keys(&shell, &mut ui, &km, "k");
    let again = render(&shell.view(), &mut ui, &km, &[], 40, 10);
    assert_eq!(
        again.lines().nth(1),
        frame.lines().nth(1),
        "same first visible row"
    );
}

#[tokio::test(start_paused = true)]
async fn spinners_animate_from_the_frame_counter_not_the_clock() {
    let shell = std::sync::Arc::clone(&common::started_demo());
    // Mid-scan: luna's probe is still out.
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    let view = shell.view();
    let km = Keymap::defaults();
    let mut ui = Ui::default();
    let first = render(&view, &mut ui, &km, &[], 40, 24);
    let same = render(&view, &mut ui, &km, &[], 40, 24);
    assert_eq!(first, same, "the same frame number draws the same frame");
    ui.frame = 4;
    let next = render(&view, &mut ui, &km, &[], 40, 24);
    let luna = |f: &str| f.lines().find(|l| l.contains("luna")).unwrap().to_owned();
    assert!(luna(&first).contains("checking"), "{first}");
    assert_ne!(
        luna(&first),
        luna(&next),
        "the spinner moved with the frame"
    );
    assert_eq!(
        first
            .lines()
            .filter(|l| !l.contains("luna"))
            .collect::<Vec<_>>(),
        next.lines()
            .filter(|l| !l.contains("luna"))
            .collect::<Vec<_>>(),
        "and nothing else did"
    );
}
