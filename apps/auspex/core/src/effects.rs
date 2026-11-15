//! The ports: every request the core can make of the shell, and what each one answers with.
//!
//! Each operation below is one [`Effect`] variant. The shell fulfils it by calling
//! `auspex-effects` and resolving the [`crux_core::Request`] with the operation's `Output`;
//! the core turns that output into an [`Event`](crate::Event) itself, stamped with the
//! request's [`Generation`] and [`RequestId`]. The shell therefore never builds an answer
//! event by hand, and cannot attach an answer to the wrong request.
//!
//! **Failures are values.** Every `Output` can say "it failed, and why". A shell that drops a
//! request instead leaves its machine spinning until the global deadline, with nothing on
//! screen to say why.
//!
//! **Cancellation.** Dropping a Crux command does not stop the shell's process, so the core
//! emits [`Effect::Cancel`] for every request still outstanding when a refresh starts or the
//! user quits, and when the global deadline passes. The shell kills whatever is running for
//! that id and drops the `Request`. An answer it resolves anyway is ignored by the core.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;

use auspex_domain::eval::EvalOutcome;
use auspex_domain::fleet::{Machine, MachineClass, Target};
use auspex_domain::observe::{ProbeOutcome, PromSample, StorePath};
use auspex_domain::status::NarCache;
use crux_core::capability::Operation;
use crux_core::macros::effect;
use crux_core::render::RenderOperation;

/// One refresh. Every request carries the generation it was issued in, and every answer
/// carries it back; an answer from an older generation is dropped without changing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Generation(pub u64);

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Names one request for the life of the core. Never reused, across refreshes included, so a
/// [`CancelRequest`] can never hit a later request that happens to share a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Read the fleet's machine list (the Clan inventory).
///
/// The shell runs the inventory eval for `flake` and parses it with
/// [`auspex_domain::fleet::parse_inventory`]. Nothing else is asked for until this answers:
/// probes landing before the list is one of the plan's known traps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The fleet flake, as configured (`--flake`, `CLAN_DIR`).
    pub flake: String,
}

/// The machines, or why the list could not be read. A failure here is the one fleet-level
/// error: with no list there are no rows to put it on.
pub type InventoryAnswer = Result<Vec<Machine>, String>;

impl Operation for InventoryRequest {
    type Output = InventoryAnswer;
}

/// Probe one machine over ssh.
///
/// The adapter runs [`auspex_domain::fleet::ssh_args`] for `target`, then
/// [`auspex_domain::fleet::PROBE_REMOTE_COMMAND`] with
/// [`auspex_domain::observe::probe_script`] for `class` on stdin, and
/// [`auspex_domain::observe::parse_probe`] over what comes back.
///
/// The per-probe timeout is the adapter's (`ProbeExit::TimedOut`); the global deadline is
/// [`Event::DeadlinePassed`](crate::Event::DeadlinePassed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The machine, for logging. The answer is matched by `id`, not by this.
    pub machine: String,
    /// Which probe script to run.
    pub class: MachineClass,
    /// The resolved, already-validated target. A machine whose target was refused is never
    /// probed, so this is always a value that passed [`Target::parse`].
    pub target: Target,
}

impl Operation for ProbeRequest {
    type Output = ProbeOutcome;
}

/// One machine to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalMachine {
    /// The machine's name: its attribute under the class's flake output.
    pub name: String,
    /// Picks `nixosConfigurations` or `darwinConfigurations`
    /// ([`MachineClass::flake_output`]).
    pub class: MachineClass,
}

/// Evaluate every machine's toplevel with `nix-eval-jobs` (one evaluator per machine, through
/// the eval cache).
///
/// A **stream**: the shell resolves the same request once per machine, in whatever order they
/// finish, with [`EvalAnswer::Machine`], and then once with [`EvalAnswer::Finished`], after
/// which it drops the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The fleet flake.
    pub flake: String,
    /// Every inventory machine, sorted by name, refused targets included: evaluation does not
    /// need ssh.
    pub machines: Vec<EvalMachine>,
}

