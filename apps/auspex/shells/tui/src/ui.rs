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

use auspex_core::{Phase, PrometheusStatus, ViewModel};
use auspex_domain::fleet::{Lint, TargetSource};
use auspex_domain::observe::{HealthSource, StorePath};
use auspex_domain::report::MachineReport;
use auspex_domain::status::Badge;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::input::{Input, Prompt};
use crate::keymap::{Action, Keymap};
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
        draw_detail(frame, detail, view, theme);
    } else if ui.input.detail_open {
        draw_detail(frame, main, view, theme);
    } else {
        draw_list(frame, main, view, ui, theme);
    }

    draw_prompt_row(frame, prompt_row, view, ui, ctx);
    draw_status(frame, status_row, view, ui, ctx);

    if ui.input.log_open {
        draw_log(frame, main, ctx);
    }
    if view.help_open {
        draw_help(frame, main, ctx);
    }
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
pub fn detail_lines(m: &MachineReport) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("status", m.status.label().to_owned()),
        ("reason", m.reason.clone().unwrap_or_else(|| "—".to_owned())),
        (
            "host",
            format!(
                "{} ({})",
                m.target.as_deref().unwrap_or("—"),
                source(m.target_source)
            ),
        ),
    ];
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
            if m.tags.is_empty() {
                String::new()
            } else {
                format!("  tags: {}", m.tags.join(", "))
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
    out
}

const LABEL_CELLS: usize = 9;

fn draw_detail(frame: &mut Frame, area: Rect, view: &ViewModel, theme: &Theme) {
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
    for (label, value) in detail_lines(m) {
        // Values wrap onto continuation lines under themselves, never under the labels.
        let mut first = true;
        for chunk in wrap(&value, value_cells) {
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
        // A word longer than the line is split by cells. Each piece takes at least one
        // character: a two-cell character on a one-cell line is a piece of its own (and
        // overflows by a cell, which the widget clips). Without that floor the split was
        // empty and this loop never ended (review finding 1).
        let mut rest = word;
        while rest.width() > width {
            let mut cells = 0;
            let split = rest
                .char_indices()
                .find(|(_, c)| {
                    cells += c.width().unwrap_or(0);
                    cells > width
                })
                .map_or(rest.len(), |(i, _)| i)
                .max(rest.chars().next().map_or(rest.len(), char::len_utf8));
            out.push(rest[..split].to_owned());
            rest = &rest[split..];
        }
        line.push_str(rest);
        used = rest.width();
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
        None => ui.input.message.as_ref().map_or_else(
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
        "OBSERVE".to_owned(),
        refresh.to_owned(),
        format!("{} pending", view.refresh.pending_machines),
    ];
    if let Some(p) = prometheus {
        parts.push(p);
    }
    if let Some(c) = ui.input.count {
        parts.push(format!("count {c}"));
    }
    let errors = match view.report.errors.as_slice() {
        [] => None,
        [one] => Some(format!("fleet error: {}", one.reason)),
        many => Some(format!("{} fleet errors: {}", many.len(), many[0].reason)),
    };
    (parts.join(" │ "), errors)
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
    let mut spans = vec![Span::styled(cut(&text, width), theme.style(Role::Title))];
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

/// The log overlay, newest line last, each cut to fit.
fn draw_log(frame: &mut Frame, area: Rect, ctx: &Context<'_>) {
    let (lines, theme) = (ctx.log, ctx.theme);
    let rect = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" log ", theme.style(Role::Title)))
        .border_style(theme.style(Role::Dim))
        .style(theme.base());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let shown: Vec<Line> = lines
        .iter()
        .skip(lines.len().saturating_sub(height))
        .map(|l| Line::from(Span::styled(cut(l, width), theme.style(Role::Text))))
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}
