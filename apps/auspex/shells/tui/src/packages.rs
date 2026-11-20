//! What the package preview looks like as text: the detail panel's few lines and the `d`
//! view's full list.
//!
//! Pure over [`MachineReport`]: everything shown here is in the report, which is what
//! `--json` prints, so the TUI can never show a preview the JSON does not carry. The words
//! for the two states that have no list (`not_cached`, `failed`) are here too, so the panel,
//! the full view and `--plain` say the same thing.
//!
//! The display rules (AUS-35):
//!
//! - the critical highlights (linux, systemd, glibc, nix) come first, with their versions;
//! - a `-unwrapped` variant is not a highlight of its own when its wrapper is one too and both
//!   change the same way, and the `d` view folds the same pair: the operator reads one line for
//!   `firefox`, not two (`--json` keeps both);
//! - every other highlight is a major-version bump, and many of those are a count, not a
//!   list: the full list (`d`) has them all.

use std::fmt::Write as _;

use auspex_domain::diff::{DiffState, HighlightKind, PackageChange, PackageDiff, PackageHighlight};
use auspex_domain::report::{MachineReport, diff_headline};
use auspex_domain::status::Badge;

/// What a line of the full list is, for styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A section title (`changed (12)`).
    Heading,
    /// One package.
    Item,
    /// Something to read, not a package: a state, or a note about what is cut.
    Note,
}

/// The sentence for a `not_cached` preview.
///
/// It says which of its two causes applies: the cache was asked and does not have the pending
/// closure, or there is no cache to ask (`cache_configured` is `ViewModel::binary_cache`).
#[must_use]
pub const fn not_cached_text(cache_configured: bool) -> &'static str {
    if cache_configured {
        "not cached: the binary cache does not have the pending closure, so there is no \
         preview until it is built and pushed"
    } else {
        "not cached: no binary cache is configured (cache.url), so the pending packages \
         cannot be known"
    }
}

/// `6.12.38`, `6.12.38, 6.12.41`, or `…` after a capped set; `—` for none.
fn versions(set: &[Option<String>], truncated: bool) -> String {
    let mut shown: String = set
        .iter()
        .map(|v| v.as_deref().unwrap_or("(none)"))
        .collect::<Vec<_>>()
        .join(", ");
    if shown.is_empty() {
        return "—".to_owned();
    }
    if truncated {
        shown.push('…');
    }
    shown
}

/// `name 6.12.38 → 6.12.41`, `name added 1.0`, `name removed 1.0`.
fn highlight_line(h: &PackageHighlight) -> String {
    match (h.before.is_empty(), h.after.is_empty()) {
        (false, false) => format!(
            "{} {} → {}",
            h.name,
            versions(&h.before, h.before_truncated),
            versions(&h.after, h.after_truncated)
        ),
        (true, false) => format!("{} added {}", h.name, versions(&h.after, h.after_truncated)),
        (false, true) => format!(
            "{} removed {}",
            h.name,
            versions(&h.before, h.before_truncated)
        ),
        (true, true) => h.name.clone(),
    }
}

fn is_critical(h: &PackageHighlight) -> bool {
    h.kinds.iter().any(|k| *k != HighlightKind::MajorVersion)
}

/// The highlights worth a line of their own, in the report's order, and how many other
/// highlights (major-version bumps) there are.
///
/// A `-unwrapped` highlight is dropped when its wrapper is highlighted too and both change the
/// same way (the same versions before and after), from the critical
/// ones and from the count alike. When the report capped its highlights the count is a lower
/// bound, which [`panel_lines`] says.
#[must_use]
pub fn split_highlights(diff: &PackageDiff) -> (Vec<&PackageHighlight>, usize) {
    let kept = diff.highlights.iter().filter(|h| {
        h.name.strip_suffix("-unwrapped").is_none_or(|wrapper| {
            !diff
                .highlights
                .iter()
                .any(|o| o.name == wrapper && o.before == h.before && o.after == h.after)
        })
    });
    let (mut critical, other): (Vec<_>, Vec<_>) = kept.partition(|h| is_critical(h));
    // The kernel, systemd, glibc, nix: the order the ticket names them in, not the report's
    // alphabetical one. Stable, so equal ranks keep the report's order.
    critical.sort_by_key(|h| {
        h.kinds
            .iter()
            .find_map(|k| match k {
                HighlightKind::Kernel => Some(0),
                HighlightKind::Systemd => Some(1),
                HighlightKind::Glibc => Some(2),
                HighlightKind::Nix => Some(3),
                HighlightKind::MajorVersion => None,
            })
            .unwrap_or(u8::MAX)
    });
    (critical, other.len())
}