/// One item of the evaluation stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalAnswer {
    /// One machine's result, success or error. An error stays on that machine's row.
    Machine {
        /// Which machine. The adapter maps nix-eval-jobs' `attr` back to the name it was
        /// given; a name the request did not contain is ignored.
        machine: String,
        /// What it evaluated to.
        outcome: EvalOutcome,
    },
    /// The evaluator is done. Any machine that has no result by now gets an eval error:
    /// `error`'s text when the evaluator itself failed, a generic reason otherwise. Without
    /// that, a machine nix-eval-jobs skipped would spin until the deadline.
    Finished {
        /// Why the evaluator as a whole failed (nix-eval-jobs crashed, the flake did not
        /// lock), or `None` when it ran to its end.
        error: Option<String>,
    },
}

impl Operation for EvalRequest {
    type Output = EvalAnswer;
}

/// Run the fleet-wide Prometheus query. Only asked when Prometheus is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrometheusRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The `instance` labels wanted, sorted and unique.
    pub instances: Vec<String>,
}

/// Samples keyed by `instance` label, or why Prometheus could not be asked.
///
/// An instance with no series is simply absent. Either way is fine: a machine without a fresh sample falls
/// back to what its ssh probe collected. `age_seconds` is the adapter's to compute, since the
/// clock is an effect.
pub type PrometheusAnswer = Result<BTreeMap<String, PromSample>, String>;

impl Operation for PrometheusRequest {
    type Output = PrometheusAnswer;
}

/// Ask the binary cache whether `store_path` is there (`HEAD <cache>/<hash>.narinfo`).
///
/// Only asked when a cache is configured, and only for a machine whose evaluated toplevel is
/// known, differs from its current system, and is therefore what a deploy would copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The machine, for logging.
    pub machine: String,
    /// The pending toplevel.
    pub store_path: StorePath,
}

impl Operation for CacheRequest {
    /// [`NarCache::Cached`], [`NarCache::NotCached`], or [`NarCache::Failed`] when the lookup
    /// failed. While it is out the row is [`NarCache::Pending`] and not settled; an answer of
    /// `Pending` or `NotApplicable` is read as `Failed`, so a confused adapter cannot hold the
    /// row open until the deadline. At the deadline an unanswered lookup becomes `Failed`.
    type Output = NarCache;
}

/// Resolve hostname targets to addresses, for the shared-IP lint. Targets that are already
/// IP literals are not sent: the core knows their address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// Machine name to the hostname its target names.
    pub hosts: BTreeMap<String, String>,
}

impl Operation for ResolveRequest {
    /// Machine name to every address its hostname resolved to. A machine that is missing or
    /// has none did not resolve; that is not an error, it just cannot collide. An entry for a
    /// machine that is not in `hosts` is ignored: it can neither invent a collision nor
    /// overwrite an IP-literal target's address.
    type Output = BTreeMap<String, Vec<IpAddr>>;
}

/// Stop request `id`: kill its process and drop its `Request`. Fire and forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelRequest {
    /// The request to stop.
    pub id: RequestId,
}

impl Operation for CancelRequest {
    type Output = ();
}

/// Everything the core can ask the shell to do.
///
/// Plain `#[effect]`, without the template's `facet_typegen`: nothing serialises an effect,
/// because the only shell links this crate and matches on the enum directly. That match is
/// exhaustive, so a variant added here is a compile error in the shell until it is fulfilled.
#[effect]
#[derive(Debug)]
pub enum Effect {
    /// Redraw from [`crate::Auspex`]'s view. The TUI's frame timer repaints anyway; headless
    /// mode can ignore it.
    Render(RenderOperation),
    /// [`InventoryRequest`].
    Inventory(InventoryRequest),
    /// [`ProbeRequest`].
    Probe(ProbeRequest),
    /// [`EvalRequest`], a stream.
    Evaluate(EvalRequest),
    /// [`PrometheusRequest`].
    Prometheus(PrometheusRequest),
    /// [`CacheRequest`].
    CacheLookup(CacheRequest),
    /// [`ResolveRequest`].
    ResolveAddresses(ResolveRequest),
    /// [`CancelRequest`].
    Cancel(CancelRequest),
}
