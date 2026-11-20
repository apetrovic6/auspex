//! The keymap is data: one table of defaults, overridden from `[keymap]` in the config.
//!
//! The help overlay is generated from the live [`Keymap`] (see `ui`), so what `?` shows and
//! what a key does cannot disagree.
//!
//! # Key syntax in the config
//!
//! - one printable character: `"j"`, `"G"`, `"/"`, `"?"`;
//! - a named key: `"esc"`, `"enter"`, `"tab"`, `"space"`, `"backspace"`, `"up"`, `"down"`,
//!   `"left"`, `"right"`, `"home"`, `"end"`, `"pageup"`, `"pagedown"`;
//! - a control chord: `"ctrl-d"`;
//! - a sequence of printable characters: `"gg"` (at most two keys).
//!
//! An action's list *replaces* its defaults; an empty list unbinds it. A key given to an action
//! explicitly is taken away from any action that only had it by default. Two explicit actions
//! claiming one key, or one binding being the first key of another (`g` and `gg`), is an
//! error: either would make some binding unreachable.

use std::collections::BTreeMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Everything a key can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// Down one row (or a count).
    Down,
    /// Up one row (or a count).
    Up,
    /// Down half a page.
    HalfPageDown,
    /// Up half a page.
    HalfPageUp,
    /// The first row (with a count, that row).
    Top,
    /// The last row (with a count, that row).
    Bottom,
    /// Open the `/` search prompt.
    Search,
    /// The next match.
    SearchNext,
    /// The previous match.
    SearchPrevious,
    /// Scan again.
    Refresh,
    /// The help overlay.
    Help,
    /// Open the `:` command prompt.
    Command,
    /// Show the detail of the selected machine (narrow layout; the wide one always shows it).
    Detail,
    /// The Log tab: the streamed output of the current and last action (Tab: the adapters' notes
    /// and the config's warnings instead).
    Log,
    /// The selected machine's package diff, in full (changed, added, removed).
    Diff,
    /// Toggle act mode (asks first; off again at once).
    Act,
    /// Update the selected machine (act mode only).
    Update,
    /// Build the selected machine's pending system here (changes nothing on any machine).
    Build,
    /// ssh to the selected machine, with the terminal (act mode only).
    Shell,
    /// Freeze the selected machine, or (asking first) unfreeze it. Works in observe mode: it
    /// only prevents changes.
    Freeze,
    /// Quit.
    Quit,
}

impl Action {
    /// Every action, in the order the help overlay lists them.
    pub const ALL: [Self; 21] = [
        Self::Down,
        Self::Up,
        Self::HalfPageDown,
        Self::HalfPageUp,
        Self::Top,
        Self::Bottom,
        Self::Search,
        Self::SearchNext,
        Self::SearchPrevious,
        Self::Detail,
        Self::Log,
        Self::Diff,
        Self::Act,
        Self::Update,
        Self::Build,
        Self::Shell,
        Self::Freeze,
        Self::Refresh,
        Self::Command,
        Self::Help,
        Self::Quit,
    ];

    /// The name in `[keymap]`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::HalfPageDown => "half_page_down",
            Self::HalfPageUp => "half_page_up",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Search => "search",
            Self::SearchNext => "search_next",
            Self::SearchPrevious => "search_previous",
            Self::Refresh => "refresh",
            Self::Help => "help",
            Self::Command => "command",
            Self::Detail => "detail",
            Self::Log => "log",
            Self::Diff => "diff",
            Self::Act => "act",
            Self::Update => "update",
            Self::Build => "build",
            Self::Shell => "shell",
            Self::Freeze => "freeze",
            Self::Quit => "quit",
        }
    }

    /// What the help overlay says it does.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Down => "down (count: 5j)",
            Self::Up => "up (count: 5k)",
            Self::HalfPageDown => "half a page down",
            Self::HalfPageUp => "half a page up",
            Self::Top => "first row (5gg: row 5)",
            Self::Bottom => "last row (5G: row 5)",
            Self::Search => "search names",
            Self::SearchNext => "next match",
            Self::SearchPrevious => "previous match",
            Self::Refresh => "scan again",
            Self::Help => "this help",
            Self::Command => "command (:q, :refresh)",
            Self::Detail => "machine detail (narrow)",
            Self::Log => "log tab: action output (Tab: notes)",
            Self::Diff => "package diff (q closes)",
            Self::Act => "toggle act mode (asks first)",
            Self::Update => "update the machine (act mode)",
            Self::Build => "build its pending system here",
            Self::Shell => "ssh to the machine (act mode)",
            Self::Freeze => "freeze the machine (asks to unfreeze)",
            Self::Quit => "quit",
        }
    }

    /// The default keys, in the config's syntax.
    #[must_use]
    pub const fn defaults(self) -> &'static [&'static str] {
        match self {
            Self::Down => &["j", "down"],
            Self::Up => &["k", "up"],
            Self::HalfPageDown => &["ctrl-d"],
            Self::HalfPageUp => &["ctrl-u"],
            Self::Top => &["gg", "home"],
            Self::Bottom => &["G", "end"],
            Self::Search => &["/"],
            Self::SearchNext => &["n"],
            Self::SearchPrevious => &["N"],
            Self::Refresh => &["r"],
            Self::Help => &["?"],
            Self::Command => &[":"],
            Self::Detail => &["enter", "tab"],
            Self::Log => &["L"],
            Self::Diff => &["d"],
            Self::Act => &["A"],
            Self::Update => &["u"],
            Self::Build => &["b"],
            Self::Shell => &["s"],
            Self::Freeze => &["f"],
            Self::Quit => &["q", "esc", "ctrl-c"],
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }
}

