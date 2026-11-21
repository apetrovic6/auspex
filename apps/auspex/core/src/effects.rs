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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;

use auspex_domain::ci::{CiStatus, HeadFacts};
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

/// Read `HEAD` of the fleet checkout for the CI badge: its commit, where it stands against its
/// upstream and, when `want_origin`, the `origin` remote (parsed, with any credentials dropped).
///
/// Only asked when `[forge]` is configured. Independent of the machine list: the badge never
/// waits for a row and no row waits for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeHeadRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// The fleet flake, as configured.
    pub flake: String,
    /// Whether the repository is to be read from the `origin` remote (`forge.repo` is not set).
    pub want_origin: bool,
}

/// What `HEAD` is, or why it could not be read. An `origin` that is missing or unusable is not a
/// failure here: it is [`HeadFacts::origin`], because it only matters without `forge.repo`.
pub type ForgeHeadAnswer = Result<HeadFacts, String>;

impl Operation for ForgeHeadRequest {
    type Output = ForgeHeadAnswer;
}

/// Ask the forge for the combined commit status of `commit` in `repo` (one request).
///
/// Only asked after [`ForgeHeadRequest`] answered and [`auspex_domain::ci::plan`] decided the
/// forge is to be asked: the commit is pushed and the repository is known. The URL and the token
/// are the shell's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeStatusRequest {
    /// This request.
    pub id: RequestId,
    /// The refresh it belongs to.
    pub generation: Generation,
    /// `owner/name`, validated by the domain.
    pub repo: String,
    /// The full commit id, validated by the domain.
    pub commit: String,
}

/// What the forge says about the commit, or why it could not be asked (a refused token, a
/// timeout, a malformed answer: one line, which never holds the token).
pub type ForgeStatusAnswer = Result<CiStatus, String>;

impl Operation for ForgeStatusRequest {
    type Output = ForgeStatusAnswer;
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
    /// Every path of the pending closure with its NAR size, or one line on why that is not
    /// known. The core leaves out the paths the machine already has (when it knows its closure)
    /// and sums the rest.
    pub closure: Result<Vec<(StorePath, u64)>, String>,
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
    /// The inventory's `deploy.buildHost` (sanitised), which decides where `clan machines
    /// update` builds. Only shown, in [`ActionRequest::start_note`].
    pub build_host: Option<String>,
    /// The machine's resolved ssh target in its canonical spelling, when it has one.
    pub target: Option<String>,
    /// The Log tab run this action writes into, when it is one machine of an update of all
    /// (`U`): the core began that run (and ends it), so the shell neither begins nor ends one and
    /// only adds lines to it. `None`: a lone `u` or `b`, whose run the shell begins and ends
    /// itself, keyed by this request's id.
    pub log: Option<RequestId>,
}

impl ActionRequest {
    /// The first Log tab line of this action: where it builds. A build is always local; an
    /// update builds where the inventory's `deploy.buildHost` says, and clan builds on the
    /// target itself when it says nothing.
    ///
    /// The build host is inventory text, so it is sanitised again here: the line is drawn
    /// as it is.
    #[must_use]
    pub fn start_note(&self) -> String {
        use auspex_domain::text::sanitize_line;
        if self.kind == ActionKind::Build {
            return "building on this machine".to_owned();
        }
        let host = self.build_host.as_deref().map(sanitize_line);
        match host.as_deref() {
            None | Some("") => "building on the target itself (no deploy.buildHost set)".to_owned(),
            Some("localhost") => {
                let copy = self
                    .target
                    .as_deref()
                    .map(|t| format!(", then copying to {}", sanitize_line(t)))
                    .unwrap_or_default();
                format!("building on this machine (deploy.buildHost = localhost){copy}")
            }
            Some(h) => format!("building on {h} (deploy.buildHost)"),
        }
    }
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
    /// It was not started: the shell's own last look (the freeze state file) refused it. Not a
    /// failure of the machine, so an update of all skips it and carries on.
    Refused,
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
            Self::Refused => "refused",
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

/// Open the operator's own ssh session to a machine, with the terminal (`s`).
///
/// Unlike every other request this one is not a background call: the shell suspends its UI,
/// gives the terminal to `ssh` and takes it back when ssh ends, so it can be answered only
/// then. The core cannot cancel it (the operator is typing into ssh, not into auspex), so it
/// has no [`CancelRequest`] either: on a termination signal the shell ends ssh itself.
///
/// The core has already checked that act mode is on and that `target` is the machine's
/// resolved, validated target; the shell builds the argv from the domain
/// ([`auspex_domain::fleet::SshOperation::Interactive`]) and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRequest {
    /// This request.
    pub id: RequestId,
    /// The machine, for the status bar.
    pub machine: String,
    /// The machine's resolved target, which passed [`Target::parse`].
    pub target: Target,
}

