//! Putting it together: arguments, config, the runtime, termination signals, and the two
//! ways of running (headless and the TUI).
//!
//! # Signals
//!
//! Children run in sessions of their own, so the terminal's signals never reach them, and if
//! auspex died from a signal no destructor would run to kill them. So an
//! [`auspex_effects::Termination`] is created once, inside the runtime and before anything
//! can spawn a child, and kept until exit. From then on SIGINT, SIGTERM, SIGHUP and SIGQUIT
//! no longer kill auspex: each mode's loop selects on it, and when it fires the shell
//! cancels every token, waits for the adapters to kill and reap their process groups,
//! restores the terminal and exits with 128 + the signal's number.
//!
//! # The terminal
//!
//! `ratatui::try_init` installs a panic hook that restores the terminal before the panic
//! message is printed; every other exit path calls `ratatui::restore` itself.

use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use auspex_core::{Config, DiffConfig, Event, PrometheusConfig, PrometheusStatus};
use auspex_effects::Termination;
use clap::Parser as _;
use crossterm::event::EventStream;
use tokio_stream::StreamExt as _;

use crate::backend::{Backend, Real};
use crate::cli::{Args, Mode};
use crate::config::{self, FlakeSource, Settings};
use crate::demo::{self, Demo};
use crate::headless;
use crate::input::Screen;
use crate::keymap::{Key, Keymap};
use crate::log::{Log, say};
use crate::shell::Shell;
use crate::theme::{self, Theme};
use crate::ui::{Context, Ui};

/// The TUI's frame rate. The frame timer repaints whether or not anything changed, so
/// answers arriving on other tasks never touch the terminal themselves.
const FRAME: Duration = Duration::from_millis(33);

/// Anything that is not the fleet's problem: a usage or config error, or auspex itself
/// unable to do its job (no usable terminal, the report could not be written, no runtime).
/// 1 means only "the fleet has a problem" (review C4); signals are 128 + n.
const USAGE: u8 = 2;

/// The core's configuration from the settings and the resolved flake.
#[must_use]
pub fn core_config(settings: &Settings, flake: String) -> Config {
    Config {
        flake,
        target_overrides: settings
            .machines
            .iter()
            .filter_map(|(name, m)| m.target.clone().map(|t| (name.clone(), t)))
            .collect(),
        prometheus: settings.prometheus.as_ref().map(|p| PrometheusConfig {
            // Only the labels the config names: for every other machine the core asks under
            // the machine's name, which the adapter turns into the default `<name>:9100`.
            instances: settings
                .machines
                .iter()
                .filter_map(|(name, m)| m.prometheus_instance.clone().map(|i| (name.clone(), i)))
                .collect(),
            max_age_seconds: p.stale_after,
        }),
        binary_cache: settings.cache_url.is_some(),
        diff: settings.diff.enabled.then_some(DiffConfig {
            timeout_seconds: settings.timeouts.diff,
        }),
    }
}

const fn signal_exit(name: &str) -> u8 {
    let number = match name.as_bytes() {
        b"SIGHUP" => 1,
        b"SIGINT" => 2,
        b"SIGQUIT" => 3,
        _ => 15,
    };
    128 + number
}

/// `main`.
pub fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(a) => a,
        Err(e) => {
            if e.use_stderr() {
                // clap quotes the offending argument; plain text, sanitised (finding 4).
                crate::log::to_stderr(&crate::log::clean(&e.render().to_string()));
            } else {
                let _ = e.print(); // --help, --version: our own text
            }
            return if e.use_stderr() {
                ExitCode::from(USAGE)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let loaded = match config::load(args.config.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            say(&e.to_string());
            return ExitCode::from(USAGE);
        }
    };
    let mut warnings = loaded.warnings;
    let keymap = match Keymap::with_overrides(&loaded.settings.keymap, &mut warnings) {
        Ok(k) => k,
        Err(e) => {
            let file = loaded
                .path
                .as_ref()
                .map_or_else(String::new, |p| format!("config file {}: ", p.display()));
            say(&format!("{file}{e}"));
            return ExitCode::from(USAGE);
        }
    };
    let clan_dir = std::env::var("CLAN_DIR").ok();
    let (flake, source) = match config::resolve_flake(
        args.flake.as_deref(),
        clan_dir.as_deref(),
        loaded.settings.flake.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            say(&e);
            return ExitCode::from(USAGE);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            say(&format!("cannot start the async runtime: {e}"));
            return ExitCode::from(USAGE);
        }
    };
    // Synchronously, before `Termination` takes the signals over (the runtime is built
    // already, but nothing runs on it yet): the read is bounded now (review finding 3), and even so nothing slow may sit
    // between taking SIGINT and the first place that listens for it.
    let (theme, theme_note) = if args.mode() == Mode::Tui {
        theme::load(loaded.settings.theme_file.as_deref())
    } else {
        (Theme::Ansi, None)
    };
    warnings.extend(theme_note);
    let run = Run {
        args,
        settings: loaded.settings,
        warnings,
        keymap,
        flake,
        source,
        theme,
    };
    let code = runtime.block_on(run.run());
    // A DNS lookup cannot be interrupted, only abandoned: do not wait for its thread.
    runtime.shutdown_timeout(Duration::from_millis(500));
    code
}

struct Run {
    args: Args,
    settings: Settings,
    warnings: Vec<String>,
    keymap: Keymap,
    flake: String,
    source: FlakeSource,
    theme: Theme,
}