/// One key press, normalised: a character's case is in the character, never in a Shift
/// modifier, so `G` from the config matches Shift+g from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The key.
    pub code: KeyCode,
    /// Control held.
    pub ctrl: bool,
}

impl Key {
    /// A plain character.
    #[must_use]
    pub const fn char(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            ctrl: false,
        }
    }

    /// A control chord of a lower-case letter (`ctrl-c`).
    #[must_use]
    pub const fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            ctrl: true,
        }
    }

    /// A key with no modifier.
    #[must_use]
    pub const fn plain(code: KeyCode) -> Self {
        Self { code, ctrl: false }
    }

    /// From a terminal event. Alt chords are not bindable in step 1, so they are `None`.
    #[must_use]
    pub const fn from_event(event: &KeyEvent) -> Option<Self> {
        if event.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
        let code = match event.code {
            KeyCode::Char(c) if ctrl => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };
        Some(Self { code, ctrl })
    }

    /// A printable character typed without Control: what a prompt takes as text.
    #[must_use]
    pub const fn text(self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if !self.ctrl => Some(c),
            _ => None,
        }
    }
}

const NAMED: &[(&str, KeyCode)] = &[
    ("esc", KeyCode::Esc),
    ("enter", KeyCode::Enter),
    ("tab", KeyCode::Tab),
    ("space", KeyCode::Char(' ')),
    ("backspace", KeyCode::Backspace),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
];

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("ctrl-")?;
        }
        if let Some((name, _)) = NAMED.iter().find(|(_, c)| *c == self.code) {
            return f.write_str(name);
        }
        match self.code {
            KeyCode::Char(c) => write!(f, "{c}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// A binding: one key, or a sequence of two.
pub type Chord = Vec<Key>;

/// Shows a chord the way the config spells it.
#[must_use]
pub fn show(chord: &[Key]) -> String {
    chord.iter().map(ToString::to_string).collect()
}

/// Parses one key in the config's syntax.
///
/// # Errors
///
/// The spec is empty, names no key, or is a sequence longer than two or containing a named key.
pub fn parse_chord(spec: &str) -> Result<Chord, String> {
    if spec.is_empty() {
        return Err("an empty key".to_owned());
    }
    let lower = spec.to_ascii_lowercase();
    if let Some((_, code)) = NAMED.iter().find(|(n, _)| *n == lower) {
        return Ok(vec![Key::plain(*code)]);
    }
    if let Some(rest) = lower.strip_prefix("ctrl-") {
        let mut chars = rest.chars();
        return match (chars.next(), chars.next()) {
            // A terminal sends these three as other keys (the same byte), so a binding
            // to them would never fire (review N4).
            (Some(c @ ('m' | 'i' | '[')), None) => Err(format!(
                "`{spec}` never arrives as such: the terminal sends it as `{}`; write that",
                match c {
                    'm' => "enter",
                    'i' => "tab",
                    _ => "esc",
                }
            )),
            (Some(c), None) if c.is_ascii_graphic() => Ok(vec![Key {
                code: KeyCode::Char(c),
                ctrl: true,
            }]),
            _ => Err(format!("`{spec}`: ctrl- takes one character")),
        };
    }
    let chars: Vec<char> = spec.chars().collect();
    if chars.iter().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!(
            "`{spec}` is not a key (use \"space\" for the space bar)"
        ));
    }
    if chars.len() > 2 {
        return Err(format!(
            "`{spec}` is not a key: not a named key, and a sequence is at most two characters"
        ));
    }
    Ok(chars.into_iter().map(Key::char).collect())
}

/// What a sequence of keys means so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// A complete binding.
    Action(Action),
    /// The start of a longer binding: wait for the next key.
    Prefix,
    /// Nothing.
    None,
}