/// How an ssh session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEnd {
    /// ssh exited with this status (255 is ssh's own failure).
    Exited {
        /// The exit code.
        code: i32,
    },
    /// ssh was killed by this signal.
    Signaled {
        /// The signal's number.
        signal: i32,
    },
    /// ssh never ran: no terminal to give it, a demo run, a program that cannot be found.
    NotStarted {
        /// Why, in a line.
        reason: String,
    },
}

impl Operation for ShellRequest {
    type Output = ShellEnd;
}

/// Keep one freeze (`f`): freeze or unfreeze `machine` in the state file, and answer with every
/// machine frozen afterwards.
///
/// **A write to auspex's own state, never to a machine.** The shell changes only this machine's
/// entry, under a lock, so two running auspex processes do not lose each other's freezes, and
/// answers with the whole set as written, which the core adopts. The machine name was validated
/// against the inventory and the domain's name rule before the request was made; the adapter
/// checks the rule again.
///
/// Unlike an action it is not cancelled by a refresh or by quitting: it is one short write, and
/// the shell waits for it when it shuts down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFreezeRequest {
    /// This request.
    pub id: RequestId,
    /// The machine.
    pub machine: String,
    /// `true` to freeze it, `false` to unfreeze.
    pub frozen: bool,
    /// Machines frozen in this run whose own save failed, so the file does not hold them: the
    /// adapter freezes them in the same write, so a later save that works also keeps them (see
    /// `app/freeze.rs`). Never contains `machine`.
    pub also_frozen: BTreeSet<String>,
}

/// The machines frozen after the change, or one line on why it could not be kept. Failures are
/// values: the core says so in a notice, and keeps the machine frozen if it was a freeze.
pub type SaveFreezeAnswer = Result<BTreeSet<String>, String>;

impl Operation for SaveFreezeRequest {
    type Output = SaveFreezeAnswer;
}

/// Read the freeze state file again: which machines `f` froze, as the file says now.
///
/// Asked at the `y` of an update, as the last decision before it is requested: another running
/// auspex may have frozen the machine since this one read the file at start-up. The answer is
/// the whole set, or one line on why the file could not be read; the core refuses the update on
/// an error, because a machine that may be frozen is not one to deploy to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadFreezesRequest {
    /// This request.
    pub id: RequestId,
}

/// The machines frozen with `f` now, or why the file could not be read.
pub type LoadFreezesAnswer = Result<BTreeSet<String>, String>;

impl Operation for LoadFreezesRequest {
    type Output = LoadFreezesAnswer;
}

/// One call to the cluster, through the operator's `kubectl` and kubeconfig (an update of all,
/// `U`, for the machines of a Kubernetes rolling group).
///
/// The shell resolves the kubeconfig itself (`kubernetes.kubeconfig`, `$KUBECONFIG`,
/// `~/.kube/config`), so the core never names a path; it asks for the operation and gets a value
/// back. A call that cannot be made says why ([`KubeAnswer`]'s `Err`).
///
/// **Answered on cancellation too**, with [`KubeReply::Cancelled`], after `kubectl` has been
/// killed and reaped. A drain cancelled by a quit has cordoned the node and may have been in the
/// middle of the call: the core uncordons only once it knows the drain is over, and it could not
/// know that from a request that is simply dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubeRequest {
    /// This request.
    pub id: RequestId,
    /// The Log tab run the call's output (a drain's) is added to; see [`ActionRequest::log`].
    pub log: Option<RequestId>,
    /// The kubeconfig context to pin the call to (`--context`), resolved once for the plan. `None`
    /// only for the plan's own questions, before it is known.
    pub context: Option<String>,
    /// What to do.
    pub op: KubeOp,
}