impl Run {
    async fn run(self) -> ExitCode {
        // Before anything can spawn a child: from here on the four termination signals are
        // ours to act on, and none is lost between two polls.
        let mut term = match Termination::new() {
            Ok(t) => t,
            Err(e) => {
                say(&format!("cannot handle termination signals: {e}"));
                return ExitCode::from(USAGE);
            }
        };
        let mode = self.args.mode();
        let log = if mode == Mode::Tui {
            Log::memory()
        } else {
            Log::Stderr
        };
        for w in &self.warnings {
            log.line(w);
        }
        let (backend, config, shown) = if self.args.demo {
            (
                Backend::Demo(Demo::default()),
                demo::config(),
                demo::FLAKE.to_owned(),
            )
        } else {
            let real = Real::new(&self.settings, self.source);
            for n in real.notes() {
                log.line(&n);
            }
            (
                Backend::Real(Box::new(real)),
                core_config(&self.settings, self.flake.clone()),
                format!("{} (from {})", self.flake, self.source.describe()),
            )
        };
        let settle = self.settings.timeouts.settle;
        let shell = Shell::new(backend, log, Duration::from_secs(settle));
        if !self.args.demo {
            shell.expect_machines(self.settings.machines.keys().cloned().collect());
        }
        match mode {
            Mode::Plain | Mode::Json => headless(&shell, config, mode, &mut term, settle).await,
            Mode::Tui => tui(&shell, config, &self.keymap, &self.theme, &shown, &mut term).await,
        }
    }
}

async fn headless(
    shell: &Arc<Shell>,
    config: Config,
    mode: Mode,
    term: &mut Termination,
    settle: u64,
) -> ExitCode {
    shell.send(Event::Start(config));
    // `refresh.settled` always becomes true: the deadline marks whatever is still pending.
    while !shell.view().refresh.settled {
        tokio::select! {
            () = shell.changed() => {}
            signal = term.recv() => {
                shell.log.line(&format!("{signal}: stopping"));
                shell.shutdown().await;
                return ExitCode::from(signal_exit(signal));
            }
        }
    }
    let view = shell.view();
    shell.shutdown().await;
    // The TUI shows "prom: down"; headless output would otherwise not say it at all (C3).
    if let PrometheusStatus::Unavailable { reason } = &view.prometheus {
        shell.log.line(&format!(
            "prometheus unavailable, so health comes from ssh: {reason}"
        ));
    }
    let text = match mode {
        Mode::Json => headless::json(&view.report),
        _ => headless::plain(&view, std::io::stdout().is_terminal(), settle),
    };
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        // A closed pipe (`| head`) is not worth a message on a broken stream; still 2, as
        // the report was not delivered.
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            say(&format!("cannot write the report: {e}"));
        }
        return ExitCode::from(USAGE);
    }
    ExitCode::from(headless::exit_status(&view))
}

async fn tui(
    shell: &Arc<Shell>,
    config: Config,
    keymap: &Keymap,
    theme: &Theme,
    flake: &str,
    term: &mut Termination,
) -> ExitCode {
    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            say(&format!(
                "cannot use the terminal ({e}); try --plain or --json"
            ));
            return ExitCode::from(USAGE);
        }
    };
    shell.send(Event::Start(config));
    let mut ui = Ui::default();
    let mut events = EventStream::new();
    let mut frames = tokio::time::interval(FRAME);
    let mut failure: Option<String> = None;
    // When the current refresh started, so a second `r` can tell a list that is merely
    // starting from one that hangs.
    let mut refresh = (shell.view().refresh.generation, std::time::Instant::now());
    let mut code = ExitCode::SUCCESS;
    while !shell.view().quit {
        tokio::select! {
            _ = frames.tick() => {
                ui.frame = ui.frame.wrapping_add(1);
                let log = shell.log.lines();
                let ctx = Context { theme, keymap, flake, log: &log };
                if let Err(e) = terminal.draw(|f| shell.draw(f, &mut ui, &ctx)) {
                    failure = Some(format!("cannot draw: {e}"));
                    code = ExitCode::from(USAGE);
                    break;
                }
            }
            event = events.next() => match event {
                Some(Ok(event)) => {
                    let Some(key) = event.as_key_press_event().and_then(|k| Key::from_event(&k)) else {
                        continue;
                    };
                    let view = shell.view();
                    let query = view.search.as_ref().map(|s| s.query.clone()).unwrap_or_default();
                    if view.refresh.generation != refresh.0 {
                        refresh = (view.refresh.generation, std::time::Instant::now());
                    }
                    let screen = Screen::from_view(&view, ui.page, refresh.1.elapsed());
                    for e in ui.input.key(key, screen, keymap, &query) {
                        shell.send(e);
                    }
                }
                // Matched here rather than as a `Some(Ok(..))` pattern on the arm, which
                // `select!` answers by disabling the branch: the frame timer would go on
                // painting a screen no key can reach.
                Some(Err(e)) => {
                    // Said after the terminal is restored, so it is readable (review finding 6).
                    failure = Some(format!("cannot read the terminal: {e}"));
                    code = ExitCode::from(USAGE);
                    break;
                }
                None => shell.send(Event::Quit),
            },
            signal = term.recv() => {
                code = ExitCode::from(signal_exit(signal));
                break;
            }
        }
    }
    // Cancel and reap first, then give the terminal back.
    shell.send(Event::Quit);
    shell.shutdown().await;
    ratatui::restore();
    if let Some(f) = failure {
        say(&f);
    }
    code
}
