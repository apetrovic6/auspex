//! Colours: the terminal's 16 ANSI colours by default, or base16 slots from `theme.file`.
//!
//! ANSI is the default because stylix and omatheme recolour the terminal itself: naming
//! ANSI colours (never RGB) and never setting a background lets the active terminal theme
//! carry through, runtime switches included. A base16 file names RGB values for each slot
//! and is used as given.
//!
//! Each badge's colour, decided here once (the help overlay shows the glyphs in them):
//!
//! | Badge | Glyph | ANSI | base16 |
//! |---|---|---|---|
//! | checking | spinner | dark grey | `base03` (comments) |
//! | evaluating | spinner | cyan | `base0C` |
//! | in sync | `✓` | green | `base0B` |
//! | update pending | `↑` | yellow | `base0A` |
//! | cached | `◆` | blue | `base0D` |
//! | failed units / alerts | `⚠` | light red | `base09` (orange) |
//! | reboot needed | `⟳` | magenta | `base0E` |
//! | not installed | `?` | grey | `base04` |
//! | unreachable | `✗` | red | `base08` |
//! | eval error | `!` | light magenta | `base0F` |
//! | frozen | `❄` | light blue | `base0D` |
//!
//! Chrome: text `base05` (ANSI: the terminal's default), borders and dim text `base03`
//! (dark grey), titles `base0D` (blue), the selected row `base02` behind `base07` (ANSI:
//! reversed), errors `base08` (red), background `base00` (ANSI: none).
//!
//! Act mode and the confirmations use a warning colour (`base0A`, ANSI yellow, bold): `ACT` in the
//! status bar must not be missed. What an action's process printed on standard error is drawn
//! in `base09` (ANSI light red), so the two streams of the Log tab can be told apart.

use std::path::Path;

use auspex_domain::status::Badge;
use ratatui::style::{Color, Modifier, Style};

/// A palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Theme {
    /// The terminal's own 16 colours.
    Ansi,
    /// `base00`..`base0F`.
    Base16([Color; 16]),
}

/// What a colour is for, besides badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Ordinary text.
    Text,
    /// Borders, labels, the log line.
    Dim,
    /// Pane titles and the mode.
    Title,
    /// The selected row.
    Selected,
    /// Fleet-level errors.
    Error,
    /// Search matches in the list.
    Match,
    /// Act mode, and a confirmation: something that changes a machine is possible or asked.
    Warning,
    /// What an action's process printed on standard error.
    Stderr,
}

impl Theme {
    const fn slot(&self, i: usize, ansi: Color) -> Color {
        match self {
            Self::Ansi => ansi,
            Self::Base16(p) => p[i],
        }
    }

    /// The whole screen's base style: base16's background and text, or nothing at all, so
    /// the terminal's own show through.
    #[must_use]
    pub fn base(&self) -> Style {
        match self {
            Self::Ansi => Style::default(),
            Self::Base16(p) => Style::default().bg(p[0]).fg(p[5]),
        }
    }

    /// A chrome role.
    #[must_use]
    pub fn style(&self, role: Role) -> Style {
        match (role, self) {
            (Role::Text, Self::Ansi) => Style::default(),
            (Role::Text, Self::Base16(p)) => Style::default().fg(p[5]),
            (Role::Dim, _) => Style::default().fg(self.slot(3, Color::DarkGray)),
            (Role::Title, _) => Style::default()
                .fg(self.slot(0xD, Color::Blue))
                .add_modifier(Modifier::BOLD),
            (Role::Selected, Self::Ansi) => Style::default().add_modifier(Modifier::REVERSED),
            (Role::Selected, Self::Base16(p)) => Style::default().bg(p[2]).fg(p[7]),
            (Role::Error, _) => Style::default().fg(self.slot(8, Color::Red)),
            (Role::Warning, _) => Style::default()
                .fg(self.slot(0xA, Color::Yellow))
                .add_modifier(Modifier::BOLD),
            (Role::Stderr, _) => Style::default().fg(self.slot(9, Color::LightRed)),
            (Role::Match, _) => Style::default()
                .fg(self.slot(0xA, Color::Yellow))
                .add_modifier(Modifier::UNDERLINED),
        }
    }

    /// A badge's colour (see the table in the module documentation).
    #[must_use]
    pub fn badge(&self, badge: Badge) -> Style {
        let (slot, ansi) = badge_colour(badge);
        Style::default().fg(self.slot(slot, ansi))
    }
}