/// How many packages are listed in all and how many paths could not be read, as one line, or
/// `None` when there is nothing to add to the counts.
fn caveats(diff: &PackageDiff) -> Option<String> {
    let mut notes = Vec::new();
    let unread = diff.invalid.deployed + diff.invalid.pending;
    if unread > 0 {
        notes.push(format!(
            "{unread} unreadable path{} ({} deployed, {} pending)",
            if unread == 1 { "" } else { "s" },
            diff.invalid.deployed,
            diff.invalid.pending
        ));
    }
    let t = diff.truncated;
    if t.added || t.removed || t.changed || t.highlights || t.rebuilt || t.bytes {
        notes.push("lists are capped; the counts are complete".to_owned());
    }
    (!notes.is_empty()).then(|| notes.join("; "))
}

/// What `--plain` adds to an update-pending machine's reason when its preview has no headline.
///
/// The plain sentence for `not_cached`, `failed: <why>` for a failure. `None` for a computed
/// preview (its headline is in the reason already), no preview, or any other status.
#[must_use]
pub fn plain_note(m: &MachineReport, cache_configured: bool) -> Option<String> {
    if m.status != Badge::UpdatePending {
        return None;
    }
    let diff = m.package_diff.as_ref()?;
    match diff.state {
        DiffState::Computed => None,
        DiffState::NotCached => Some(
            if cache_configured {
                "packages: not in the binary cache, no preview"
            } else {
                "packages: no binary cache configured, no preview"
            }
            .to_owned(),
        ),
        DiffState::Failed => Some(format!(
            "packages: preview failed: {}",
            diff.reason.as_deref().unwrap_or("no reason given")
        )),
    }
}

/// The most rows the detail panel gives the block of package preview lines. The rest is on
/// `d`.
pub const PANEL_LINES: usize = 4;