/// What a [`KubeRequest`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KubeOp {
    /// `kubectl config view --minify -o json`: which cluster the kubeconfig points at.
    Context,
    /// `kubectl get nodes -o json`.
    Nodes,
    /// `kubectl get pods -n kube-system -l component=etcd -o json`: the etcd member pods.
    EtcdPods,
    /// `kubectl uncordon <node>`.
    Uncordon {
        /// The node, named like its machine.
        node: String,
    },
    /// `kubectl drain <node> --ignore-daemonsets --delete-emptydir-data --timeout <t>s`. Cordons
    /// the node first, and leaves it cordoned when it fails.
    Drain {
        /// The node, named like its machine.
        node: String,
        /// kubectl's own `--timeout`.
        timeout_seconds: u64,
    },
}

/// What a successful [`KubeRequest`] came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KubeReply {
    /// The kubeconfig's current context and API server.
    Cluster(auspex_domain::roll::ClusterId),
    /// The cluster's nodes.
    Nodes(Vec<auspex_domain::roll::Node>),
    /// The etcd pods, one per member.
    EtcdPods(Vec<auspex_domain::roll::EtcdPod>),
    /// The node was drained or uncordoned.
    Done,
    /// The request was cancelled; `kubectl` is gone.
    Cancelled,
}

/// The reply, or one line on why the call failed.
pub type KubeAnswer = Result<KubeReply, String>;

impl Operation for KubeRequest {
    type Output = KubeAnswer;
}

/// Wait until a machine that was just updated is healthy again, and (for a Kubernetes machine)
/// its node is `Ready`: the pause between two machines of a rolling group.
///
/// The shell polls: it probes `target` (the same probe a refresh runs), asks the domain whether
/// the answer is [`auspex_domain::roll::healthy`] for `expect`, and when `node` is given also
/// reads the cluster's nodes ([`auspex_domain::roll::node_state`]); it asks again every
/// `poll_seconds` until both hold or `timeout_seconds` has passed. Time is the shell's: the core has no
/// clock. Cancelling it ends the polling; it is not answered then (it has nothing to reap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitRequest {
    /// This request.
    pub id: RequestId,
    /// The Log tab run the polling's progress is added to; see [`ActionRequest::log`].
    pub log: Option<RequestId>,
    /// The machine, for the log.
    pub machine: String,
    /// Which probe script to run.
    pub class: MachineClass,
    /// The resolved, already-validated ssh target.
    pub target: Target,
    /// The system the update deployed: what `/run/current-system` must be.
    pub expect: StorePath,
    /// The Kubernetes node to wait for, when the machine's rolling group asks for it.
    pub node: Option<String>,
    /// The machine's etcd pod must be `Ready` too before it counts as back (its rolling group asks
    /// for the etcd check). Only with `node`.
    pub etcd: bool,
    /// The kubeconfig context the node is read in (`--context`).
    pub context: Option<String>,
    /// The machine's failed units when its turn began: what was already wrong is not the update's
    /// doing ([`auspex_domain::roll::healthy`]).
    pub baseline: Vec<String>,
    /// How long to wait before giving up.
    pub timeout_seconds: u64,
    /// How long to wait between two looks (`update.poll_interval`).
    pub poll_seconds: u64,
}

/// `Ok(())` when the machine is healthy (and its node `Ready`); otherwise why it was not by the
/// deadline, naming the last thing seen.
pub type AwaitAnswer = Result<(), String>;

impl Operation for AwaitRequest {
    type Output = AwaitAnswer;
}

