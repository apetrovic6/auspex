//! The terminal shell — a placeholder until the real one lands.
//!
//! The core became auspex's fleet scanner; this shell has not caught up yet. It drives the
//! core's view state (`j`/`k`, `?`, `q`) and draws how many machines the core knows about,
//! which is zero, because it never sends `Event::Start`: fulfilling the inventory, probe and
//! evaluation effects means `auspex-effects`, which is the next pull request's work. The
//! effects are matched exhaustively anyway, so a variant added to the core is a compile
//! error here rather than a request silently dropped.
//!
//! The loop is ratatui's own, from `examples/apps/async-github`: `#[tokio::main]`, and a
//! `tokio::select!` over a frame timer and a `crossterm::EventStream`, because the real shell
//! will fulfil effects from background tasks and must not block on the terminal meanwhile.

use std::time::Duration;

use anyhow::{Context as _, Result};
use auspex_core::{Auspex, Core, Effect, Event as AppEvent};
use crossterm::event::{Event, EventStream, KeyCode};
use i18n::{Key, Locale, plural_key, translate_with};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::symbols::border;
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame};
use tokio_stream::StreamExt as _;

struct App {
    core: Core<Auspex>,
    /// The locale this shell draws in, detected once at start-up (ADR 0015).
    locale: Locale,
}

impl App {
    /// How often the screen is repainted, independent of when anything changes: answers
    /// arriving from background tasks must not touch the terminal themselves.
    const FRAMES_PER_SECOND: f32 = 60.0;

    fn new() -> Self {
        Self {
            core: Core::new(),
            locale: i18n::adopt_system_locale(),
        }
    }

    async fn run(self, mut terminal: DefaultTerminal) -> Result<()> {
        let period = Duration::from_secs_f32(1.0 / Self::FRAMES_PER_SECOND);
        let mut interval = tokio::time::interval(period);
        let mut events = EventStream::new();

        while !self.core.view().quit {
            tokio::select! {
                _ = interval.tick() => { terminal.draw(|frame| self.render(frame))?; },
                event = events.next() => match event {
                    Some(Ok(event)) => self.handle_event(&event),
                    // Matched here rather than as a `Some(Ok(..))` pattern on the arm, which
                    // `select!` answers by disabling the branch: the frame timer would go on
                    // painting a screen no key can reach.
                    Some(Err(error)) => return Err(error).context("reading a terminal event"),
                    None => self.dispatch(AppEvent::Quit),
                },
            }
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_event(&self, event: &Event) {
        let Some(key) = event.as_key_press_event() else {
            return;
        };
        // Key mapping (counts, `gg`, the configurable keymap) is the real shell's job; this
        // is just enough to show the core's view state responds.
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.dispatch(AppEvent::Quit),
            KeyCode::Char('j') | KeyCode::Down => self.dispatch(AppEvent::MoveDown(1)),
            KeyCode::Char('k') | KeyCode::Up => self.dispatch(AppEvent::MoveUp(1)),
            KeyCode::Char('?') => self.dispatch(AppEvent::ToggleHelp),
            _ => {}
        }
    }

    fn dispatch(&self, event: AppEvent) {
        for effect in self.core.process_event(event) {
            match effect {
                // Render: the frame timer draws, so there is nothing to schedule. The rest are
                // never emitted here: they all follow `Event::Start`, which this placeholder
                // does not send. Listed rather than wildcarded so the real shell inherits a
                // match that names every port.
                Effect::Render(_)
                | Effect::Inventory(_)
                | Effect::Probe(_)
                | Effect::Evaluate(_)
                | Effect::Prometheus(_)
                | Effect::CacheLookup(_)
                | Effect::ResolveAddresses(_)
                | Effect::Cancel(_) => {}
            }
        }
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let view = self.core.view();
        let machines = i64::try_from(view.report.machines.len()).unwrap_or(i64::MAX);

        let instructions = Line::from(vec![
            " Move ".into(),
            "<j/k>".blue().bold(),
            " Help ".into(),
            "<?>".blue().bold(),
            " Quit ".into(),
            "<Q> ".blue().bold(),
        ]);
        let block = Block::bordered()
            .title(Line::from(" Auspex ".bold()).centered())
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        let inner = block.inner(area);
        block.render(area, buf);

        let [_, middle, _] = inner.layout(&Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Fill(1),
        ]));
        let [count_line, legend_line] = middle.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
        ]));

        // The template's two catalog sentences, fed the machine count. Placeholder copy: the
        // real shell brings auspex's own keys. They stay because xtask's
        // `plural_families_reach_a_shell` requires every shipped plural family to be named by
        // some shell (ADR 0018), and this is the only shell.
        Paragraph::new(Line::from(
            translate_with!(
                Key::CountIs,
                self.locale,
                count = i18n::number(machines, self.locale)
            )
            .yellow()
            .bold(),
        ))
        .centered()
        .render(count_line, buf);

        Paragraph::new(Line::from(
            translate_with!(
                plural_key(Key::Apples, machines, self.locale),
                self.locale,
                count = i18n::number(machines, self.locale)
            )
            .dark_gray(),
        ))
        .centered()
        .render(legend_line, buf);
    }
}

/// `anyhow::Result` rather than a bare `io::Result`: `main` prints its error with `Debug`,
/// which for an `anyhow::Error` is the message and everything that caused it.
#[tokio::main]
async fn main() -> Result<()> {
    let terminal = ratatui::init();
    let result = App::new().run(terminal).await;
    ratatui::restore();
    result
}
