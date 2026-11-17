//! The command line.

use std::path::PathBuf;

use clap::Parser;

/// Which machines in a Clan fleet are reachable and up to date.
///
/// Starts a read-only terminal UI. With --plain or --json it scans once without a terminal,
/// prints the result when the fleet has settled, and exits: 0 for a scan that completed with
/// no fleet-level error; 1 only when the fleet has a problem (a fleet-level error, or the scan
/// did not settle in time); 2 when auspex itself could not do its job (a usage or config
/// error, no usable terminal, the report could not be written); 128 + n after signal n.
#[derive(Debug, Clone, Parser)]
#[command(name = "auspex", version, about, long_about)]
pub struct Args {
    /// The fleet flake. Overrides everything; otherwise the `CLAN_DIR` environment variable,
    /// then the config file's `flake`, then the current directory.
    #[arg(long, value_name = "PATH")]
    pub flake: Option<String>,

    /// The config file. Default: `$XDG_CONFIG_HOME/auspex/config.toml`
    /// (`~/.config/auspex/config.toml`), which may be missing; a file named here must exist.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// A scripted fleet instead of a real one: no machine, nix, cache or network is touched.
    #[arg(long)]
    pub demo: bool,

    /// Scan once, then print one aligned line per machine.
    #[arg(long, conflicts_with = "json")]
    pub plain: bool,

    /// Scan once, then print the fleet report as JSON (the documented, versioned contract).
    #[arg(long)]
    pub json: bool,
}

/// How the result is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The terminal UI.
    Tui,
    /// `--plain`.
    Plain,
    /// `--json`.
    Json,
}

impl Args {
    /// The mode the flags ask for.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        if self.json {
            Mode::Json
        } else if self.plain {
            Mode::Plain
        } else {
            Mode::Tui
        }
    }
}
