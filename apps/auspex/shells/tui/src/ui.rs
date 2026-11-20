//! Drawing the view model: the list, the detail panel, the status bar and the overlays.
//!
//! Pure over its inputs (the view model, the shell's interaction state, the theme, the live
//! keymap, the log and a frame counter), so the snapshot tests draw it on ratatui's
//! `TestBackend` with no terminal and no clock. Spinners animate from the frame counter.
//!
//! Every string drawn from outside (machine names, reasons, targets, tags, notes) comes from
//! the domain's report or from the log, both of which sanitised it where it entered.
//! Widths are measured in terminal cells and everything is cut with an ellipsis, so a long
//! or wide name never moves a column.

use std::time::{Duration, Instant};

use auspex_core::{
    ActionResult, ActionStatus, ActionView, ConfirmKind, ConfirmView, Event, Mode, Phase,
    PrometheusStatus, ViewModel,
};
use auspex_domain::fleet::{Lint, TargetSource};
use auspex_domain::observe::{HealthSource, StorePath};
use auspex_domain::report::MachineReport;
use auspex_domain::status::Badge;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::input::{Input, Prompt, Screen};
use crate::keymap::{Action, Key, Keymap};
use crate::output::{Output, Stream};
use crate::packages::{self, Kind};
use crate::theme::{Role, Theme};

/// At this width or more, list and detail sit side by side.
pub const WIDE: u16 = 100;

/// The badge column's width in cells: room for `↑◆⚠⟳`.
pub const BADGE_CELLS: usize = 4;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Frames per spinner step (at the TUI's 30 frames a second, about 7 steps a second).
const FRAMES_PER_STEP: u64 = 4;

/// The spinner glyph for `frame`.
#[must_use]
pub fn spinner(frame: u64) -> &'static str {
    let step = usize::try_from(frame / FRAMES_PER_STEP).unwrap_or(0) % SPINNER.len();
    SPINNER[step]
}

/// The shell-side state the drawing needs besides the view model.
#[derive(Debug, Clone, Default)]
pub struct Ui {
    /// Keys, prompts and the shell's own overlays.
    pub input: Input,
    /// The list's first visible row, kept between frames so the list scrolls only when the
    /// selection leaves it.
    pub offset: usize,
    /// The list's visible height in the last frame, for half pages.
    pub page: usize,
    /// Frames drawn, for the spinners.
    pub frame: u64,
    /// Probes currently waiting in the SSH connect/authentication phase.
    pub approvals: usize,
    /// The last thing auspex could not do besides the action itself (the action log failed).
    pub warning: Option<String>,
    /// The question the last frame drew whole (its `ConfirmView::serial`), and when a frame
    /// first did. A question is answered only after it has been on screen for
    /// [`QUESTION_DWELL`]: see [`Ui::key`].
    pub confirm_seen: Option<(u64, Instant)>,
}

/// How long a question must have been on screen, whole, before a key may answer it.
///
/// A frame is drawn every 33 ms, so "a frame has drawn it" alone is passed by a `y` typed
/// right behind the `u` by habit (80 to 150 ms between keys), and over a slow link a frame can
/// still be in flight. The question names the machine and the system: it has to be readable
/// first. A deliberate operator loses 0.3 s.
pub const QUESTION_DWELL: Duration = Duration::from_millis(300);

impl Ui {
    /// Handles one key against `view`: the events to send, in order.
    ///
    /// While a question is open and has not been on screen, whole, for [`QUESTION_DWELL`], the
    /// key is discarded, not answered. The question names the machine and the system; an answer
    /// typed before it could be read (typeahead, `Ayuy` in one burst, a `y` typed right behind
    /// the `u` by habit) is not an answer to it. Every question is a new one to draw and wait
    /// for. `now` is when the key arrived.
    pub fn key(
        &mut self,
        key: Key,
        view: &ViewModel,
        since_refresh: Duration,
        keymap: &Keymap,
        now: Instant,
    ) -> Vec<Event> {
        if self.question_unseen(view, now) {
            return Vec::new();
        }
        let query = view
            .search
            .as_ref()
            .map(|s| s.query.clone())
            .unwrap_or_default();
        let screen = Screen::from_view(view, self.page.max(1), since_refresh);
        self.input.key(key, screen, keymap, &query)
    }

    /// Handles pasted text (bracketed paste, so it is not typed): never an answer to a
    /// question except no, and never a command.
    ///
    /// With a question open, a paste is a no once the question could be read (and nothing
    /// before). With a prompt open, the text goes into it as typed, without control characters
    /// (a pasted newline must not submit it). Otherwise it is ignored: pasted `uy` is not `u`
    /// and `y`.
    pub fn paste(
        &mut self,
        text: &str,
        view: &ViewModel,
        since_refresh: Duration,
        keymap: &Keymap,
        now: Instant,
    ) -> Vec<Event> {
        if view.confirm.is_some() {
            return if self.question_unseen(view, now) {
                Vec::new()
            } else {
                vec![Event::ConfirmAnswer(false)]
            };
        }
        if self.input.prompt.is_none() {
            return Vec::new();
        }
        let mut events = Vec::new();
        for c in text.chars().filter(|c| !c.is_control()) {
            events.extend(self.key(Key::char(c), view, since_refresh, keymap, now));
        }
        events
    }

