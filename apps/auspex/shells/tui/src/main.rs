//! The terminal shell.
//!
//! Three jobs: turn keystrokes into events, hand the resulting effects somewhere useful,
//! and draw the view model. It never decides what the count is.
//!
//! The loop is ratatui's own, from `examples/apps/async-github`: `#[tokio::main]`, and a
//! `tokio::select!` over a frame timer and a `crossterm::EventStream`. Nothing in this
//! shell is async yet — the core asks for nothing but a redraw — and that is exactly why
//! the shape is here rather than added later. The first effect a shell has to fulfil is an
//! API call, and under a blocking `event::read()` loop it cannot be awaited anywhere; under
//! this one it becomes a background task whose answer the next frame picks up. See
//! `apps/counter` in the template repository for that same loop with the work filled in.

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
    should_quit: bool,
    /// The locale this shell draws in.
    ///
    /// It lives here, in the shell, exactly as in the desktop shell. Detected once at
    /// start-up from the system's preference list (ADR 0015) and fixed for the run; whatever
    /// else writes it must also call `i18n::set_locale`, because rust-i18n's own macros read
    /// that global and this field does not exist to them.
    locale: Locale,
}

impl App {
    /// How often the screen is repainted, independent of when anything changes.
    ///
    /// A frame timer rather than a redraw triggered by `Effect::Render`: once something
    /// answers from a background task it must not touch the terminal, and this is what
    /// means it does not have to. The cost is bounded — ratatui diffs the buffer, so a
    /// frame in which nothing moved writes nothing.
    const FRAMES_PER_SECOND: f32 = 60.0;

    fn new() -> Self {
        Self {
            core: Core::new(),
            should_quit: false,
            locale: i18n::adopt_system_locale(),
        }
    }

    async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        let period = Duration::from_secs_f32(1.0 / Self::FRAMES_PER_SECOND);
        let mut interval = tokio::time::interval(period);
        let mut events = EventStream::new();

        while !self.should_quit {
            tokio::select! {
                _ = interval.tick() => { terminal.draw(|frame| self.render(frame))?; },
                event = events.next() => match event {
                    Some(Ok(event)) => self.handle_event(&event),
                    // Matched here rather than as a `Some(Ok(..))` pattern on the arm
                    // itself, which is what `select!` answers by disabling the branch: the
                    // frame timer would go on painting a screen no key can reach — alive to
                    // look at, dead to use, and silent about why. An error ends the loop
                    // carrying its cause; an ended stream is the terminal going away, which
                    // is a quit rather than a failure.
                    Some(Err(error)) => return Err(error).context("reading a terminal event"),
                    None => self.should_quit = true,
                },
            }
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_event(&mut self, event: &Event) {
        let Some(key) = event.as_key_press_event() else {
            return;
        };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('+' | '=') | KeyCode::Up => self.dispatch(AppEvent::Increment),
            KeyCode::Char('-') | KeyCode::Down => self.dispatch(AppEvent::Decrement),
            KeyCode::Char('0') => self.dispatch(AppEvent::Reset),
            _ => {}
        }
    }

    fn dispatch(&self, event: AppEvent) {
        for effect in self.core.process_event(event) {
            match effect {
                // The frame timer draws; there is nothing to schedule.
                Effect::Render(_) => {}
            }
        }
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let view = self.core.view();

        let instructions = Line::from(vec![
            " Count ".into(),
            "<+/->".blue().bold(),
            " Reset ".into(),
            "<0>".blue().bold(),
            " Quit ".into(),
            "<Q> ".blue().bold(),
        ]);
        let block = Block::bordered()
            .title(Line::from(" Auspex ".bold()).centered())
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        let inner = block.inner(area);
        block.render(area, buf);

        // Two rows rather than one: the sentence and the legend under it. A one-row middle
        // would clip the second line, and a legend clipped to nothing is the failure this
        // skeleton is meant to make impossible.
        let [_, middle, _] = inner.layout(&Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Fill(1),
        ]));
        let [count_line, legend_line] = middle.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
        ]));

        // The core hands over a number and this shell supplies the sentence (ADR 0016), so
        // the one line of copy in this app is already in `locales/` rather than hardcoded
        // here in whichever language the skeleton happened to be written. The number is
        // formatted before it is interpolated (ADR 0017): `Display` on an `i64` writes no
        // locale's separators, and the catalog does not know the placeholder is a number.
        Paragraph::new(Line::from(
            translate_with!(
                Key::CountIs,
                self.locale,
                count = i18n::number(view.count, self.locale)
            )
            .yellow()
            .bold(),
        ))
        .centered()
        .render(count_line, buf);

        // The plural, under the count, so a scaffolded app starts with a family that is
        // rendered rather than one that only exists in `locales/` (ADR 0018). `apples.*` is
        // in `common/`, which means every app is shipped the family; the reason it has to be
        // named *here* is that a skeleton which does not name it teaches every app copied
        // from it the same omission, and xtask's `plural_families_reach_a_shell` fails on
        // the copy, not on the copy's author's own work.
        Paragraph::new(Line::from(
            translate_with!(
                plural_key(Key::Apples, view.count, self.locale),
                self.locale,
                count = i18n::number(view.count, self.locale)
            )
            .dark_gray(),
        ))
        .centered()
        .render(legend_line, buf);
    }
}

/// `anyhow::Result` rather than a bare `io::Result`: `main` prints its error with `Debug`,
/// which for an `io::Error` is its fields and for an `anyhow::Error` is the message and
/// everything that caused it.
#[tokio::main]
async fn main() -> Result<()> {
    // `ratatui::init`/`restore` rather than `ratatui::run`: `run` hands the terminal to a
    // synchronous closure, and the loop below is a future. The panic hook `init` installs
    // still restores the terminal, and an error returned from here is printed after
    // `restore` has put the normal screen back.
    let terminal = ratatui::init();
    let result = App::new().run(terminal).await;
    ratatui::restore();
    result
}