/// The live keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    bindings: Vec<(Chord, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Keymap {
    /// The defaults alone.
    ///
    /// # Panics
    ///
    /// Never: every default key parses (`every_default_shows_as_it_is_spelled`).
    #[must_use]
    pub fn defaults() -> Self {
        let mut bindings = Vec::new();
        for a in Action::ALL {
            for spec in a.defaults() {
                bindings.push((parse_chord(spec).expect("the default keys parse"), a));
            }
        }
        Self { bindings }
    }

    /// The defaults with `[keymap]` applied. Unknown action names are warnings.
    ///
    /// # Errors
    ///
    /// A key that does not parse, a key bound explicitly to two actions, or a binding that is
    /// a prefix of another; each names the `keymap.<action>` key.
    pub fn with_overrides(
        overrides: &BTreeMap<String, Vec<String>>,
        warnings: &mut Vec<String>,
    ) -> Result<Self, String> {
        let mut explicit: Vec<(Chord, Action)> = Vec::new();
        let mut replaced = Vec::new();
        for (name, specs) in overrides {
            let Some(action) = Action::from_name(name) else {
                warnings.push(format!(
                    "unknown key `keymap.{name}` ignored (not an action in this auspex)"
                ));
                continue;
            };
            replaced.push(action);
            for spec in specs {
                let chord = parse_chord(spec).map_err(|e| format!("`keymap.{name}`: {e}"))?;
                if let Some((_, other)) = explicit.iter().find(|(c, _)| *c == chord) {
                    if *other != action {
                        return Err(format!(
                            "`keymap.{name}`: `{spec}` is also bound to `{}`",
                            other.name()
                        ));
                    }
                    continue;
                }
                explicit.push((chord, action));
            }
        }
        let mut bindings = explicit.clone();
        for (chord, action) in Self::defaults().bindings {
            if replaced.contains(&action) || explicit.iter().any(|(c, _)| *c == chord) {
                continue;
            }
            bindings.push((chord, action));
        }
        for (a, (ca, aa)) in bindings.iter().enumerate() {
            for (cb, ab) in &bindings[a + 1..] {
                let (short, long) = if ca.len() <= cb.len() {
                    (ca, cb)
                } else {
                    (cb, ca)
                };
                if short.len() < long.len() && long.starts_with(short) {
                    return Err(format!(
                        "`keymap`: `{}` ({}) is the start of `{}` ({}), so one of them could \
                         never be typed",
                        show(short),
                        if short == ca { aa.name() } else { ab.name() },
                        show(long),
                        if long == ca { aa.name() } else { ab.name() },
                    ));
                }
            }
        }
        // Raw mode makes Ctrl-C a key, so with neither a quit key nor the `:` prompt (for
        // `:q`) only a signal from another terminal could end the TUI (review finding 6).
        if !bindings
            .iter()
            .any(|(_, a)| matches!(a, Action::Quit | Action::Command))
        {
            return Err(
                "`keymap.quit`: no key quits and `keymap.command` is unbound too (so no `:q`); \
                 keep at least one of them"
                    .to_owned(),
            );
        }
        Ok(Self { bindings })
    }

    /// The keys bound to `action`, as the config spells them, explicit ones first.
    #[must_use]
    pub fn keys_for(&self, action: Action) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|(_, a)| *a == action)
            .map(|(c, _)| show(c))
            .collect()
    }

    /// What `keys` mean.
    #[must_use]
    pub fn lookup(&self, keys: &[Key]) -> Lookup {
        if let Some((_, a)) = self.bindings.iter().find(|(c, _)| c == keys) {
            return Lookup::Action(*a);
        }
        if self
            .bindings
            .iter()
            .any(|(c, _)| c.len() > keys.len() && c.starts_with(keys))
        {
            return Lookup::Prefix;
        }
        Lookup::None
    }

    /// Whether `key` alone is bound (a digit that is bound is not a count).
    #[must_use]
    pub fn binds(&self, key: Key) -> bool {
        self.lookup(&[key]) != Lookup::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse() {
        assert_eq!(parse_chord("j").unwrap(), vec![Key::char('j')]);
        assert_eq!(parse_chord("G").unwrap(), vec![Key::char('G')]);
        assert_eq!(
            parse_chord("gg").unwrap(),
            vec![Key::char('g'), Key::char('g')]
        );
        assert_eq!(parse_chord("Esc").unwrap(), vec![Key::plain(KeyCode::Esc)]);
        assert_eq!(
            parse_chord("ctrl-D").unwrap(),
            vec![Key {
                code: KeyCode::Char('d'),
                ctrl: true
            }]
        );
        assert!(parse_chord("").is_err());
        assert!(parse_chord("ggg").is_err());
        assert!(parse_chord("a b").is_err());
        assert!(parse_chord("ctrl-").is_err());
    }

    #[test]
    fn every_default_shows_as_it_is_spelled() {
        for a in Action::ALL {
            let shown = Keymap::defaults().keys_for(a);
            let spelled: Vec<String> = a.defaults().iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(shown, spelled, "{a:?}");
        }
    }
}
