//! Keys to core events: counts, two-key sequences, the `/` and `:` prompts, and what Esc
//! closes first.
//!
//! Pure: [`Input::key`] takes a key and what is on screen, and returns the core events to send
//! and what changed in the shell's own state. No terminal, so `tests/keys.rs` drives it
//! directly.
//!
//! Esc's precedence, first that applies: a prompt is cancelled; the help overlay closes; the
//! log closes; the narrow layout's detail view closes; a half-typed count or sequence is
//! dropped. Only then is Esc an ordinary key (by default, quit).

use auspex_core::Event;
use auspex_domain::text::sanitize_line;
use crossterm::event::KeyCode;

use crate::keymap::{Action, Key, Keymap, Lookup};

/// The largest count accepted; more digits are ignored, as vim caps a count.
pub const MAX_COUNT: usize = 99_999;

/// A prompt being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// `/`: each keystroke searches (incremental, as vim's `incsearch`).
    Search {
        /// What has been typed.
        input: String,
        /// The query before the prompt opened, restored on Esc.
        previous: String,
    },
    /// `:`.
    Command {
        /// What has been typed.
        input: String,
    },
}

/// The shell's own interaction state (the core keeps selection, search and help).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    /// A count being typed (`5` of `5j`).
    pub count: Option<usize>,
    /// The first key of a two-key sequence (`g` of `gg`).
    pub pending: Vec<Key>,
    /// The open prompt.
    pub prompt: Option<Prompt>,
    /// The narrow layout's detail view.
    pub detail_open: bool,
    /// The log overlay.
    pub log_open: bool,
    /// A one-line message for the prompt row (an unknown command), until the next key.
    pub message: Option<String>,
}

/// What is on screen, as far as a key's meaning depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    /// The core's help overlay is open.
    pub help_open: bool,
    /// The list's visible height in rows, for half pages.
    pub page: usize,
}

impl Input {
    /// Handles one key. Returns the core events to send, in order.
    pub fn key(
        &mut self,
        key: Key,
        screen: Screen,
        keymap: &Keymap,
        current_query: &str,
    ) -> Vec<Event> {
        self.message = None;
        if let Some(prompt) = self.prompt.take() {
            return self.prompt_key(prompt, key);
        }
        let esc = key == Key::plain(KeyCode::Esc);
        if esc {
            if screen.help_open {
                return vec![Event::CloseHelp];
            }
            if self.log_open {
                self.log_open = false;
                return Vec::new();
            }
            if self.detail_open {
                self.detail_open = false;
                return Vec::new();
            }
            if self.count.is_some() || !self.pending.is_empty() {
                self.count = None;
                self.pending.clear();
                return Vec::new();
            }
        }
        // A digit is a count unless the keymap binds it (and `0` only continues one).
        if self.pending.is_empty()
            && !keymap.binds(key)
            && let Some(d) = key.text().and_then(|c| c.to_digit(10))
            && (d != 0 || self.count.is_some())
        {
            let next = self.count.unwrap_or(0).saturating_mul(10) + d as usize;
            self.count = Some(next.min(MAX_COUNT));
            return Vec::new();
        }
        let mut keys = std::mem::take(&mut self.pending);
        keys.push(key);
        let action = match keymap.lookup(&keys) {
            Lookup::Prefix => {
                self.pending = keys;
                return Vec::new();
            }
            Lookup::None => {
                self.count = None;
                return Vec::new();
            }
            Lookup::Action(a) => a,
        };
        let count = self.count.take();
        if screen.help_open && !matches!(action, Action::Help | Action::Quit) {
            // The overlay covers the list; keys that move it would act unseen.
            return Vec::new();
        }
        self.action(action, count, screen, current_query)
    }

    fn action(
        &mut self,
        action: Action,
        count: Option<usize>,
        screen: Screen,
        current_query: &str,
    ) -> Vec<Event> {
        let n = count.unwrap_or(1);
        let half = screen.page.max(1);
        match action {
            Action::Down => vec![Event::MoveDown(n)],
            Action::Up => vec![Event::MoveUp(n)],
            Action::HalfPageDown => vec![Event::HalfPageDown(half); n],
            Action::HalfPageUp => vec![Event::HalfPageUp(half); n],
            Action::Top | Action::Bottom if count.is_some() => {
                vec![Event::MoveTop, Event::MoveDown(n.saturating_sub(1))]
            }
            Action::Top => vec![Event::MoveTop],
            Action::Bottom => vec![Event::MoveBottom],
            Action::SearchNext => vec![Event::SearchNext; n],
            Action::SearchPrevious => vec![Event::SearchPrevious; n],
            Action::Search => {
                self.prompt = Some(Prompt::Search {
                    input: String::new(),
                    previous: current_query.to_owned(),
                });
                Vec::new()
            }
            Action::Command => {
                self.prompt = Some(Prompt::Command {
                    input: String::new(),
                });
                Vec::new()
            }
            Action::Refresh => vec![Event::Refresh],
            Action::Help => vec![Event::ToggleHelp],
            Action::Detail => {
                self.detail_open = !self.detail_open;
                Vec::new()
            }
            Action::Log => {
                self.log_open = !self.log_open;
                Vec::new()
            }
            Action::Quit => vec![Event::Quit],
        }
    }

    fn prompt_key(&mut self, prompt: Prompt, key: Key) -> Vec<Event> {
        let cancel = key == Key::plain(KeyCode::Esc)
            || key
                == (Key {
                    code: KeyCode::Char('c'),
                    ctrl: true,
                });
        match prompt {
            Prompt::Search {
                mut input,
                previous,
            } => {
                if cancel {
                    return vec![Event::Search(previous)];
                }
                match key.code {
                    KeyCode::Enter => Vec::new(),
                    KeyCode::Backspace if input.is_empty() => vec![Event::Search(previous)],
                    KeyCode::Backspace => {
                        input.pop();
                        let query = input.clone();
                        self.prompt = Some(Prompt::Search { input, previous });
                        vec![Event::Search(query)]
                    }
                    _ => {
                        if let Some(c) = key.text() {
                            input.push(c);
                        }
                        let query = input.clone();
                        self.prompt = Some(Prompt::Search { input, previous });
                        vec![Event::Search(query)]
                    }
                }
            }
            Prompt::Command { mut input } => {
                if cancel {
                    return Vec::new();
                }
                match key.code {
                    KeyCode::Enter => self.command(input.trim()),
                    KeyCode::Backspace if input.is_empty() => Vec::new(),
                    KeyCode::Backspace => {
                        input.pop();
                        self.prompt = Some(Prompt::Command { input });
                        Vec::new()
                    }
                    _ => {
                        if let Some(c) = key.text() {
                            input.push(c);
                        }
                        self.prompt = Some(Prompt::Command { input });
                        Vec::new()
                    }
                }
            }
        }
    }

    /// `:q` and `:refresh`; the plan's other commands are later steps.
    fn command(&mut self, command: &str) -> Vec<Event> {
        match command {
            "" => Vec::new(),
            "q" | "quit" => vec![Event::Quit],
            "refresh" => vec![Event::Refresh],
            other => {
                self.message = Some(format!(
                    "not a command in observe mode: :{} (try :q or :refresh)",
                    sanitize_line(other)
                ));
                Vec::new()
            }
        }
    }
}