/// What to write to the Log tab (fire and forget).
///
/// A run is a titled section of the tab. An update of all begins a run of its own (its plan, a
/// line per machine as it finishes, the totals) and one more for each machine; the shell adds the
/// process output of that machine's actions to the machine's run ([`ActionRequest::log`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    /// The run, a number the core made up ([`RequestId`] is only a source of unique ones).
    pub run: RequestId,
    /// What to do with it.
    pub op: LogOp,
}

/// What a [`LogRequest`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOp {
    /// Opens the run. `group`: it is the first of a new group of runs (the update of all's own),
    /// so the tab drops what it kept from before; otherwise it joins the group that is open.
    Begin {
        /// The heading.
        title: String,
        /// Starts a new group.
        group: bool,
    },
    /// One line of auspex's own words (never process output).
    Line {
        /// The text; the shell sanitises it.
        text: String,
    },
    /// Closes the run, with this under it. The result is how the machine ended, for the action
    /// log row that was kept open for it (a drained machine has one before `clan` starts).
    End {
        /// The text.
        text: String,
        /// How it ended.
        result: EndResult,
    },
}

/// How a machine of an update of all ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndResult {
    /// Updated, healthy, node given back.
    Done,
    /// Refused before anything was done to it.
    Refused,
    /// Started and failed.
    Failed,
    /// Not started.
    Skipped,
    /// Stopped by the operator.
    Cancelled,
}

impl Operation for LogRequest {
    type Output = ();
}

/// Take the fleet's update lock: one auspex updates a fleet at a time.
///
/// An exclusive `flock` in the state directory, asked for at the `y` of a `u` and of a `U`; the
/// answer is `Ok` or why not (and who holds it). Held until [`FleetUnlockRequest`], or until the
/// process ends.
///
/// A run with no state directory (the demo, most tests) is granted at once: without one the
/// freezes are unknown, so a real run could not update anything anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetLockRequest {
    /// This request; it also names the lock for [`FleetUnlockRequest`].
    pub id: RequestId,
    /// What will be done with it, for the refusal another auspex gets ("update all").
    pub what: String,
}

impl Operation for FleetLockRequest {
    type Output = Result<(), String>;
}

/// Let go of the lock [`FleetLockRequest`] `id` took. Fire and forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetUnlockRequest {
    /// The lock request.
    pub id: RequestId,
}

impl Operation for FleetUnlockRequest {
    type Output = ();
}

/// Read the state of the fleet repository, once, for the plan of an update of all: the tree part
/// of the checks of [`PreflightRequest`], without a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRequest {
    /// This request.
    pub id: RequestId,
    /// The fleet flake, as configured.
    pub flake: String,
    /// `timeouts.diff`.
    pub timeout_seconds: u64,
}

impl Operation for TreeRequest {
    type Output = Result<TreeState, String>;
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
    /// [`ForgeHeadRequest`]: `HEAD` of the fleet checkout, for the CI badge.
    ForgeHead(ForgeHeadRequest),
    /// [`ForgeStatusRequest`]: the forge's combined status of that commit.
    ForgeStatus(ForgeStatusRequest),
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
    /// [`ShellRequest`]: the operator's own ssh session, with the terminal.
    OpenShell(ShellRequest),
    /// [`SaveFreezeRequest`]: keep a freeze or an unfreeze in the state file.
    SaveFreeze(SaveFreezeRequest),
    /// [`LoadFreezesRequest`]: read the state file again, before an update.
    LoadFreezes(LoadFreezesRequest),
    /// [`KubeRequest`]: a call to the cluster, for an update of all.
    Kube(KubeRequest),
    /// [`AwaitRequest`]: wait for an updated machine (and its node) to be healthy.
    Await(AwaitRequest),
    /// [`LogRequest`]: words for the Log tab.
    Log(LogRequest),
    /// [`FleetLockRequest`]: take the update lock of the fleet.
    FleetLock(FleetLockRequest),
    /// [`FleetUnlockRequest`]: let it go.
    FleetUnlock(FleetUnlockRequest),
    /// [`TreeRequest`]: the state of the fleet repository, for the plan.
    Tree(TreeRequest),
}