/// The base16 slot and the ANSI colour for a badge. One table, read by both palettes and by
/// `--plain`'s colour.
#[must_use]
pub const fn badge_colour(badge: Badge) -> (usize, Color) {
    match badge {
        Badge::Checking => (3, Color::DarkGray),
        Badge::Evaluating => (0xC, Color::Cyan),
        Badge::InSync => (0xB, Color::Green),
        Badge::UpdatePending => (0xA, Color::Yellow),
        Badge::Cached => (0xD, Color::Blue),
        Badge::FailedUnits => (9, Color::LightRed),
        Badge::RebootNeeded => (0xE, Color::Magenta),
        Badge::NotInstalled => (4, Color::Gray),
        Badge::Unreachable => (8, Color::Red),
        Badge::EvalError => (0xF, Color::LightMagenta),
        Badge::Frozen => (0xD, Color::LightBlue),
    }
}

/// Reads a base16 theme file: a flat TOML table, `base00 = "#1d2021"` through `base0F`
/// (the `#` is optional; stylix writes it). Every slot must be there.
///
/// # Errors
///
/// Why the file is unusable, for the note shown before falling back to ANSI.
pub fn parse_base16(text: &str) -> Result<[Color; 16], String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| format!("not valid TOML: {e}"))?;
    let mut palette = [Color::Reset; 16];
    for (i, colour) in palette.iter_mut().enumerate() {
        let name = format!("base{i:02X}");
        let value = table
            .get(&name)
            .ok_or_else(|| format!("`{name}` is missing"))?
            .as_str()
            .ok_or_else(|| format!("`{name}` must be a string like \"#1d2021\""))?;
        *colour = hex(value).ok_or_else(|| {
            format!(
                "`{name}` = {:?} is not a colour like \"#1d2021\"",
                auspex_domain::text::sanitize_line(value)
            )
        })?;
    }
    Ok(palette)
}

fn hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// The largest theme file read. A base16 file is sixteen short lines.
pub const MAX_BYTES: usize = 64 * 1024;

/// Reads `file` if it is a regular file of at most [`MAX_BYTES`].
///
/// Checked before opening, so a FIFO or a device is never opened; opened non-blocking, so a
/// file swapped for a FIFO in between cannot block either; checked again on the open file;
/// and read through a cap, so a file that grows cannot run on.
fn read_bounded(file: &Path) -> Result<String, String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let not_regular = || "it is not a regular file".to_owned();
    let meta = std::fs::metadata(file).map_err(|e| format!("cannot read it: {e}"))?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let nonblock = i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).unwrap_or(0);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nonblock)
        .open(file)
        .map_err(|e| format!("cannot read it: {e}"))?;
    let meta = f.metadata().map_err(|e| format!("cannot read it: {e}"))?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    f.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read it: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("it is larger than {MAX_BYTES} bytes"));
    }
    String::from_utf8(bytes).map_err(|_| "it is not UTF-8".to_owned())
}

/// The theme for `file`: base16 when it reads, otherwise ANSI and a note saying why.
#[must_use]
pub fn load(file: Option<&Path>) -> (Theme, Option<String>) {
    let Some(file) = file else {
        return (Theme::Ansi, None);
    };
    let read = read_bounded(file).and_then(|t| parse_base16(&t));
    match read {
        Ok(p) => (Theme::Base16(p), None),
        Err(why) => (
            Theme::Ansi,
            Some(format!(
                "theme file {}: {why}; using the terminal's ANSI colours",
                file.display()
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        for i in 0..16 {
            let _ = writeln!(s, "base{i:02X} = \"#0000{i:02x}\"");
        }
        s
    }

    #[test]
    fn a_full_palette_reads_with_or_without_the_hash() {
        let p = parse_base16(&full()).unwrap();
        assert_eq!(p[15], Color::Rgb(0, 0, 15));
        let bare = full().replace('#', "");
        assert_eq!(parse_base16(&bare).unwrap(), p);
    }

    #[test]
    fn a_missing_or_malformed_slot_is_refused() {
        let missing = full().replace("base0A = \"#00000a\"\n", "");
        assert!(parse_base16(&missing).unwrap_err().contains("base0A"));
        let bad = full().replace("#00000a", "#zz");
        assert!(parse_base16(&bad).unwrap_err().contains("base0A"));
    }

    #[test]
    fn a_missing_file_falls_back_with_a_note() {
        let (t, note) = load(Some(Path::new("/nonexistent/auspex-theme.toml")));
        assert_eq!(t, Theme::Ansi);
        assert!(note.unwrap().contains("ANSI"));
    }
}