    /// A question is open that has not been on screen, whole, for [`QUESTION_DWELL`] at `now`.
    fn question_unseen(&self, view: &ViewModel, now: Instant) -> bool {
        view.confirm
            .as_ref()
            .is_some_and(|c| match self.confirm_seen {
                Some((serial, since)) if serial == c.serial => {
                    now.saturating_duration_since(since) < QUESTION_DWELL
                }
                _ => true,
            })
    }
}

/// Everything that does not change frame to frame.
pub struct Context<'a> {
    /// Colours.
    pub theme: &'a Theme,
    /// For the help overlay.
    pub keymap: &'a Keymap,
    /// The fleet flake, as given.
    pub flake: &'a str,
    /// The log, oldest first.
    pub log: &'a [String],
    /// The streamed output of updates and builds, for the Log tab.
    pub output: &'a Output,
    /// When this frame is drawn: the time a question is first on screen is read from it.
    pub now: Instant,
}

/// `s` cut to `width` cells, with `…` when cut, and padded with spaces to exactly `width`.
#[must_use]
pub fn fit(s: &str, width: usize) -> String {
    let w = s.width();
    if w <= width {
        return format!("{s}{}", " ".repeat(width - w));
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > width - 1 {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    used += 1;
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// `s` cut to `width` cells with `…`, not padded.
#[must_use]
pub fn cut(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    fit(s, width).trim_end().to_owned()
}

/// A row's badges, primary first, each with its glyph (a spinner for the two pending ones).
#[must_use]
pub fn badge_glyphs(m: &MachineReport, frame: u64) -> Vec<(Badge, &'static str)> {
    let glyph = |b: Badge| b.glyph().unwrap_or_else(|| spinner(frame));
    std::iter::once(m.status)
        .chain(m.badges.iter().copied().filter(|b| *b != m.status))
        .map(|b| (b, glyph(b)))
        .collect()
}

/// The badge cell: glyphs in their colours, padded to [`BADGE_CELLS`].
fn badge_spans(m: &MachineReport, frame: u64, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used = 0;
    for (b, g) in badge_glyphs(m, frame) {
        let w = g.width();
        if used + w > BADGE_CELLS {
            break;
        }
        used += w;
        spans.push(Span::styled(g, theme.badge(b)));
    }
    spans.push(Span::raw(" ".repeat(BADGE_CELLS - used)));
    spans
}

/// Draws one frame.
pub fn draw(frame: &mut Frame, view: &ViewModel, ui: &mut Ui, ctx: &Context<'_>) {
    let area = frame.area();
    let theme = ctx.theme;
    frame.render_widget(Block::default().style(theme.base()), area);
    let [main, prompt_row, status_row] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    if area.width >= WIDE {
        let [list, detail] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(main);
        draw_list(frame, list, view, ui, theme);
        draw_detail(frame, detail, view, ctx);
    } else if ui.input.detail_open {
        draw_detail(frame, main, view, ctx);
    } else {
        draw_list(frame, main, view, ui, theme);
    }

    draw_prompt_row(frame, prompt_row, view, ui, ctx);
    draw_status(frame, status_row, view, ui, ctx);

    if ui.input.log_open {
        draw_log(frame, main, ui, ctx);
    }
    if ui.input.diff_open {
        draw_diff(frame, main, view, ui, ctx);
    }
    if view.help_open {
        draw_help(frame, main, ctx);
    }
    // A question counts as drawn only if all of it fitted; one that did not (a small pane, a
    // resize that shrank it) starts its dwell afresh when it does.
    let whole = draw_confirm(frame, main, view, ctx);
    ui.confirm_seen = match (&view.confirm, whole) {
        (Some(c), true) => match ui.confirm_seen {
            Some((serial, since)) if serial == c.serial => Some((serial, since)),
            _ => Some((c.serial, ctx.now)),
        },
        _ => None,
    };
}

fn draw_list(frame: &mut Frame, area: Rect, view: &ViewModel, ui: &mut Ui, theme: &Theme) {
    let machines = &view.report.machines;
    let title = format!(" machines ({}) ", machines.len());
    let block = Block::bordered()
        .title(Span::styled(title, theme.style(Role::Title)))
        .border_style(theme.style(Role::Dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = usize::from(inner.height);
    ui.page = height;
    if machines.is_empty() {
        let what = match view.refresh.phase {
            Phase::NotStarted => "starting…",
            Phase::ListingMachines => "listing machines…",
            Phase::Scanning | Phase::Settled => "no machines",
        };
        frame.render_widget(
            Paragraph::new(Span::styled(what, theme.style(Role::Dim))),
            inner,
        );
        return;
    }
    let selected = view.selected.unwrap_or(0);
    if selected < ui.offset {
        ui.offset = selected;
    } else if height > 0 && selected >= ui.offset + height {
        ui.offset = selected + 1 - height;
    }
    ui.offset = ui.offset.min(machines.len().saturating_sub(height.max(1)));

    let width = usize::from(inner.width);
    let longest = machines.iter().map(|m| m.name.width()).max().unwrap_or(0);
    // Name column: as wide as the longest name, but never more than 40% of the row, so the
    // status always has room.
    let name_cells = longest.min((width * 2 / 5).max(8));
    let matches = view
        .search
        .as_ref()
        .map_or(&[][..], |s| s.matches.as_slice());

    let lines: Vec<Line> = machines
        .iter()
        .enumerate()
        .skip(ui.offset)
        .take(height)
        .map(|(i, m)| {
            // A marker as well as the highlight, so the selection reads without colour.
            let mut spans = vec![Span::raw(if i == selected { "›" } else { " " })];
            spans.extend(badge_spans(m, ui.frame, theme));
            spans.push(Span::raw(" "));
            let name_style = if matches.contains(&i) {
                theme.style(Role::Match)
            } else {
                theme.style(Role::Text)
            };
            spans.push(Span::styled(fit(&m.name, name_cells), name_style));
            spans.push(Span::raw(" "));
            let rest = width.saturating_sub(1 + BADGE_CELLS + 1 + name_cells + 1);
            spans.push(Span::styled(
                fit(m.status.label(), rest),
                theme.style(Role::Dim),
            ));
            let line = Line::from(spans);
            if i == selected {
                line.style(theme.style(Role::Selected))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

const fn source(s: TargetSource) -> &'static str {
    match s {
        TargetSource::Override => "config override",
        TargetSource::Inventory => "inventory deploy.targetHost",
        TargetSource::MachineName => "machine name",
    }
}

/// A store path as hash prefix and name: `0a1b2c3d nixos-system-terra-25.11`.
#[must_use]
pub fn short_path(path: &str) -> String {
    StorePath::new(path).map_or_else(
        |_| path.to_owned(),
        |p| format!("{} {}", &p.hash()[..8], p.name()),
    )
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[expect(clippy::cast_precision_loss, reason = "a display value, one decimal")]
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn duration(seconds: u64) -> String {
    let (d, h, m) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m {}s", seconds % 60)
    }
}

const fn health_source(s: HealthSource) -> &'static str {
    match s {
        HealthSource::Prometheus => "prometheus",
        HealthSource::Ssh => "ssh",
    }
}

/// The detail panel's lines for one machine: label, value, and an optional source.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "one entry per line of the panel, in the panel's order"
)]
pub fn detail_lines(
    m: &MachineReport,
    cache_configured: bool,
    diff_key: Option<&str>,
    value_cells: usize,
) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("status", m.status.label().to_owned()),
        (
            "reason",
            packages::panel_reason(m).unwrap_or_else(|| "—".to_owned()),
        ),
        (
            "host",
            format!(
                "{} ({})",
                m.target.as_deref().unwrap_or("—"),
                source(m.target_source)
            ),
        ),
    ];
    // One row for the preview, right after the reason: health comes before the full block below,
    // which a short panel cuts, so this is what says a preview exists.
    if let Some(row) = packages::summary_row(m, diff_key, value_cells) {
        out.insert(2, row);
    }
    if let Some(e) = &m.target_error {
        out.push(("refused", e.clone()));
    }
    if let Some(b) = &m.build_host {
        out.push(("build", b.clone()));
    }
    out.push((
        "class",
        format!(
            "{}{}",
            match m.class {
                auspex_domain::fleet::MachineClass::Nixos => "nixos",
                auspex_domain::fleet::MachineClass::Darwin => "darwin",
            },
            {
                let tags: Vec<&str> = m.display_tags().collect();
                if tags.is_empty() {
                    String::new()
                } else {
                    format!("  tags: {}", tags.join(", "))
                }
            }
        ),
    ));
    if m.lints.is_empty() {
        out.push(("lints", "none".to_owned()));
    }
    for lint in &m.lints {
        let text = match lint {
            Lint::HostnameTarget { host } => format!("hostname target {host} (prefer an IP)"),
            Lint::SharedIp { ip, with } => {
                format!("{ip} is also {}: deploys refused", with.join(", "))
            }
        };
        out.push(("lint", text));
    }
    out.push((
        "current",
        m.current_system
            .as_deref()
            .map_or_else(|| "—".to_owned(), short_path),
    ));
    out.push((
        "pending",
        m.pending_system
            .as_deref()
            .map_or_else(|| "—".to_owned(), short_path),
    ));
    out.push((
        "revision",
        m.deployed_revision
            .clone()
            .unwrap_or_else(|| "not set".to_owned()),
    ));
    if let Some(state) = &m.system_state {
        out.push(("systemd", state.clone()));
    }
    out.push((
        "failed",
        match &m.failed_units {
            None => "unknown".to_owned(),
            Some(u) if u.is_empty() => "none".to_owned(),
            Some(u) => u.join(", "),
        },
    ));
    let h = &m.health;
    out.push((
        "uptime",
        h.uptime_seconds.map_or_else(
            || "—".to_owned(),
            |s| format!("{} ({})", duration(s.value), health_source(s.source)),
        ),
    ));
    out.push((
        "disk",
        h.root_disk.map_or_else(
            || "—".to_owned(),
            |s| {
                let d = s.value;
                format!(
                    "{} of {} used, {} free ({})",
                    bytes(d.used_bytes),
                    bytes(d.total_bytes),
                    bytes(d.available_bytes),
                    health_source(s.source)
                )
            },
        ),
    ));
    out.push((
        "load",
        h.load.map_or_else(
            || "—".to_owned(),
            |s| {
                format!(
                    "{:.2} {:.2} {:.2} ({})",
                    s.value.one,
                    s.value.five,
                    s.value.fifteen,
                    health_source(s.source)
                )
            },
        ),
    ));
    out.push((
        "memory",
        h.memory.map_or_else(
            || "—".to_owned(),
            |s| {
                format!(
                    "{} of {} free ({})",
                    bytes(s.value.available_bytes),
                    bytes(s.value.total_bytes),
                    health_source(s.source)
                )
            },
        ),
    ));
    // The package preview comes last, capped at a few lines: failed units and the systemd
    // state matter most just before an update, and the full list is on `d`.
    out.extend(packages::panel_rows(m, cache_configured, value_cells));
    out
}

const LABEL_CELLS: usize = 9;

fn draw_detail(frame: &mut Frame, area: Rect, view: &ViewModel, ctx: &Context<'_>) {
    let theme = ctx.theme;
    let m = view.selected.and_then(|i| view.report.machines.get(i));
    let title = m.map_or_else(|| " detail ".to_owned(), |m| format!(" {} ", m.name));
    let block = Block::bordered()
        .title(Span::styled(
            cut(&title, usize::from(area.width.saturating_sub(4))),
            theme.style(Role::Title),
        ))
        .border_style(theme.style(Role::Dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(m) = m else {
        return;
    };
    let width = usize::from(inner.width);
    let value_cells = width.saturating_sub(LABEL_CELLS + 1).max(1);
    let mut lines = Vec::new();
    let mut badges = vec![
        Span::styled(fit("badges", LABEL_CELLS), theme.style(Role::Dim)),
        Span::raw(" "),
    ];
    for (b, g) in badge_glyphs(m, 0) {
        badges.push(Span::styled(g, theme.badge(b)));
        badges.push(Span::styled(
            format!(" {}  ", b.label()),
            theme.style(Role::Text),
        ));
    }
    lines.push(Line::from(badges));
    let room = usize::from(inner.height);
    let diff_key = ctx.keymap.keys_for(Action::Diff).into_iter().next();
    for (label, value) in detail_lines(m, view.binary_cache, diff_key.as_deref(), value_cells) {
        if lines.len() >= room {
            break; // nothing below the panel is drawn, so nothing below it is wrapped
        }
        // Values wrap onto continuation lines under themselves, never under the labels.
        let mut first = true;
        for chunk in wrap(&cap_value(&value), value_cells) {
            let label = if first { label } else { "" };
            first = false;
            lines.push(Line::from(vec![
                Span::styled(fit(label, LABEL_CELLS), theme.style(Role::Dim)),
                Span::raw(" "),
                Span::styled(chunk, theme.style(Role::Text)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The most of any one detail value that is wrapped: 2 KiB.
///
/// A deployed revision is 40
/// characters and a reason one line, but both come from a remote host (the probe allows
/// 256 KiB of output), and 2 KiB is already about thirty full lines at 80 columns. A
/// longer value is cut and ends in `…`; `--json` still carries it whole.
pub const DETAIL_VALUE_CAP: usize = 2048;

fn cap_value(value: &str) -> std::borrow::Cow<'_, str> {
    if value.len() <= DETAIL_VALUE_CAP {
        return value.into();
    }
    let mut end = DETAIL_VALUE_CAP;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end]).into()
}

/// `s` broken into pieces of at most `width` cells, at spaces where possible.
#[must_use]
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in s.split(' ') {
        let w = word.width();
        let sep = usize::from(!line.is_empty());
        if used + sep + w <= width {
            if sep == 1 {
                line.push(' ');
            }
            line.push_str(word);
            used += sep + w;
            continue;
        }
        if !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
        // A word longer than the line is split by cells, in one pass over its characters
        // (review N2: rescanning the rest for every piece cost the square of the word's
        // length). Each piece takes at least one character: a two-cell character on a
        // one-cell line is a piece of its own and overflows by a cell, which the widget
        // clips; without that floor the piece was empty and this never ended (finding 1).
        let mut piece = String::new();
        let mut cells = 0;
        for c in word.chars() {
            let cw = c.width().unwrap_or(0);
            if cells + cw > width && !piece.is_empty() {
                out.push(std::mem::take(&mut piece));
                cells = 0;
            }
            piece.push(c);
            cells += cw;
        }
        line = piece;
        used = cells;
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

fn draw_prompt_row(frame: &mut Frame, area: Rect, view: &ViewModel, ui: &Ui, ctx: &Context<'_>) {
    let width = usize::from(area.width);
    let theme = ctx.theme;
    let line = match &ui.input.prompt {
        Some(Prompt::Search { input, .. }) => {
            let found = view.search.as_ref().map_or(0, |s| s.matches.len());
            let text = format!(
                "/{input}▏  {found} {}",
                if found == 1 { "match" } else { "matches" }
            );
            Line::from(Span::styled(cut(&text, width), theme.style(Role::Text)))
        }
        Some(Prompt::Command { input }) => Line::from(Span::styled(
            cut(&format!(":{input}▏"), width),
            theme.style(Role::Text),
        )),
        None => ui
            .input
            .message
            .as_ref()
            .or(view.notice.as_ref())
            .map_or_else(
                || {
                    Line::from(Span::styled(
                        cut(
                            &ctx.log.last().map_or_else(
                                || format!("fleet {}", ctx.flake),
                                |l| format!("log: {l}"),
                            ),
                            width,
                        ),
                        theme.style(Role::Dim),
                    ))
                },
                |m| Line::from(Span::styled(cut(m, width), theme.style(Role::Error))),
            ),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The status bar's text: mode, refresh state, pending machines, Prometheus, fleet errors.
#[must_use]
pub fn status_text(view: &ViewModel, ui: &Ui) -> (String, Option<String>) {
    let refresh = match view.refresh.phase {
        Phase::NotStarted => "starting",
        Phase::ListingMachines => "listing machines",
        Phase::Scanning => "scanning",
        Phase::Settled if view.refresh.timed_out => "timed out",
        Phase::Settled => "settled",
    };
    let prometheus = match &view.prometheus {
        PrometheusStatus::NotConfigured => None,
        PrometheusStatus::NotAsked => Some("prom: not asked".to_owned()),
        PrometheusStatus::Pending => Some("prom: …".to_owned()),
        PrometheusStatus::Available => Some("prom: ok".to_owned()),
        PrometheusStatus::Unavailable { .. } => Some("prom: down, ssh".to_owned()),
    };
    let mut parts = vec![
        mode_label(view).to_owned(),
        refresh.to_owned(),
        format!("{} pending", view.refresh.pending_machines),
    ];
    // The action first: what is running (or how the last one ended) matters more than the scan.
    if let Some(a) = &view.action {
        parts.insert(1, action_text(a));
    }
    // What the previews are doing: they are not "pending machines" (the status and badges are
    // final), but a settled-looking bar over a still-filling panel would say nothing.
    match view.refresh.diffs_pending {
        0 => {}
        1 => parts.push("1 diff collecting".to_owned()),
        n => parts.push(format!("{n} diffs collecting")),
    }
    if ui.approvals != 0 {
        let noun = if ui.approvals == 1 { "host" } else { "hosts" };
        parts.push(format!("ssh: connecting {} {noun}", ui.approvals));
    }
    if let Some(p) = prometheus {
        parts.push(p);
    }
    if let Some(c) = ui.input.count {
        parts.push(format!("count {c}"));
    }
    let mut errors: Vec<String> = Vec::new();
    match view.report.errors.as_slice() {
        [] => {}
        [one] => errors.push(format!("fleet error: {}", one.reason)),
        many => errors.push(format!("{} fleet errors: {}", many.len(), many[0].reason)),
    }
    if let Some(a) = &view.action
        && let ActionStatus::Ended(end) = &a.status
        && matches!(end.result, ActionResult::Failed | ActionResult::TimedOut)
    {
        // Plainly where it failed, and the fix when it is ssh's: the explanation first.
        errors.push(
            end.explanation
                .clone()
                .unwrap_or_else(|| end.summary.join(" | ")),
        );
    }
    if let Some(w) = &ui.warning {
        errors.push(format!("⚠ {w}"));
    }
    (
        (parts.join(" │ ")),
        (!errors.is_empty()).then(|| errors.join(" │ ")),
    )
}

fn draw_status(frame: &mut Frame, area: Rect, view: &ViewModel, ui: &Ui, ctx: &Context<'_>) {
    let theme = ctx.theme;
    let width = usize::from(area.width);
    let (text, errors) = status_text(view, ui);
    let help = ctx
        .keymap
        .keys_for(Action::Help)
        .first()
        .map(|k| format!(" {k} help"))
        .unwrap_or_default();
    // The mode word on its own: ACT in the warning colour, so it cannot be missed.
    let label = mode_label(view);
    let shown = cut(&text, width);
    // Split by characters, not bytes: a bar cut to a few cells ends in `…`, which is three bytes.
    let split = shown
        .char_indices()
        .nth(label.chars().count())
        .map_or(shown.len(), |(i, _)| i);
    let (head, tail) = shown.split_at(split);
    let head_role = if view.mode == Mode::Act {
        Role::Warning
    } else {
        Role::Title
    };
    let mut spans = vec![
        Span::styled(head.to_owned(), theme.style(head_role)),
        Span::styled(tail.to_owned(), theme.style(Role::Title)),
    ];
    let mut used = text.width().min(width);
    if let Some(e) = errors {
        let room = width.saturating_sub(used + 3);
        if room > 0 {
            spans.push(Span::styled(" │ ", theme.style(Role::Dim)));
            let e = cut(&e, room);
            used += 3 + e.width();
            spans.push(Span::styled(e, theme.style(Role::Error)));
        }
    }
    if used + help.width() <= width {
        spans.push(Span::raw(" ".repeat(width - used - help.width())));
        spans.push(Span::styled(help, theme.style(Role::Dim)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// The help overlay's lines, from the live keymap: every action with its keys (an unbound
/// one says so), the prompt's commands, and the badges.
#[must_use]
pub fn help_lines(keymap: &Keymap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Action::ALL
        .into_iter()
        .map(|a| {
            let keys = keymap.keys_for(a);
            let keys = if keys.is_empty() {
                "(unbound)".to_owned()
            } else {
                keys.join(" ")
            };
            (keys, a.description().to_owned())
        })
        .collect();
    out.push((":q".to_owned(), "quit".to_owned()));
    out.push((":refresh".to_owned(), "scan again".to_owned()));
    out
}

const LEGEND: [Badge; 11] = [
    Badge::Checking,
    Badge::Evaluating,
    Badge::InSync,
    Badge::UpdatePending,
    Badge::Cached,
    Badge::FailedUnits,
    Badge::RebootNeeded,
    Badge::NotInstalled,
    Badge::Unreachable,
    Badge::EvalError,
    Badge::Frozen,
];

fn draw_help(frame: &mut Frame, area: Rect, ctx: &Context<'_>) {
    let theme = ctx.theme;
    let entries = help_lines(ctx.keymap);
    let keys_cells = entries
        .iter()
        .map(|(k, _)| k.width())
        .max()
        .unwrap_or(0)
        .min(20);
    let mut lines: Vec<Line> = entries
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(fit(k, keys_cells), theme.style(Role::Title)),
                Span::raw("  "),
                Span::styled(d.clone(), theme.style(Role::Text)),
            ])
        })
        .collect();
    lines.push(Line::default());
    for b in LEGEND {
        let glyph = b.glyph().unwrap_or(SPINNER[0]);
        lines.push(Line::from(vec![
            Span::styled(glyph, theme.badge(b)),
            Span::raw(" "),
            Span::styled(b.label(), theme.style(Role::Text)),
        ]));
    }
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let rect = centred(area, 56, height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(Span::styled(" help ", theme.style(Role::Title)))
        .border_style(theme.style(Role::Dim))
        .style(theme.base());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The rows of the Log tab from row `start`, each cut to `width` cells and coloured by where it
/// came from: standard output as text, standard error in its own colour, auspex's own words
/// dimmed.
fn log_rows(
    ctx: &Context<'_>,
    notes: bool,
    start: usize,
    height: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let theme = ctx.theme;
    if notes {
        return ctx
            .log
            .iter()
            .skip(start)
            .take(height)
            .map(|l| Line::from(Span::styled(cut(l, width), theme.style(Role::Text))))
            .collect();
    }
    if ctx.output.is_empty() {
        return [
            "no action has run yet.",
            "u updates the selected machine (act mode, A); b builds its pending system here.",
        ]
        .into_iter()
        .map(|l| Line::from(Span::styled(cut(l, width), theme.style(Role::Dim))))
        .collect();
    }
    ctx.output
        .window(start, height)
        .into_iter()
        .map(|l| {
            let role = match l.stream {
                Stream::Stdout => Role::Text,
                Stream::Stderr => Role::Stderr,
                Stream::Note => Role::Dim,
            };
            Line::from(Span::styled(cut(&l.text, width), theme.style(role)))
        })
        .collect()
}

/// The Log tab: the streamed output of the current and last action, or (Tab) the adapters'
/// notes. Scrollable; follows the tail until the operator scrolls up.
///
/// What is drawn is one window of the output, taken under the buffer's lock, never a copy of
/// the whole of it. The first visible row is written back into
/// [`LogTab::scroll`](crate::input::LogTab::scroll), and the drawing is what decides
/// whether the tab is still following: it is when nothing was scrolled up, or when the scroll
/// has come back to the end.
fn draw_log(frame: &mut Frame, area: Rect, ui: &mut Ui, ctx: &Context<'_>) {
    let theme = ctx.theme;
    let rect = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    frame.render_widget(Clear, rect);
    let notes = ui.input.log.notes || ctx.output.is_empty() && !ctx.log.is_empty();
    let inner_height = usize::from(rect.height.saturating_sub(2)).max(1);
    let (total, dropped, older) = if notes {
        (ctx.log.len(), 0, 0)
    } else {
        (
            ctx.output.rows(),
            ctx.output.dropped(),
            ctx.output.last_rows(),
        )
    };
    let last_start = total.saturating_sub(inner_height);
    let input = &mut ui.input;
    if input.log.unfollowed {
        // Lines dropped from the front since the last frame move every row of the current
        // action up: keep showing the same ones (the older action's rows do not move).
        let shift =
            usize::try_from(dropped.saturating_sub(input.log.dropped_seen)).unwrap_or(usize::MAX);
        if input.log.scroll >= older {
            input.log.scroll = input.log.scroll.saturating_sub(shift).max(older);
        }
    }
    input.log.dropped_seen = dropped;
    let start = if input.log.unfollowed {
        input.log.scroll.min(last_start)
    } else {
        last_start
    };
    if start >= last_start {
        input.log.unfollowed = false;
    }
    input.log.scroll = start;
    let following = !input.log.unfollowed;

    let shown = log_rows(
        ctx,
        notes,
        start,
        inner_height,
        usize::from(rect.width.saturating_sub(2)),
    );
    let position = if total > inner_height {
        format!(
            " {}–{}/{}{} ",
            start + 1,
            (start + inner_height).min(total),
            total,
            if following { " following" } else { "" }
        )
    } else {
        String::new()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            if notes { " log: notes " } else { " log " },
            theme.style(Role::Title),
        ))
        .title_bottom(Span::styled(
            format!(
                "{position}Tab {} · q/esc close ",
                if notes { "output" } else { "notes" }
            ),
            theme.style(Role::Dim),
        ))
        .border_style(theme.style(Role::Dim))
        .style(theme.base());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(shown), inner);
}

/// What the box says when the pane cannot show the question.
const TOO_SMALL: &str = "terminal too small to ask: make it larger";

/// The narrowest box the question is asked in. Narrower, it wraps into a column nobody reads.
const MIN_CONFIRM_WIDTH: u16 = 28;

/// What the operator must always see in the question box: the question, the typed name (and a
/// mismatch) and what to press. The line saying what `y` does is the one a clipped box used to
/// lose first.
fn confirm_tail(confirm: &ConfirmView, cells: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut tail: Vec<Line> = wrap(&confirm.question, cells)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, theme.style(Role::Warning))))
        .collect();
    if let Some(typed) = &confirm.typed {
        tail.push(Line::default());
        tail.push(Line::from(vec![
            Span::styled("> ", theme.style(Role::Dim)),
            Span::styled(typed.text.clone(), theme.style(Role::Text)),
            Span::styled("▏", theme.style(Role::Dim)),
        ]));
        if typed.mismatch {
            tail.push(Line::from(Span::styled(
                format!("that is not {}", typed.expected),
                theme.style(Role::Error),
            )));
        }
    }
    tail.push(Line::default());
    let hint = match confirm.kind {
        ConfirmKind::Refused => "any key closes",
        ConfirmKind::Checking => "any key cancels",
        ConfirmKind::UpdateTyped => "Enter confirms the exact name; Esc cancels",
        _ => "y confirms; any other key cancels",
    };
    tail.extend(
        wrap(hint, cells)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, theme.style(Role::Dim)))),
    );
    tail
}

/// What the checks found, refusals first, in at most `room` lines.
///
/// Each sentence starts with its kind in words, so the colour is not the only thing that says
/// which is which (a monochrome terminal, a colour-blind operator), and continuation lines are
/// indented under it. What does not fit is replaced by a line saying how many are not shown;
/// refusals come first, so they are the last to go.
fn confirm_found(
    confirm: &ConfirmView,
    cells: usize,
    room: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let sentence = |label: &str, role: Role, item: &String| -> Vec<Line<'static>> {
        wrap(&format!("{label}: {item}"), cells)
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let l = if i == 0 { l } else { format!("  {l}") };
                Line::from(Span::styled(l, theme.style(role)))
            })
            .collect()
    };
    let found: Vec<Vec<Line>> = confirm
        .refusals
        .iter()
        .map(|r| sentence("refused", Role::Error, r))
        .chain(
            confirm
                .warnings
                .iter()
                .map(|w| sentence("warning", Role::Warning, w)),
        )
        .collect();
    let mut shown = 0;
    let mut used = 0;
    for item in &found {
        if used + item.len() > room {
            break;
        }
        used += item.len();
        shown += 1;
    }
    // A "…and N more" line has to fit too: give up the last shown item for it if need be.
    let mut more = found.len() - shown;
    if more > 0 {
        while shown > 0 && used + 1 > room {
            shown -= 1;
            used -= found[shown].len();
            more += 1;
        }
    }
    let mut lines: Vec<Line> = found[..shown].iter().flatten().cloned().collect();
    if more > 0 && used < room {
        let hidden_refusals = confirm.refusals.len().saturating_sub(shown);
        let (what, role) = if hidden_refusals > 0 {
            (
                format!("…and {more} more, {hidden_refusals} refused: enlarge"),
                Role::Error,
            )
        } else {
            (
                format!(
                    "…and {more} more warning{}",
                    if more == 1 { "" } else { "s" }
                ),
                Role::Warning,
            )
        };
        lines.push(Line::from(Span::styled(what, theme.style(role))));
    }
    lines
}

/// The question waiting for an answer, in a box of its own: a one-line prompt would cut the very
/// words that say what `y` does at 40 columns.
///
/// For an update the box carries everything the checks found, in the order that matters:
/// what **refused** it (the update cannot go ahead), then the **warnings**, then the question,
/// then what to press. A refused update says there is nothing to press but a key to close it.
///
/// Returns whether the whole question, and the line saying what `y` does, fitted. A pane too
/// small to show it is not asking anything: the box then says so, the question is not counted
/// as drawn, and no key can answer it. When the question fits but what the checks found does
/// not, the question, the typed name and the hint stay and the findings are cut: refusals
/// first, with a line saying how many are not shown.
fn draw_confirm(frame: &mut Frame, area: Rect, view: &ViewModel, ctx: &Context<'_>) -> bool {
    let Some(confirm) = &view.confirm else {
        return false;
    };
    let theme = ctx.theme;
    let width = area.width.min(78);
    let text_cells = usize::from(width.saturating_sub(4)).max(1);
    let tail = confirm_tail(confirm, text_cells, theme);
    let has_findings = !confirm.refusals.is_empty() || !confirm.warnings.is_empty();
    // What is left of the pane once the question, the typed name and the hint have theirs. The
    // question is "whole" only if what the checks found is shown too, or at least counted: a
    // dialog with findings and no line left for even "…and N more" would read as a clean y/N
    // question, and `y` would deploy what the warnings were about. So with findings, one spare
    // line is needed at the very least.
    let spare = usize::from(area.height).saturating_sub(2 + tail.len());
    let whole = width >= MIN_CONFIRM_WIDTH
        && tail.len() + 2 <= usize::from(area.height)
        && (!has_findings || spare >= 1);
    let lines: Vec<Line> = if whole {
        // The findings get the spare lines, less one for the blank between the two parts; with
        // exactly one spare line that is the findings' (their count), and the blank goes.
        let room = if spare >= 2 { spare - 1 } else { spare };
        let mut lines = confirm_found(confirm, text_cells, room, theme);
        if !lines.is_empty() && spare >= 2 {
            lines.push(Line::default());
        }
        lines.extend(tail);
        lines
    } else {
        // Not the question: a message, wrapped to what room there is.
        let cells = usize::from(area.width.saturating_sub(2)).max(1);
        wrap(TOO_SMALL, cells)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, theme.style(Role::Warning))))
            .collect()
    };
    let width = if whole { width } else { area.width };
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let rect = centred(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(Span::styled(" confirm ", theme.style(Role::Warning)))
        .border_style(theme.style(Role::Warning))
        .style(theme.base());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);
    whole
}