/// [`panel_lines`] as at most [`PANEL_LINES`] *rendered* rows of `cells` cells each.
///
/// Each value is wrapped to the width it will be drawn at, and what does not fit in the
/// budget is cut, the last row ending in `…`: the cap counts rows on screen, not entries.
#[must_use]
pub fn panel_rows(
    m: &MachineReport,
    cache_configured: bool,
    cells: usize,
) -> Vec<(&'static str, String)> {
    let mut rows: Vec<(&'static str, String)> = Vec::new();
    let mut cut_off = false;
    for (label, value) in panel_lines(m, cache_configured) {
        for (i, piece) in crate::ui::wrap(&value, cells).into_iter().enumerate() {
            if rows.len() == PANEL_LINES {
                cut_off = true;
                break;
            }
            rows.push((if i == 0 { label } else { "" }, piece));
        }
    }
    if cut_off && let Some((_, last)) = rows.last_mut() {
        let mut shown: String = last.chars().take(cells.saturating_sub(1)).collect();
        shown.push('…');
        *last = shown;
    }
    rows
}

/// One row for the detail panel, near its top, saying that a preview exists and how to open it.
///
/// `+3 −2 ~12 packages (d)`, `not cached (d)`, `failed (d)`, `checking…`; nothing for a machine
/// that is not update pending or has no preview coming. `key` is the key bound to the `d` view,
/// or `None` when it is unbound. The value is cut to `cells` with the key hint kept, so a
/// preview is always visible however short the panel is.
#[must_use]
pub fn summary_row(
    m: &MachineReport,
    key: Option<&str>,
    cells: usize,
) -> Option<(&'static str, String)> {
    if m.status != Badge::UpdatePending {
        return None;
    }
    let base = match m.package_diff.as_ref().map(|d| (d.state, d.counts)) {
        None if m.settled => return None,
        None => return Some(("diff", "checking…".to_owned())),
        Some((DiffState::Computed, c)) => {
            format!("+{} −{} ~{} packages", c.added, c.removed, c.changed)
        }
        Some((DiffState::NotCached, _)) => "not cached".to_owned(),
        Some((DiffState::Failed, _)) => "failed".to_owned(),
    };
    let hint = key.map_or_else(String::new, |k| format!(" ({k})"));
    let room = cells.saturating_sub(hint.chars().count());
    Some(("diff", format!("{}{hint}", crate::ui::cut(&base, room))))
}

/// The `reason` line for the detail panel, without the package headline.
///
/// The machine's reason minus the headline
/// that `MachineReport::with_package_diff` appended, because the panel's `diff` line carries
/// the headline (once) when a preview is computed.
#[must_use]
pub fn panel_reason(m: &MachineReport) -> Option<String> {
    let reason = m.reason.clone()?;
    let Some(headline) = m.package_diff.as_ref().and_then(diff_headline) else {
        return Some(reason);
    };
    if m.status != Badge::UpdatePending {
        return Some(reason);
    }
    // `with_package_diff` inserts "; <headline>" before the first ';' of the reason, or at
    // its end; as the only text it is the whole reason.
    Some(if reason == headline {
        "—".to_owned()
    } else {
        reason.replacen(&format!("; {headline}"), "", 1)
    })
}

/// The detail panel's lines for the package preview.
///
/// `(label, value)`, at most [`PANEL_LINES`] entries (see [`panel_rows`] for the cap in rendered
/// rows), or nothing for a machine that is not update pending or has no preview to
/// show (and is settled, so none is coming).
///
/// Computed: the headline (once; see [`panel_reason`]), the critical highlights with
/// versions and what was rebuilt, a count of the other major-version changes, then the
/// counts with a note on unreadable paths or capped lists. `cache_configured` is
/// `ViewModel::binary_cache`.
#[must_use]
pub fn panel_lines(m: &MachineReport, cache_configured: bool) -> Vec<(&'static str, String)> {
    if m.status != Badge::UpdatePending {
        return Vec::new();
    }
    let Some(diff) = &m.package_diff else {
        // Unknown yet. Once the machine has settled and still none, the preview is off.
        return if m.settled {
            Vec::new()
        } else {
            vec![("diff", "checking…".to_owned())]
        };
    };
    match diff.state {
        DiffState::NotCached => {
            return vec![("diff", not_cached_text(cache_configured).to_owned())];
        }
        DiffState::Failed => {
            return vec![(
                "diff",
                format!(
                    "failed: {}",
                    diff.reason.as_deref().unwrap_or("no reason given")
                ),
            )];
        }
        DiffState::Computed => {}
    }
    let mut out = Vec::new();
    if let Some(headline) = diff_headline(diff) {
        out.push(("diff", headline));
    }
    let (critical, major) = split_highlights(diff);
    let mut critical: Vec<String> = critical.into_iter().map(highlight_line).collect();
    critical.extend(diff.rebuilt.iter().map(|name| format!("{name} rebuilt")));
    if !critical.is_empty() {
        out.push(("critical", critical.join(", ")));
    }
    // The major-version count shares the counts row: the block is a few rows, and the full
    // list is on `d`.
    let c = diff.counts;
    let mut counts = String::new();
    if major > 0 {
        let _ = write!(
            counts,
            "{}{major} other major-version change{}; ",
            if diff.truncated.highlights {
                "at least "
            } else {
                ""
            },
            if major == 1 { "" } else { "s" }
        );
    }
    let _ = write!(
        counts,
        "+{} −{} ~{}  {} rebuilt",
        c.added,
        c.removed,
        c.changed,
        diff.rebuilt.len()
    );
    if diff.unversioned.added + diff.unversioned.removed > 0 {
        let _ = write!(
            counts,
            "  unversioned +{} −{}",
            diff.unversioned.added, diff.unversioned.removed
        );
    }
    if let Some(note) = caveats(diff) {
        let _ = write!(counts, "; {note}");
    }
    out.push(("counts", counts));
    out
}

/// Whether `c` is `X-unwrapped` and `X` is in the same list with the same versions: one line for
/// both.
fn folded_unwrapped(c: &PackageChange, list: &[PackageChange]) -> bool {
    c.name.strip_suffix("-unwrapped").is_some_and(|wrapper| {
        list.iter()
            .any(|o| o.name == wrapper && o.before == c.before && o.after == c.after)
    })
}

/// Whether `list` has `c-unwrapped` with the same versions as `c`.
fn has_unwrapped(c: &PackageChange, list: &[PackageChange]) -> bool {
    list.iter().any(|o| {
        o.name.strip_suffix("-unwrapped") == Some(c.name.as_str())
            && o.before == c.before
            && o.after == c.after
    })
}

fn change_line(c: &PackageChange) -> String {
    let before = versions(&c.before, c.before_truncated);
    let after = versions(&c.after, c.after_truncated);
    match (c.before.is_empty(), c.after.is_empty()) {
        (false, false) => format!("{}  {before} → {after}", c.name),
        (true, _) => format!("{}  {after}", c.name),
        (_, true) => format!("{}  {before}", c.name),
    }
}

/// The `d` view: every changed, added and removed package the report lists.
///
/// Under a heading each, or the sentence for a preview that has none. The lists are the
/// report's own, capped at 128 each; a note says so when a list was cut.
#[must_use]
pub fn full_lines(m: &MachineReport, cache_configured: bool) -> Vec<(Kind, String)> {
    if m.status != Badge::UpdatePending {
        return vec![(
            Kind::Note,
            format!(
                "{} is {}: there is no update to preview",
                m.name,
                m.status.label()
            ),
        )];
    }
    let Some(diff) = &m.package_diff else {
        return vec![(
            Kind::Note,
            if m.settled {
                "no package preview was gathered (it is switched off: diff.enabled)".to_owned()
            } else {
                "checking…".to_owned()
            },
        )];
    };
    match diff.state {
        DiffState::NotCached => {
            return vec![(Kind::Note, not_cached_text(cache_configured).to_owned())];
        }
        DiffState::Failed => {
            return vec![(
                Kind::Note,
                format!(
                    "the preview failed: {}",
                    diff.reason.as_deref().unwrap_or("no reason given")
                ),
            )];
        }
        DiffState::Computed => {}
    }
    let mut out = Vec::new();
    if let Some(headline) = diff_headline(diff) {
        out.push((Kind::Note, headline));
    }
    let sections = [
        (
            "changed",
            diff.counts.changed,
            &diff.changed,
            diff.truncated.changed,
        ),
        (
            "added",
            diff.counts.added,
            &diff.added,
            diff.truncated.added,
        ),
        (
            "removed",
            diff.counts.removed,
            &diff.removed,
            diff.truncated.removed,
        ),
    ];
    for (title, count, list, truncated) in sections {
        if count == 0 && list.is_empty() {
            continue;
        }
        out.push((Kind::Heading, format!("{title} ({count})")));
        out.extend(list.iter().filter(|c| !folded_unwrapped(c, list)).map(|c| {
            let more = if has_unwrapped(c, list) {
                " (+ unwrapped)"
            } else {
                ""
            };
            (Kind::Item, format!("{}{more}", change_line(c)))
        }));
        if truncated {
            out.push((
                Kind::Note,
                format!(
                    "… {} not listed (the list is capped)",
                    count.saturating_sub(list.len())
                ),
            ));
        }
    }
    if !diff.rebuilt.is_empty() {
        out.push((Kind::Heading, format!("rebuilt ({})", diff.rebuilt.len())));
        out.extend(
            diff.rebuilt
                .iter()
                .map(|n| (Kind::Item, format!("{n}  same version, new build"))),
        );
    }
    if let Some(note) = caveats(diff) {
        out.push((Kind::Note, note));
    }
    out
}

#[cfg(test)]
mod tests {
    use auspex_domain::diff::diff_closures;

    use super::*;

    fn path(c: char, name: &str) -> String {
        format!("/nix/store/{c}123456789abcdfghijklmnpqrsvwxyz-{name}")
    }

    fn highlight(name: &str, kinds: &[HighlightKind]) -> PackageHighlight {
        PackageHighlight {
            name: name.to_owned(),
            kinds: kinds.to_vec(),
            before: vec![Some("1.0".to_owned())],
            after: vec![Some("2.0".to_owned())],
            before_truncated: false,
            after_truncated: false,
        }
    }

    #[test]
    fn unwrapped_variants_fold_into_their_wrapper_and_majors_are_counted() {
        let mut diff = PackageDiff::not_cached();
        diff.highlights = vec![
            highlight(
                "linux",
                &[HighlightKind::Kernel, HighlightKind::MajorVersion],
            ),
            highlight("firefox", &[HighlightKind::MajorVersion]),
            highlight("firefox-unwrapped", &[HighlightKind::MajorVersion]),
            // No wrapper highlighted: stays.
            highlight("thunderbird-unwrapped", &[HighlightKind::MajorVersion]),
            highlight("systemd", &[HighlightKind::Systemd]),
        ];
        let (critical, major) = split_highlights(&diff);
        let names: Vec<&str> = critical.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["linux", "systemd"]);
        // Out of order in the report, in the ticket's order on screen.
        diff.highlights.reverse();
        let (critical, _) = split_highlights(&diff);
        let names: Vec<&str> = critical.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["linux", "systemd"]);
        assert_eq!(major, 2, "firefox and thunderbird-unwrapped");
    }

    #[test]
    fn an_unwrapped_variant_that_changes_differently_is_not_folded() {
        let mut diff = PackageDiff::not_cached();
        let mut other = highlight("firefox-unwrapped", &[HighlightKind::MajorVersion]);
        other.after = vec![Some("3.0".to_owned())];
        diff.highlights = vec![highlight("firefox", &[HighlightKind::MajorVersion]), other];
        assert_eq!(split_highlights(&diff).1, 2);
    }

    #[test]
    fn the_full_list_folds_an_unwrapped_pair_and_keeps_a_different_one() {
        let change = |name: &str, after: &str| PackageChange {
            name: name.to_owned(),
            before: vec![Some("1.0".to_owned())],
            after: vec![Some(after.to_owned())],
            before_truncated: false,
            after_truncated: false,
        };
        let list = [
            change("chromium", "2.0"),
            change("chromium-unwrapped", "2.0"),
            change("electron", "2.0"),
            change("electron-unwrapped", "3.0"),
        ];
        let folded: Vec<bool> = list.iter().map(|c| folded_unwrapped(c, &list)).collect();
        assert_eq!(folded, [false, true, false, false]);
        assert!(has_unwrapped(&list[0], &list));
        assert!(!has_unwrapped(&list[2], &list));
    }

    #[test]
    fn a_critical_unwrapped_highlight_follows_the_same_rule() {
        let mut diff = PackageDiff::not_cached();
        diff.highlights = vec![
            highlight("nix", &[HighlightKind::Nix]),
            highlight("nix-unwrapped", &[HighlightKind::Nix]),
        ];
        let (critical, major) = split_highlights(&diff);
        assert_eq!(critical.len(), 1);
        assert_eq!(critical[0].name, "nix");
        assert_eq!(major, 0);
    }

    #[test]
    fn the_full_list_has_every_changed_added_and_removed_package() {
        let deployed = [
            path('a', "linux-6.12.38"),
            path('b', "htop-3.3.0"),
            path('c', "zlib-1.3.1"),
        ];
        let pending = [
            path('d', "linux-6.12.41"),
            path('f', "ripgrep-14.1.1"),
            path('c', "zlib-1.3.1"),
        ];
        let diff = diff_closures(&deployed, Some(&pending)).unwrap().unwrap();
        let mut m = report_with(&diff);
        m.settled = true;
        let lines: Vec<String> = full_lines(&m, true).into_iter().map(|(_, l)| l).collect();
        assert!(lines.contains(&"changed (1)".to_owned()), "{lines:?}");
        assert!(
            lines.contains(&"linux  6.12.38 → 6.12.41".to_owned()),
            "{lines:?}"
        );
        assert!(lines.contains(&"ripgrep  14.1.1".to_owned()), "{lines:?}");
        assert!(lines.contains(&"htop  3.3.0".to_owned()), "{lines:?}");
    }

    fn report_with(diff: &PackageDiff) -> MachineReport {
        let json = serde_json::json!({
            "name": "luna", "class": "nixos", "tags": [], "target": null,
            "target_source": "machine_name", "target_error": null, "build_host": null,
            "status": "update_pending", "badges": ["update_pending"], "settled": false,
            "reason": null, "current_system": null, "pending_system": null,
            "package_diff": diff, "deployed_revision": null, "system_state": null,
            "failed_units": null,
            "health": {"uptime_seconds": null, "root_disk": null, "load": null, "memory": null},
            "lints": [], "frozen": false
        });
        serde_json::from_value(json).unwrap()
    }

    fn computed_with(f: impl FnOnce(&mut PackageDiff)) -> MachineReport {
        let deployed = [path('a', "linux-6.12.38"), path('c', "zlib-1.3.1")];
        let pending = [path('d', "linux-6.12.41"), path('c', "zlib-1.3.1")];
        let mut diff = diff_closures(&deployed, Some(&pending)).unwrap().unwrap();
        f(&mut diff);
        let mut m = report_with(&diff);
        m.settled = true;
        m
    }

    #[test]
    fn the_panel_is_at_most_four_lines_and_says_at_least_when_capped() {
        let m = computed_with(|d| {
            d.highlights = vec![
                highlight("linux", &[HighlightKind::Kernel]),
                highlight("systemd", &[HighlightKind::Systemd]),
                highlight("glibc", &[HighlightKind::Glibc]),
                highlight("firefox", &[HighlightKind::MajorVersion]),
            ];
            d.rebuilt = vec!["nix".to_owned()];
            d.unversioned.added = 2;
            d.invalid.pending = 3;
            d.truncated.highlights = true;
        });
        let lines = panel_lines(&m, true);
        assert!(lines.len() <= PANEL_LINES, "{lines:?}");
        // Counted in rendered rows, at any width.
        for cells in [12, 30, 62, 200] {
            let rows = panel_rows(&m, true, cells);
            assert!(rows.len() <= PANEL_LINES, "{cells}: {rows:?}");
            assert!(
                rows.iter().all(|(_, v)| v.chars().count() <= cells),
                "{rows:?}"
            );
        }
        let narrow = panel_rows(&m, true, 20);
        assert_eq!(narrow.len(), PANEL_LINES);
        assert!(narrow.last().unwrap().1.ends_with('…'), "{narrow:?}");
        assert!(
            !panel_rows(&m, true, 200)
                .iter()
                .any(|(_, v)| v.ends_with('…'))
        );
        let (label, value) = summary_row(&m, Some("d"), 40).unwrap();
        assert_eq!(label, "diff");
        assert!(
            value.ends_with(" (d)") && value.starts_with("+0 −0 ~1"),
            "{value}"
        );
        // However narrow, the hint stays.
        assert!(summary_row(&m, Some("d"), 9).unwrap().1.ends_with(" (d)"));
        assert_eq!(summary_row(&m, None, 40).unwrap().1, "+0 −0 ~1 packages");
        let text: Vec<String> = lines.iter().map(|(l, v)| format!("{l} {v}")).collect();
        assert!(
            text.iter()
                .any(|l| l.starts_with("counts at least 1 other major-version change; +")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|l| l.starts_with("critical linux") && l.ends_with("nix rebuilt")),
            "{text:?}"
        );
        // The unreadable-path note counts the pending side too.
        assert!(
            text.iter()
                .any(|l| l.contains("3 unreadable paths (0 deployed, 3 pending)")),
            "{text:?}"
        );
        let m = computed_with(|d| d.invalid.deployed = 1);
        let text: Vec<String> = panel_lines(&m, true)
            .iter()
            .map(|(l, v)| format!("{l} {v}"))
            .collect();
        assert!(
            text.iter()
                .any(|l| l.contains("1 unreadable path (1 deployed, 0 pending)")),
            "{text:?}"
        );
    }

    #[test]
    fn the_headline_is_in_the_panel_once() {
        let mut m = computed_with(|_| {});
        let headline = diff_headline(m.package_diff.as_ref().unwrap()).unwrap();
        m.reason = Some(format!(
            "pending nixos-system-x; {headline}; failed units: a"
        ));
        assert_eq!(
            panel_reason(&m).as_deref(),
            Some("pending nixos-system-x; failed units: a")
        );
        m.reason = Some(headline.clone());
        assert_eq!(panel_reason(&m).as_deref(), Some("—"));
        assert_eq!(panel_lines(&m, true)[0], ("diff", headline));
    }

    #[test]
    fn states_without_a_list_say_so_plainly() {
        let m = report_with(&PackageDiff::not_cached());
        assert_eq!(
            panel_lines(&m, true),
            [("diff", not_cached_text(true).to_owned())]
        );
        assert_eq!(
            panel_lines(&m, false),
            [("diff", not_cached_text(false).to_owned())]
        );
        assert_ne!(not_cached_text(true), not_cached_text(false));
        assert_eq!(
            plain_note(&m, true).as_deref(),
            Some("packages: not in the binary cache, no preview")
        );
        assert_eq!(
            plain_note(&m, false).as_deref(),
            Some("packages: no binary cache configured, no preview")
        );
        let m = report_with(&PackageDiff::failed("ssh: Connection refused"));
        assert_eq!(
            panel_lines(&m, true),
            [("diff", "failed: ssh: Connection refused".to_owned())]
        );
        assert_eq!(
            full_lines(&m, true),
            [(
                Kind::Note,
                "the preview failed: ssh: Connection refused".to_owned()
            )]
        );
    }
}
