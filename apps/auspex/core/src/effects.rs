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
//!
//! The one exception to "the global deadline cancels everything" is the package preview's two
//! closure queries ([`DeployedClosureRequest`], [`PendingClosureRequest`]): they start only
//! after a machine's probe, evaluation and cache lookup are in, so `timeouts.settle` may
//! already be nearly spent, and the preview must not make the scan "timed out". They carry
//! their own deadline, `timeouts.diff`, which the shell enforces, and the fleet settles when
//! they have answered.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;

use auspex_domain::closure::ClosurePaths;
use auspex_domain::eval::EvalOutcome;
use auspex_domain::fleet::{Machine, MachineClass, Target};
use auspex_domain::observe::{ProbeOutcome, PromSample, StorePath};
use auspex_domain::preflight::TreeState;
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

/// Query what `target` runs: its `/run/current-system` closure.
///
/// The adapter authenticates like a probe (the same ssh control master, the same approval
/// phase when none is up), runs `nix-store --query --requisites /run/current-system` there
/// and hands the output to [`auspex_domain::closure::parse_closure`]. Only asked for a
/// machine whose probe observed a system and whose pending toplevel is known, differs and is
/// cached: there is nothing to compare otherwise.
///
/// `timeout_seconds` is `timeouts.diff`: the adapter's budget for the query, and the shell's
/// bound on the whole request, approval phase included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployedClosureRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The machine, for logging.
    pub machine: String,
    /// The resolved, already-validated target the probe used.
    pub target: Target,
    /// `timeouts.diff`.
    pub timeout_seconds: u64,
}

/// The closure's store paths, or one line on why it could not be had. A failure here makes
/// the machine's preview `failed`; it never changes the machine's status.
pub type ClosureAnswer = Result<ClosurePaths, String>;

impl Operation for DeployedClosureRequest {
    type Output = ClosureAnswer;
}

/// Query what `store_path`, the pending toplevel, would run: its closure.
///
/// The adapter uses the local store when the path is valid there (`nix-store --query
/// --requisites`), otherwise the binary cache (`nix path-info --recursive --store <cache>`),
/// and writes nothing to the local store either way. Only asked when the core's cache lookup
/// said [`NarCache::Cached`]: without that the preview is `not_cached` and no query runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClosureRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The machine, for logging.
    pub machine: String,
    /// The pending toplevel.
    pub store_path: StorePath,
    /// `timeouts.diff`.
    pub timeout_seconds: u64,
}

impl Operation for PendingClosureRequest {
    type Output = ClosureAnswer;
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

/// The checks that need the world, run after `u` and before the confirmation: the state of
/// the fleet repository and the size of the closure the update would copy.
///
/// The shell fulfils it with `auspex-effects`'s `preflight` adapters (one read-only `git
/// status`, one `nix path-info --recursive --size`), both bounded by `timeout_seconds`, and
/// answers once with both results. Each is a value, so a check that could not run says why
/// and the core shows it as a warning; the shell never drops this request. What the core adds
/// to the answer (the probe's free space, the machine addresses, the `public` flag) it
/// already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightRequest {
    /// This request.
    pub id: RequestId,
    /// The machine, for logging.
    pub machine: String,
    /// The fleet flake, as configured: where the repository to check is.
    pub flake: String,
    /// The pending toplevel, whose closure is sized.
    pub store_path: StorePath,
    /// `timeouts.diff`.
    pub timeout_seconds: u64,
}

/// What the shell found out for the checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightAnswer {
    /// The fleet repository's state, or one line on why it could not be read (not a local
    /// directory, not a git repository, git failed or timed out).
    pub tree: Result<TreeState, String>,
    /// The pending closure's NAR size in bytes, or one line on why it is not known.
    pub closure_bytes: Result<u64, String>,
}

impl Operation for PreflightRequest {
    type Output = PreflightAnswer;
}

/// What an action does to a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// `clan machines update`: deploy to the machine.
    Update,
    /// `nix build` of the pending system, here, changing nothing on the machine.
    Build,
}

impl ActionKind {
    /// The word for the status bar and the confirmation: `update`, `build`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Build => "build",
        }
    }
}

/// Run one action against one machine, streaming its output.
///
/// **The one request that is not a pure question.** It starts a process that changes a machine
/// (`Update`) or fills the local store (`Build`), so the core issues it only after the
/// operator has said yes, and only for a machine it holds a row for. The shell resolves `Update`
/// with the fleet's pinned `clan` (resolved here, not by the core), runs exactly
/// `clan machines update --host-key-check strict --flake <fleet> -- <machine>`, and `Build`
/// with `nix build --no-link --print-out-paths <fleet>#…`; it re-checks the machine name and,
/// for a build, the system, and refuses before spawning anything.
///
/// The process's output goes to the shell's own log buffer, line by line, never through the
/// core: a deploy can print hundreds of thousands of lines, and nothing the core decides
/// depends on them. The core hears once, with [`ActionEnd`].
///
/// **Answered on cancellation too.** Unlike every other request, the shell answers a cancelled
/// action ([`ActionResult::Cancelled`]), after the process group has been killed and reaped:
/// quitting waits for that answer, which is how "auspex exits only when nothing is left
/// running" is a fact the core knows rather than one it hopes for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRequest {
    /// This request.
    pub id: RequestId,
    /// What to do.
    pub kind: ActionKind,
    /// The machine, validated against the inventory and the domain's name rules.
    pub machine: String,
    /// NixOS or darwin: which flake output a build names.
    pub class: MachineClass,
    /// The nix system of the machine's evaluated toplevel, when the evaluation said. A build
    /// of a machine whose system is not this one's is refused.
    pub system: Option<String>,
    /// The fleet flake, as configured.
    pub flake: String,
}

/// How an action ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionResult {
    /// The process exited 0.
    Succeeded,
    /// It ran and failed, or could not be started, or was refused.
    Failed,
    /// It took too long and was killed.
    TimedOut,
    /// The operator stopped it; the process group is gone.
    Cancelled,
}

impl ActionResult {
    /// For the status bar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
            Self::Cancelled => "cancelled",
        }
    }
}

/// What an action did, once it is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionEnd {
    /// How it ended.
    pub result: ActionResult,
    /// The exit code, when the process exited with one.
    pub exit_code: Option<i32>,
    /// A short summary: the last few lines it printed (stderr's, else stdout's), or why it
    /// could not start. Never the whole output.
    pub summary: Vec<String>,
    /// Plain words about a deploy that failed at ssh, and the fix
    /// ([`auspex_domain::act::explain_ssh_failure`]).
    pub explanation: Option<String>,
}

impl Operation for ActionRequest {
    type Output = ActionEnd;
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
    /// [`DeployedClosureRequest`].
    DeployedClosure(DeployedClosureRequest),
    /// [`PendingClosureRequest`].
    PendingClosure(PendingClosureRequest),
    /// [`PreflightRequest`]: the checks before an update.
    Preflight(PreflightRequest),
    /// [`CancelRequest`].
    Cancel(CancelRequest),
    /// [`ActionRequest`]: an update or a build, with its output streamed to the shell's log.
    RunAction(ActionRequest),
}