/// The package diff overlay (`d`): the selected machine's full list, scrollable.
///
/// `ui.input.diff_scroll` is clamped here, to the last page of the list, and written back:
/// the keys only add to it (`G` sets it to the maximum), so the drawing is what knows how
/// long the list is.
fn draw_diff(frame: &mut Frame, area: Rect, view: &ViewModel, ui: &mut Ui, ctx: &Context<'_>) {
    let theme = ctx.theme;
    let machine = view.selected.and_then(|i| view.report.machines.get(i));
    let rect = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    frame.render_widget(Clear, rect);
    let lines = machine.map_or_else(
        || vec![(Kind::Note, "no machine selected".to_owned())],
        |m| packages::full_lines(m, view.binary_cache),
    );
    let inner_height = usize::from(rect.height.saturating_sub(2));
    let last_start = lines.len().saturating_sub(inner_height.max(1));
    ui.input.diff_scroll = ui.input.diff_scroll.min(last_start);
    let start = ui.input.diff_scroll;
    let position = if lines.len() > inner_height {
        format!(
            " {}–{}/{} ",
            start + 1,
            (start + inner_height).min(lines.len()),
            lines.len()
        )
    } else {
        String::new()
    };
    let close = ctx
        .keymap
        .keys_for(Action::Quit)
        .into_iter()
        .find(|k| k == "q")
        .map_or_else(
            || ctx.keymap.keys_for(Action::Diff).join(" "),
            |_| "q".to_owned(),
        );
    let title = format!(
        " package diff: {} ",
        machine.map_or("—", |m| m.name.as_str())
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            cut(&title, usize::from(rect.width.saturating_sub(4))),
            theme.style(Role::Title),
        ))
        .title_bottom(Span::styled(
            format!("{position}{close}/esc close "),
            theme.style(Role::Dim),
        ))
        .border_style(theme.style(Role::Dim))
        .style(theme.base());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let width = usize::from(inner.width);
    let shown: Vec<Line> = lines
        .iter()
        .skip(start)
        .take(inner_height)
        .map(|(kind, text)| match kind {
            Kind::Heading => Line::from(Span::styled(cut(text, width), theme.style(Role::Title))),
            Kind::Item => Line::from(Span::styled(
                format!("  {}", cut(text, width.saturating_sub(2))),
                theme.style(Role::Text),
            )),
            Kind::Note => Line::from(Span::styled(cut(text, width), theme.style(Role::Dim))),
        })
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// `ACT` or `OBSERVE`: the status bar's first word.
const fn mode_label(view: &ViewModel) -> &'static str {
    match view.mode {
        Mode::Observe => "OBSERVE",
        Mode::Act => "ACT",
    }
}

/// What the action is doing, or how it ended: `update luna: running`.
fn action_text(a: &ActionView) -> String {
    let what = match &a.status {
        ActionStatus::Running => "running".to_owned(),
        ActionStatus::Stopping => "stopping".to_owned(),
        ActionStatus::Ended(end) => match end.exit_code {
            Some(code) if end.result != ActionResult::Succeeded => {
                format!("{} (exit {code})", end.result.label())
            }
            _ => end.result.label().to_owned(),
        },
    };
    format!("{} {}: {what}", a.kind.label(), a.machine)
}
