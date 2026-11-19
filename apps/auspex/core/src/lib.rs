//! auspex's Crux core: what to ask the fleet, in what order, and what the answers mean.
//!
//! Orchestration only. The rules live in `auspex-domain`, which this crate calls; the I/O
//! lives in `auspex-effects`, which this crate never sees. The [`Effect`] enum is the set of
//! ports, and the shell's handlers are the adapters:
//!
//! ```text
//! shells/tui ──► core (this) ──► auspex-domain
//!     └────────► auspex-effects ──┘
//! ```
//!
//! - [`effects`]: the ports, one operation per request, with what each answers.
//! - [`Event`]: what the shell tells the core, including the answers the core builds itself.
//! - [`ViewModel`]: what a shell draws; [`Model::fleet_settled`] is when headless mode prints.
//!
//! No I/O, no async runtime and no clock: a rule that needs "now" receives an age, and the
//! global deadline arrives as [`Event::DeadlinePassed`].

mod app;
pub mod config;
pub mod effects;
pub mod view;

pub use app::{
    Auspex, CACHE_LOOKUP_FAILED, EVAL_MISSING, EVAL_TIMED_OUT, Event, INVENTORY_TIMED_OUT, Model,
    NO_DEPLOYED_SYSTEM, PENDING_SYSTEM_MISMATCH, PREVIEW_TIMED_OUT, PROBE_TIMED_OUT,
    PROMETHEUS_TIMED_OUT, SYSTEM_CHANGED,
};
pub use auspex_domain::report::{FleetError, FleetErrorKind};
pub use config::{Config, DiffConfig, PrometheusConfig};
pub use crux_core::Core;
pub use effects::{Effect, Generation, RequestId};
pub use view::{Mode, Phase, PrometheusStatus, RefreshView, SearchView, ViewModel};
