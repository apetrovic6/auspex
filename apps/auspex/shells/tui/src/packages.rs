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
//! - a `-unwrapped` variant is not a highlight of its own when its wrapper is one too: the
//!   operator reads one line for `firefox`, not two;
//! - every other highlight is a major-version bump, and many of those are a count, not a
//!   list: the full list (`d`) has them all.

use std::fmt::Write as _;

use auspex_domain::diff::{DiffState, HighlightKind, PackageChange, PackageDiff, PackageHighlight};
use auspex_domain::report::{MachineReport, diff_headline};
use auspex_domain::status::Badge;

/// What a line of the full list is, for styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A section title (`changed (10)`).
    Heading,
    /// One package.
    Item,
    /// Something to read, not a package: a state, or a note about what is cut.
    Note,
}

/// The sentence for a preview that has no list because the pending closure is not available.
pub const NOT_CACHED: &str = "not cached: the pending closure is not in the binary cache (or \
                              no cache is configured), so there is no preview until it is \
                              built and pushed";

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
/// A `-unwrapped` highlight is dropped when its wrapper is highlighted too, from the critical
/// ones and from the count alike. When the report capped its highlights the count is a lower
/// bound, which [`panel_lines`] says.
#[must_use]
pub fn split_highlights(diff: &PackageDiff) -> (Vec<&PackageHighlight>, usize) {
    let kept = diff.highlights.iter().filter(|h| {
        h.name
            .strip_suffix("-unwrapped")
            .is_none_or(|wrapper| !diff.highlights.iter().any(|o| o.name == wrapper))
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
pub fn plain_note(m: &MachineReport) -> Option<String> {
    if m.status != Badge::UpdatePending {
        return None;
    }
    let diff = m.package_diff.as_ref()?;
    match diff.state {
        DiffState::Computed => None,
        DiffState::NotCached => Some("packages: not cached, no preview".to_owned()),
        DiffState::Failed => Some(format!(
            "packages: preview failed: {}",
            diff.reason.as_deref().unwrap_or("no reason given")
        )),
    }
}

/// The detail panel's lines for the package preview: `(label, value)`, or nothing for a
/// machine that is not update pending or has no preview to show (and is settled, so none is
/// coming).
///
/// Order: the headline, the critical highlights with versions, a count of the other
/// major-version changes, then the counts.
#[must_use]
pub fn panel_lines(m: &MachineReport) -> Vec<(&'static str, String)> {
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
        DiffState::NotCached => return vec![("diff", NOT_CACHED.to_owned())],
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
    for (i, h) in critical.into_iter().enumerate() {
        out.push((if i == 0 { "critical" } else { "" }, highlight_line(h)));
    }
    for name in &diff.rebuilt {
        out.push(("", format!("{name} rebuilt (same version, new build)")));
    }
    if major > 0 {
        out.push((
            "major",
            format!(
                "{}{major} other major-version change{}",
                if diff.truncated.highlights {
                    "at least "
                } else {
                    ""
                },
                if major == 1 { "" } else { "s" }
            ),
        ));
    }
    let c = diff.counts;
    let mut counts = format!(
        "+{} added  −{} removed  ~{} changed  {} rebuilt",
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
    out.push(("counts", counts));
    if let Some(note) = caveats(diff) {
        out.push(("", note));
    }
    out
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
pub fn full_lines(m: &MachineReport) -> Vec<(Kind, String)> {
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
        DiffState::NotCached => return vec![(Kind::Note, NOT_CACHED.to_owned())],
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
        out.extend(list.iter().map(|c| (Kind::Item, change_line(c))));
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
        let lines: Vec<String> = full_lines(&m).into_iter().map(|(_, l)| l).collect();
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

    #[test]
    fn states_without_a_list_say_so_plainly() {
        let m = report_with(&PackageDiff::not_cached());
        assert_eq!(panel_lines(&m), [("diff", NOT_CACHED.to_owned())]);
        let m = report_with(&PackageDiff::failed("ssh: Connection refused"));
        assert_eq!(
            panel_lines(&m),
            [("diff", "failed: ssh: Connection refused".to_owned())]
        );
        assert_eq!(
            full_lines(&m),
            [(
                Kind::Note,
                "the preview failed: ssh: Connection refused".to_owned()
            )]
        );
    }
}
