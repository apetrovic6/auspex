//! auspex's terminal shell: the binary.
//!
//! It owns everything the core and the adapters must not: the terminal, the clock, the tokio
//! runtime, the config file, the command line, and the mapping from each core `Effect` to a
//! function in `auspex-effects`.
//!
//! - [`cli`] and [`config`]: what the operator asked for.
//! - [`shell`]: the effect mapping, cancellation and the global deadline; [`backend`] the
//!   adapters behind it, or [`demo`]'s script.
//! - [`keymap`] and [`input`]: keys to core events; [`ui`] and [`theme`]: drawing.
//! - [`output`]: the streamed output of updates and builds, which the Log tab draws.
//! - [`handoff`]: `s`, lending the terminal to ssh and taking it back.
//! - [`panics`]: what a panic does by where it happens, and the answer a panicked task still gives.
//! - [`packages`]: the package preview as text, for the detail panel and the `d` view.
//! - [`headless`]: `--plain` and `--json`.
//! - [`app`]: putting it together, signals included.
//!
//! A library and a thin binary, so the integration tests reach the pieces directly.

pub mod app;
pub mod backend;
pub mod cli;
pub mod config;
pub mod cordon;
pub mod demo;
pub mod handoff;
pub mod headless;
pub mod input;
pub mod keymap;
pub mod log;
pub mod output;
pub mod packages;
pub mod panics;
pub mod shell;
pub mod theme;
pub mod ui;
