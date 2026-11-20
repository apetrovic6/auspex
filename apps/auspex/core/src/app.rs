//! The orchestration: what to ask for, in what order, and what each answer means.
//!
//! # One refresh
//!
//! 1. [`Event::Start`] or [`Event::Refresh`] cancels whatever is still outstanding, starts a
//!    new [`Generation`] and asks for the machine list. **Nothing else.** Probes landing
//!    before the list is one of the plan's known traps.
//! 2. When the list arrives, each machine's target is resolved
//!    ([`auspex_domain::fleet::resolve_target`]) and the core asks, all at once: one probe
//!    per machine whose target parsed, one evaluation of all machines, the Prometheus query
//!    if configured, and address resolution for hostname targets. A refused target gets no
//!    probe and is unreachable with its reason, as the domain decides.
//! 3. Each answer updates its own machine and nothing else. One dead or slow host never
//!    delays or changes another row.
//! 4. Once a machine's evaluated toplevel is known and differs from its current system, and
//!    a binary cache is configured, the core asks whether that path is cached.
//! 5. When the package preview is on ([`Config::diff`]) and the answer is
//!    [`NarCache::Cached`], the core asks for both closures at once: what the machine runs
//!    ([`DeployedClosureRequest`]) and what the update would run ([`PendingClosureRequest`]).
//!    When both are in, the domain's [`diff_closures`] makes the preview and the report
//!    carries it. Any other cache answer, or no cache, makes the preview `not_cached` without
//!    asking anything. A failure of either query makes it `failed` (and cancels the other);
//!    it never changes the machine's status, and never fails the scan.
//!
//! # Settling
//!
//! [`Model::fleet_settled`] is what headless mode waits for. It always becomes true: every
//! request either answers or is cancelled by [`Event::DeadlinePassed`], which also marks
//! whatever is still pending as timed out, except the package previews' queries, which have
//! the later deadline described next.
//!
//! The package preview's closure queries are the exception to "cancelled by the deadline".
//! They are requested late (after a probe, an evaluation and a cache lookup), so the global
//! deadline may be nearly spent when they start; cancelling them there would make a slow
//! preview into a timed-out scan. Each carries its own deadline instead, `timeouts.diff`,
//! which the shell enforces by answering with a failure, so the fleet is settled when the
//! deadline has passed *and* the last preview has answered, at most `timeouts.diff` after it
//! was requested. A refresh or a quit still cancels them like everything else.
//!
//! The core does not rely on the shell for that. A shell that knows `timeouts.diff` also
//! sends [`Event::PreviewDeadlinePassed`] at `timeouts.settle + timeouts.diff`: the core then
//! cancels any preview still outstanding and marks it `failed` ("timed out"), so the fleet
//! settles even if an adapter task never answers. Headless worst case: `settle + diff`.

mod act;

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use auspex_domain::closure::ClosurePaths;
use auspex_domain::diff::{DiffState, PackageDiff, diff_closures};
use auspex_domain::eval::{EvalFailure, EvalOutcome};
use auspex_domain::fleet::{Machine, ResolveError, ResolvedTarget, resolve_target, shared_ips};
use auspex_domain::observe::{Health, ProbeOutcome, PromSample, StorePath, merge_health};
use auspex_domain::report::{FleetError, FleetErrorKind, FleetFacts, fleet_report, machine_report};
use auspex_domain::status::{Badge, MachineFacts, NarCache, Outstanding, machine_status};
use crux_core::render::render;
use crux_core::{App, Command};

use crate::config::Config;
use crate::effects::{
    ActionEnd, CacheRequest, CancelRequest, ClosureAnswer, DeployedClosureRequest, Effect,
    EvalAnswer, EvalMachine, EvalRequest, Generation, InventoryAnswer, InventoryRequest,
    PendingClosureRequest, ProbeRequest, PrometheusAnswer, PrometheusRequest, RequestId,
    ResolveRequest,
};
use crate::view::{Phase, PrometheusStatus, RefreshView, SearchView, ViewModel, ViewState};

/// The reason a probe still out at the global deadline is shown with.
pub const PROBE_TIMED_OUT: &str = "timed out: no answer from the probe before the global deadline";
/// The reason an evaluation still out at the global deadline is shown with.
pub const EVAL_TIMED_OUT: &str = "error: the evaluation did not finish before the global deadline";
/// The reason for a machine the evaluator finished without answering for.
pub const EVAL_MISSING: &str = "error: the evaluator finished without a result for this machine";
/// The Prometheus reason when the deadline passed first.
pub const PROMETHEUS_TIMED_OUT: &str = "no answer before the global deadline";
/// The reason of a `failed` preview whose cache lookup failed.
pub const CACHE_LOOKUP_FAILED: &str =
    "the cache lookup failed, so the pending packages are unknown";
/// The reason of a preview that never got a query slot before the backstop deadline.
pub const PREVIEW_NO_SLOT: &str =
    "no preview slot became free before timeouts.settle + timeouts.diff passed";
/// The reason a package preview still out at the backstop deadline is `failed` with.
pub const PREVIEW_TIMED_OUT: &str =
    "timed out: the package preview did not finish before timeouts.settle + timeouts.diff";
/// The `inventory_timed_out` fleet error's reason.
pub const INVENTORY_TIMED_OUT: &str = "the machine list did not arrive before the global deadline";

/// Everything a shell can tell the core.
///
/// Three groups. The shell sends the first and last directly. The answers in the middle are
/// built by the core itself from a resolved request, stamped with that request's generation
/// and id; a shell never constructs one.
#[derive(Debug, Clone)]
pub enum Event {
    // --- lifecycle --------------------------------------------------------------------
    /// Configure and start the first refresh. Sent again, it replaces the configuration and
    /// refreshes.
    Start(Config),
    /// Start everything again: cancel what is outstanding, re-read the machine list. Ignored
    /// before [`Event::Start`].
    Refresh,
    /// The shell's global deadline for refresh `generation` passed (read the generation from
    /// [`RefreshView::generation`] when the refresh starts). Everything still outstanding is
    /// cancelled and marked as timed out, so the fleet settles, except the package previews'
    /// queries ([`Event::PreviewDeadlinePassed`]). A deadline for an older generation is
    /// ignored.
    DeadlinePassed {
        /// The refresh the deadline was started for.
        generation: Generation,
    },
    /// The backstop for the package previews: `timeouts.settle + timeouts.diff` after refresh
    /// `generation` started. Every preview still outstanding is cancelled and marked `failed`
    /// (timed out), whatever its query was doing, so the fleet settles without depending on
    /// the shell having answered every query. A shell that does not send it (the previews'
    /// own deadline, enforced by the shell, still applies) loses only that guarantee. A
    /// deadline for an older generation is ignored.
    PreviewDeadlinePassed {
        /// The refresh the deadline was started for.
        generation: Generation,
    },
    /// The shell's preview query `id` now holds a preview slot, so its `timeouts.diff` clock
    /// has started. A query that never gets here before [`Event::PreviewDeadlinePassed`] is
    /// failed as having had no slot, not as having timed out. Ignored when stale.
    PreviewStarted {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
    },

    // --- answers (built by the core) ---------------------------------------------------
    /// [`InventoryRequest`] answered.
    InventoryAnswered {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        answer: InventoryAnswer,
    },
    /// [`ProbeRequest`] answered.
    ProbeAnswered {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        outcome: ProbeOutcome,
    },
    /// One item of the [`EvalRequest`] stream.
    EvalAnswered {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        answer: EvalAnswer,
    },
    /// [`PrometheusRequest`] answered.
    PrometheusAnswered {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        answer: PrometheusAnswer,
    },
    /// [`CacheRequest`] answered.
    CacheAnswered {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        cache: NarCache,
    },
    /// [`ResolveRequest`] answered.
    AddressesResolved {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        addresses: BTreeMap<String, Vec<IpAddr>>,
    },

    /// [`DeployedClosureRequest`] answered.
    DeployedClosureAnswered {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        answer: ClosureAnswer,
    },
    /// [`PendingClosureRequest`] answered.
    PendingClosureAnswered {
        /// The request's generation.
        generation: Generation,
        /// The request.
        id: RequestId,
        /// What it answered.
        answer: ClosureAnswer,
    },

    // --- view state (the shell maps keys to these) -------------------------------------
    /// Down `n` rows, clamped (`j`, `5j`). The shell sends 1 when no count was typed.
    MoveDown(usize),
    /// Up `n` rows, clamped (`k`, `5k`).
    MoveUp(usize),
    /// The first row (`gg`).
    MoveTop,
    /// The last row (`G`).
    MoveBottom,
    /// Down half of a page of `n` rows (`Ctrl-d`), at least one row.
    HalfPageDown(usize),
    /// Up half of a page of `n` rows (`Ctrl-u`), at least one row.
    HalfPageUp(usize),
    /// Set the search query (`/`), and jump to the first match at or after the selection.
    /// An empty query clears the search.
    Search(String),
    /// The next match, wrapping (`n`).
    SearchNext,
    /// The previous match, wrapping (`N`).
    SearchPrevious,
    /// Open or close the help overlay (`?`).
    ToggleHelp,
    /// Close the help overlay (`Esc` while it is open).
    CloseHelp,
    /// Toggle act mode (`A`): turning it on asks first, turning it off does not.
    ToggleAct,
    /// Update the selected machine (`u`). Act mode only; asks "update luna to …? y/N" first.
    UpdateSelected,
    /// Build the selected machine's pending system here (`b`). Changes nothing on any machine,
    /// so it needs no act mode; refused for another system's machine.
    BuildSelected,
    /// The answer to the open question: `true` for `y`, `false` for anything else.
    ConfirmAnswer(bool),
    /// The operator wants to quit. Cancels everything outstanding and sets
    /// [`ViewModel::quit`], except while an action is running: then it asks, and quits once
    /// the action has been stopped, killed and reaped. A second quit while it is stopping
    /// forces.
    Quit,
    /// A termination signal, or the shell tearing down: as [`Event::Quit`] without the question.
    Terminate,

    // --- the action's answer (built by the core) ----------------------------------------
    /// [`ActionRequest`](crate::effects::ActionRequest) answered: the process is over.
    ActionEnded {
        /// The request.
        id: RequestId,
        /// How it ended.
        end: ActionEnd,
    },
}

/// What a request id is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Inventory,
    /// The probe for this machine.
    Probe(String),
    /// The evaluation stream; ends with [`EvalAnswer::Finished`].
    Eval,
    /// One machine's evaluation, asked for on its own after an update: a stream of its own, so
    /// its `Finished` can only mark that machine's row.
    EvalOne(String),
    Prometheus,
    /// The cache lookup for this machine.
    Cache(String),
    /// Address resolution for exactly these machines: its answer may update no others, so
    /// an adapter answering for a name it was not asked about can neither invent a
    /// collision nor overwrite an IP-literal target's known address.
    Resolve(BTreeSet<String>),
    /// What this machine runs (the package preview's deployed side).
    Deployed(String),
    /// What this machine's pending toplevel would run (the preview's pending side).
    Update(String),
}

impl Pending {
    /// A package-preview query: bounded by `timeouts.diff`, not by the global deadline.
    const fn is_closure(&self) -> bool {
        matches!(self, Self::Deployed(_) | Self::Update(_))
    }
}

/// Which side of the preview an answer is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Deployed,
    Pending,
}

/// One machine's package preview.
#[derive(Debug, Default)]
enum DiffSlot {
    /// Nothing asked: not an update-pending machine, the preview is off, or the cache
    /// question is still out. [`Model::package_diff`] decides what the report says.
    #[default]
    Idle,
    /// Both closure queries are out or one is back; held until both answer.
    Collecting {
        deployed: Option<ClosurePaths>,
        pending: Option<ClosurePaths>,
    },
    /// Settled: computed, or failed.
    Done(PackageDiff),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Inventory {
    #[default]
    NotStarted,
    Pending,
    Loaded,
    /// Failed or timed out; the reason is in `fleet_errors`.
    Failed,
}

/// One machine of the current generation.
#[derive(Debug)]
struct Row {
    machine: Machine,
    target: Result<ResolvedTarget, ResolveError>,
    probe: Option<ProbeOutcome>,
    eval: Option<EvalOutcome>,
    cache: NarCache,
    diff: DiffSlot,
}

impl Row {
    /// The domain's facts for this row. `prometheus_out` is whether the fleet-wide
    /// Prometheus query that feeds its health is still outstanding.
    const fn facts(&self, prometheus_out: bool) -> MachineFacts<'_> {
        MachineFacts {
            probe: self.probe.as_ref(),
            eval: self.eval.as_ref(),
            cache: self.cache,
            frozen: false,
            alerts_firing: 0,
            outstanding: Outstanding {
                prometheus: prometheus_out,
                // No Alertmanager query until step 3.
                alerts: false,
            },
            target_error: None, // machine_report derives it from `target`
        }
    }

    /// Whether the domain would show a spinner for this row.
    fn spinning(&self) -> bool {
        let error = self.target.as_ref().err().map(|e| e.error.to_string());
        let facts = MachineFacts {
            target_error: error.as_deref(),
            ..self.facts(false)
        };
        machine_status(&facts).badges.iter().any(|b| b.is_spinner())
    }
}

/// The app's whole state. Private fields: a shell sees [`ViewModel`].
#[derive(Debug, Default)]
pub struct Model {
    config: Option<Config>,
    generation: Generation,
    /// The next request id. Never reset, so ids are unique for the life of the core.
    next_id: u64,
    /// Every request issued and neither answered nor cancelled.
    outstanding: BTreeMap<RequestId, Pending>,
    inventory: Inventory,
    /// The current generation's machines, sorted by name.
    rows: Vec<Row>,
    prometheus: PrometheusStatus,
    samples: BTreeMap<String, PromSample>,
    /// Each machine's known addresses: IP-literal targets at once, hostnames once resolved.
    addresses: BTreeMap<String, Vec<IpAddr>>,
    fleet_errors: Vec<FleetError>,
    /// The deadline passed for this generation.
    deadline_passed: bool,
    /// Closure requests whose query holds a preview slot (the shell said so with
    /// [`Event::PreviewStarted`]): the rest are still waiting for one.
    started: BTreeSet<RequestId>,
    /// ... with something still outstanding.
    timed_out: bool,
    view: ViewState,
    /// Act mode. Starts off, is never persisted, and can only be turned on when
    /// [`Config::allow_act`] is set.
    act: bool,
    /// The action in progress, or the last one to end. Not in `outstanding`: a refresh must not
    /// cancel it, and it must not hold the fleet unsettled.
    action: Option<act::Action>,
    /// The question waiting for y or N.
    confirm: Option<act::Confirm>,
    /// Counts the questions asked, so a shell can tell one question from the next (it may only
    /// accept an answer to a question it has drawn).
    confirm_serial: u64,
}

impl Model {
    /// Whether the fleet has settled: what headless mode runs until.
    ///
    /// True once the global deadline has passed for this refresh; otherwise once the machine
    /// list has failed, or once it has loaded, no machine shows a spinner, and no request is
    /// outstanding. The last clause is stricter than "no spinner": it also waits for
    /// Prometheus, the cache question and address resolution, so `--json` never prints a
    /// health value from ssh only because Prometheus was a moment slower, or an empty
    /// shared-IP list only because resolution was.
    #[must_use]
    pub fn fleet_settled(&self) -> bool {
        if self.deadline_passed {
            // The deadline cancelled everything except the package previews, which answer
            // within `timeouts.diff` of being requested.
            return !self.closures_outstanding();
        }
        match self.inventory {
            Inventory::NotStarted | Inventory::Pending => false,
            Inventory::Failed => true,
            Inventory::Loaded => {
                self.outstanding.is_empty() && !self.rows.iter().any(Row::spinning)
            }
        }
    }

    /// A package-preview query is out.
    fn closures_outstanding(&self) -> bool {
        self.outstanding.values().any(Pending::is_closure)
    }

    /// The package preview the report carries for `row`, whose report entry is `report`.
    ///
    /// `None` while it is unknown (the preview is off, the machine is not update pending, the
    /// cache question or the closure queries are still out). A machine whose update is
    /// pending is `not_cached` when the cache said it does not hold the pending closure or
    /// there is no cache to ask, and `failed` ([`CACHE_LOOKUP_FAILED`]) when the lookup itself
    /// failed or the deadline cut it: nobody checked, so nobody may say the path is missing.
    /// Derived here, not stored, so every way the cache question can end says the same.
    fn package_diff(&self, row: &Row, update_pending: bool) -> Option<PackageDiff> {
        self.config.as_ref()?.diff?;
        match &row.diff {
            DiffSlot::Done(diff) => Some(diff.clone()),
            DiffSlot::Idle if !update_pending => None,
            // The lookup answered "no", or there is no cache to ask: the pending closure is
            // known not to be available.
            DiffSlot::Idle
                if matches!(row.cache, NarCache::NotCached | NarCache::NotApplicable) =>
            {
                Some(PackageDiff::not_cached())
            }
            // The lookup itself failed (or the deadline cut it): nobody knows whether the cache
            // has the path, so this is a failure, not "not cached".
            DiffSlot::Idle if row.cache == NarCache::Failed => {
                Some(PackageDiff::failed(CACHE_LOOKUP_FAILED))
            }
            DiffSlot::Collecting { .. } | DiffSlot::Idle => None,
        }
    }

    /// Prometheus is configured but there is nothing to ask it about this refresh.
    fn prometheus_not_asked(&mut self) {
        if self.prometheus == PrometheusStatus::Pending {
            self.prometheus = PrometheusStatus::NotAsked;
        }
    }

    /// Every fleet-wide request is back: the machine list, the evaluator's `Finished`, the
    /// Prometheus query and address resolution. Per-machine answers (probes, cache
    /// lookups) are the rows' own business. After the deadline nothing is outstanding, so
    /// this is true and `timed_out` says why the report is not settled.
    fn scan_complete(&self) -> bool {
        matches!(self.inventory, Inventory::Loaded | Inventory::Failed)
            && !self.outstanding.values().any(|p| {
                matches!(
                    p,
                    Pending::Inventory | Pending::Eval | Pending::Prometheus | Pending::Resolve(_)
                )
            })
    }

    fn names(&self) -> Vec<&str> {
        self.rows.iter().map(|r| r.machine.name.as_str()).collect()
    }

    fn row_mut(&mut self, name: &str) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.machine.name == name)
    }

    fn issue(&mut self, pending: Pending) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        self.outstanding.insert(id, pending);
        id
    }

    /// Takes `id` off the outstanding list if it belongs to the current generation and is
    /// waiting for what `is` accepts. Anything else is a stale or cancelled answer, and the
    /// caller drops it.
    fn accept(
        &mut self,
        generation: Generation,
        id: RequestId,
        is: impl Fn(&Pending) -> bool,
    ) -> Option<Pending> {
        if generation != self.generation || !self.outstanding.get(&id).is_some_and(is) {
            return None;
        }
        self.outstanding.remove(&id)
    }

    /// Cancels every outstanding request, in id order.
    fn cancel_all(&mut self) -> Vec<Command<Effect, Event>> {
        self.cancel_where(|_| true)
    }

    /// Cancels, in id order, every outstanding request `which` accepts, and forgets it.
    fn cancel_where(&mut self, which: impl Fn(&Pending) -> bool) -> Vec<Command<Effect, Event>> {
        let ids: Vec<RequestId> = self
            .outstanding
            .iter()
            .filter(|(_, pending)| which(pending))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .map(|id| {
                self.outstanding.remove(&id);
                Command::notify_shell(CancelRequest { id }).into()
            })
            .collect()
    }

    fn health(&self, row: &Row) -> Health {
        let ssh = match &row.probe {
            Some(ProbeOutcome::Observed(o)) => Some(&o.ssh_health),
            _ => None,
        };
        let prom = self.config.as_ref().and_then(|c| c.prometheus.as_ref());
        let sample = prom.and_then(|p| self.samples.get(p.instance_for(&row.machine.name)));
        merge_health(ssh, sample, prom.map_or(0, |p| p.max_age_seconds))
    }
}

/// The app.
#[derive(Debug, Default)]
pub struct Auspex;

impl App for Auspex {
    type Event = Event;
    type Model = Model;
    type ViewModel = ViewModel;
    type Effect = Effect;

    fn update(&self, event: Event, model: &mut Model) -> Command<Effect, Event> {
        match event {
            Event::Start(config) => {
                model.config = Some(config);
                refresh(model)
            }
            Event::Refresh if model.config.is_some() => refresh(model),
            Event::Refresh => Command::done(),
            Event::DeadlinePassed { generation } => deadline(model, generation),
            Event::PreviewDeadlinePassed { generation } => preview_deadline(model, generation),
            Event::PreviewStarted { generation, id } => preview_started(model, generation, id),
            Event::InventoryAnswered {
                generation,
                id,
                answer,
            } => match model.accept(generation, id, |p| *p == Pending::Inventory) {
                Some(_) => inventory_answered(model, answer),
                None => Command::done(),
            },
            Event::ProbeAnswered {
                generation,
                id,
                outcome,
            } => match model.accept(generation, id, |p| matches!(p, Pending::Probe(_))) {
                Some(Pending::Probe(name)) => probe_answered(model, &name, outcome),
                _ => Command::done(),
            },
            Event::EvalAnswered {
                generation,
                id,
                answer,
            } => eval_answered(model, generation, id, answer),
            Event::PrometheusAnswered {
                generation,
                id,
                answer,
            } => prometheus_answered(model, generation, id, answer),
            Event::CacheAnswered {
                generation,
                id,
                cache,
            } => match model.accept(generation, id, |p| matches!(p, Pending::Cache(_))) {
                Some(Pending::Cache(name)) => {
                    if let Some(row) = model.row_mut(&name) {
                        // `Pending` or `NotApplicable` from the shell is not an answer; read
                        // either as a failed lookup so the row cannot wait forever.
                        row.cache = match cache {
                            NarCache::Pending | NarCache::NotApplicable => NarCache::Failed,
                            answered => answered,
                        };
                    }
                    Command::all([ask_closures(model, &name), render()])
                }
                _ => Command::done(),
            },
            Event::DeployedClosureAnswered {
                generation,
                id,
                answer,
            } => closure_event(model, generation, id, Side::Deployed, answer),
            Event::PendingClosureAnswered {
                generation,
                id,
                answer,
            } => closure_event(model, generation, id, Side::Pending, answer),
            Event::AddressesResolved {
                generation,
                id,
                addresses,
            } => match model.accept(generation, id, |p| matches!(p, Pending::Resolve(_))) {
                Some(Pending::Resolve(asked)) => {
                    for (name, ips) in addresses {
                        if asked.contains(&name) {
                            model.addresses.insert(name, ips);
                        }
                    }
                    render()
                }
                _ => Command::done(),
            },
            event @ (Event::Quit
            | Event::Terminate
            | Event::ToggleAct
            | Event::UpdateSelected
            | Event::BuildSelected
            | Event::ConfirmAnswer(_)
            | Event::ActionEnded { .. }) => act::event(model, event),
            view_event => {
                view_update(model, view_event);
                render()
            }
        }
    }

    fn view(&self, model: &Model) -> ViewModel {
        let shared = shared_ips(&model.addresses);
        let prometheus_out = model
            .outstanding
            .values()
            .any(|p| *p == Pending::Prometheus);
        let machines = model
            .rows
            .iter()
            .map(|r| {
                let mut report = machine_report(
                    &r.machine,
                    &r.target,
                    &r.facts(prometheus_out),
                    model.health(r),
                    &shared,
                );
                // The preview never changes the status, the badges or whether the scan
                // succeeded; it only holds the machine unsettled while its closures are
                // being queried, so `--json` is not printed with the field still to come.
                report.settled &= !matches!(r.diff, DiffSlot::Collecting { .. });
                match model.package_diff(r, report.status == Badge::UpdatePending) {
                    Some(diff) => report.with_package_diff(diff),
                    None => report,
                }
            })
            .collect();
        // The fleet errors and the timed-out flag go into the report, which is what `--json`
        // prints: a failed list must not read as a settled, empty fleet. `report.settled`
        // is the domain's ("the scan completed cleanly"); `refresh.settled` below is
        // "nothing more will change", which is what headless mode waits for.
        let report = fleet_report(
            machines,
            shared,
            FleetFacts {
                scan_complete: model.scan_complete(),
                timed_out: model.timed_out,
                errors: model.fleet_errors.clone(),
            },
        );
        let settled = model.fleet_settled();
        let (mode, action, confirm) = act::views(model);

        let names = model.names();
        let phase = match model.inventory {
            Inventory::NotStarted => Phase::NotStarted,
            _ if settled => Phase::Settled,
            Inventory::Pending => Phase::ListingMachines,
            Inventory::Loaded | Inventory::Failed => Phase::Scanning,
        };
        ViewModel {
            refresh: RefreshView {
                generation: model.generation,
                phase,
                pending_machines: model.rows.iter().filter(|r| r.spinning()).count(),
                diffs_pending: model
                    .rows
                    .iter()
                    .filter(|r| matches!(r.diff, DiffSlot::Collecting { .. }))
                    .count(),
                settled,
                timed_out: model.timed_out,
            },
            report,
            prometheus: model.prometheus.clone(),
            binary_cache: model.config.as_ref().is_some_and(|c| c.binary_cache),
            selected: model.view.index(&names),
            search: (!model.view.query.is_empty()).then(|| SearchView {
                query: model.view.query.clone(),
                matches: model.view.matches(&names),
            }),
            help_open: model.view.help_open,
            quit: model.view.quit,
            mode,
            action,
            confirm,
            notice: model.view.notice.clone(),
        }
    }
}

/// Starts a new generation: cancel, reset, ask for the list and nothing else.
fn refresh(model: &mut Model) -> Command<Effect, Event> {
    let mut commands = model.cancel_all();
    model.generation = Generation(model.generation.0 + 1);
    model.inventory = Inventory::Pending;
    // The rows go: a row from the last generation would show a status nobody is checking.
    // The selection is kept by name and found again when the new list arrives.
    model.rows.clear();
    model.samples.clear();
    model.addresses.clear();
    model.fleet_errors.clear();
    model.deadline_passed = false;
    model.started.clear();
    model.timed_out = false;
    model.prometheus = if model
        .config
        .as_ref()
        .is_some_and(|c| c.prometheus.is_some())
    {
        PrometheusStatus::Pending
    } else {
        PrometheusStatus::NotConfigured
    };

    let generation = model.generation;
    let id = model.issue(Pending::Inventory);
    let flake = model
        .config
        .as_ref()
        .map(|c| c.flake.clone())
        .unwrap_or_default();
    commands.push(
        Command::request_from_shell(InventoryRequest {
            id,
            generation,
            flake,
        })
        .then_send(move |answer| Event::InventoryAnswered {
            generation,
            id,
            answer,
        }),
    );
    commands.push(render());
    Command::all(commands)
}

fn inventory_answered(model: &mut Model, answer: InventoryAnswer) -> Command<Effect, Event> {
    let mut machines = match answer {
        Ok(machines) => machines,
        Err(reason) => {
            model.inventory = Inventory::Failed;
            model
                .fleet_errors
                .push(FleetError::new(FleetErrorKind::InventoryFailed, &reason));
            model.prometheus_not_asked();
            return render();
        }
    };
    model.inventory = Inventory::Loaded;
    // `parse_inventory` already sorts and cannot repeat a name, but the row order and
    // uniqueness are this crate's invariants, so they are not left to the adapter.
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    machines.dedup_by(|a, b| a.name == b.name);

    let config = model.config.clone().unwrap_or_default();
    model.rows = machines
        .into_iter()
        .map(|machine| {
            let target = resolve_target(
                &machine,
                config
                    .target_overrides
                    .get(&machine.name)
                    .map(String::as_str),
            );
            Row {
                machine,
                target,
                probe: None,
                eval: None,
                cache: NarCache::NotApplicable,
                diff: DiffSlot::Idle,
            }
        })
        .collect();
    let names: Vec<&str> = model.rows.iter().map(|r| r.machine.name.as_str()).collect();
    model.view.reanchor(&names);

    let mut commands = ask_probes(model);
    commands.extend(ask_evaluation(model, &config.flake));
    commands.extend(ask_prometheus(model, &config));
    commands.extend(ask_addresses(model));
    commands.push(render());
    Command::all(commands)
}

/// One probe per machine whose target parsed. A refused target is never probed: the domain
/// shows it unreachable with the reason.
fn ask_probes(model: &mut Model) -> Vec<Command<Effect, Event>> {
    let generation = model.generation;
    let probes: Vec<_> = model
        .rows
        .iter()
        .filter_map(|row| {
            let resolved = row.target.as_ref().ok()?;
            Some((
                row.machine.name.clone(),
                row.machine.class,
                resolved.target.clone(),
            ))
        })
        .collect();
    // Collected first: issuing an id borrows the model mutably.
    let mut commands = Vec::with_capacity(probes.len());
    for (machine, class, target) in probes {
        let id = model.issue(Pending::Probe(machine.clone()));
        commands.push(
            Command::request_from_shell(ProbeRequest {
                id,
                generation,
                machine,
                class,
                target,
            })
            .then_send(move |outcome| Event::ProbeAnswered {
                generation,
                id,
                outcome,
            }),
        );
    }
    commands
}

/// One evaluation of every machine, refused targets included: evaluating needs no ssh.
fn ask_evaluation(model: &mut Model, flake: &str) -> Option<Command<Effect, Event>> {
    if model.rows.is_empty() {
        return None;
    }
    let generation = model.generation;
    let machines = model
        .rows
        .iter()
        .map(|r| EvalMachine {
            name: r.machine.name.clone(),
            class: r.machine.class,
        })
        .collect();
    let id = model.issue(Pending::Eval);
    Some(
        Command::stream_from_shell(EvalRequest {
            id,
            generation,
            flake: flake.to_owned(),
            machines,
        })
        .then_send(move |answer| Event::EvalAnswered {
            generation,
            id,
            answer,
        }),
    )
}

/// The fleet-wide Prometheus query, when Prometheus is configured.
fn ask_prometheus(model: &mut Model, config: &Config) -> Option<Command<Effect, Event>> {
    let prom = config.prometheus.as_ref()?;
    if model.rows.is_empty() {
        model.prometheus_not_asked();
        return None;
    }
    let generation = model.generation;
    let instances: BTreeSet<String> = model
        .rows
        .iter()
        .map(|r| prom.instance_for(&r.machine.name).to_owned())
        .collect();
    let id = model.issue(Pending::Prometheus);
    Some(
        Command::request_from_shell(PrometheusRequest {
            id,
            generation,
            instances: instances.into_iter().collect(),
        })
        .then_send(move |answer| Event::PrometheusAnswered {
            generation,
            id,
            answer,
        }),
    )
}

fn prometheus_answered(
    model: &mut Model,
    generation: Generation,
    id: RequestId,
    answer: PrometheusAnswer,
) -> Command<Effect, Event> {
    if model
        .accept(generation, id, |p| *p == Pending::Prometheus)
        .is_none()
    {
        return Command::done();
    }
    match answer {
        Ok(samples) => {
            model.samples = samples;
            model.prometheus = PrometheusStatus::Available;
        }
        Err(reason) => model.prometheus = PrometheusStatus::Unavailable { reason },
    }
    render()
}

/// Records IP-literal targets' addresses, and asks to resolve the hostname ones.
fn ask_addresses(model: &mut Model) -> Option<Command<Effect, Event>> {
    let mut hostnames = BTreeMap::new();
    for row in &model.rows {
        let Ok(resolved) = &row.target else {
            continue;
        };
        match resolved.target.ip() {
            Some(ip) => {
                model.addresses.insert(row.machine.name.clone(), vec![ip]);
            }
            None => {
                hostnames.insert(row.machine.name.clone(), resolved.target.host().to_owned());
            }
        }
    }
    if hostnames.is_empty() {
        return None;
    }
    let generation = model.generation;
    let id = model.issue(Pending::Resolve(hostnames.keys().cloned().collect()));
    Some(
        Command::request_from_shell(ResolveRequest {
            id,
            generation,
            hosts: hostnames,
        })
        .then_send(move |addresses| Event::AddressesResolved {
            generation,
            id,
            addresses,
        }),
    )
}

fn probe_answered(model: &mut Model, name: &str, outcome: ProbeOutcome) -> Command<Effect, Event> {
    if let Some(row) = model.row_mut(name) {
        row.probe = Some(outcome);
    }
    Command::all([ask_cache(model, name), render()])
}

fn eval_answered(
    model: &mut Model,
    generation: Generation,
    id: RequestId,
    answer: EvalAnswer,
) -> Command<Effect, Event> {
    // A stream: per-machine items leave the request outstanding, `Finished` ends it. The
    // fleet-wide stream answers for every machine; the one asked for after an update only for
    // its own, and its `Finished` marks no other row.
    let scope: Option<String> = match model.outstanding.get(&id) {
        Some(Pending::Eval) if generation == model.generation => None,
        Some(Pending::EvalOne(name)) if generation == model.generation => Some(name.clone()),
        _ => return Command::done(),
    };
    match answer {
        EvalAnswer::Machine { machine, outcome } => {
            if scope.as_ref().is_some_and(|s| *s != machine) {
                return Command::done(); // not the machine this stream was asked about
            }
            let Some(row) = model.row_mut(&machine) else {
                return Command::done(); // not a machine we asked about
            };
            if row.eval.is_some() {
                return Command::done(); // the first result for a machine stands
            }
            row.eval = Some(outcome);
            Command::all([ask_cache(model, &machine), render()])
        }
        EvalAnswer::Finished { error } => {
            model.outstanding.remove(&id);
            let reason = error.clone().unwrap_or_else(|| EVAL_MISSING.to_owned());
            for row in model
                .rows
                .iter_mut()
                .filter(|r| r.eval.is_none() && scope.as_ref().is_none_or(|s| *s == r.machine.name))
            {
                row.eval = Some(eval_error(&row.machine.name, &reason));
            }
            if let Some(reason) = error.filter(|_| scope.is_none()) {
                model
                    .fleet_errors
                    .push(FleetError::new(FleetErrorKind::EvaluationFailed, &reason));
            }
            render()
        }
    }
}

/// The cache question, when it applies to `name` now: a cache is configured, the toplevel
/// evaluated, and the machine runs something else.
///
/// Called when the probe or the evaluation lands. Each is accepted at most once per
/// generation (the probe's id leaves the outstanding table; the first eval result stands),
/// so exactly one of those calls sees both, and the question is asked at most once without a
/// flag to say so.
fn ask_cache(model: &mut Model, name: &str) -> Command<Effect, Event> {
    let binary_cache = model.config.as_ref().is_some_and(|c| c.binary_cache);
    let Some(row) = model.rows.iter_mut().find(|r| r.machine.name == name) else {
        return Command::done();
    };
    let (Some(EvalOutcome::Success(eval)), Some(ProbeOutcome::Observed(observed))) =
        (&row.eval, &row.probe)
    else {
        return Command::done();
    };
    let pending = observed
        .current_system
        .as_ref()
        .is_some_and(|current| *current != eval.out_path);
    if !binary_cache || !pending {
        return Command::done();
    }
    let store_path = eval.out_path.clone();
    // Outstanding until it answers: the domain keeps the row unsettled meanwhile.
    row.cache = NarCache::Pending;
    let generation = model.generation;
    let id = model.issue(Pending::Cache(name.to_owned()));
    Command::request_from_shell(CacheRequest {
        id,
        generation,
        machine: name.to_owned(),
        store_path,
    })
    .then_send(move |cache| Event::CacheAnswered {
        generation,
        id,
        cache,
    })
}

/// The package preview's two queries, when the cache has just said the pending toplevel is
/// there: what the machine runs, and what the update would run, both at once.
///
/// Called when the cache answer lands. Only a machine with an update pending was ever asked
/// the cache question, so `Cached` implies one; the probe supplies the target and the
/// evaluation the toplevel. Any other answer asks nothing: the preview is then `not_cached`
/// (see [`Model::package_diff`]), with no ssh and no network.
fn ask_closures(model: &mut Model, name: &str) -> Command<Effect, Event> {
    let Some(diff) = model.config.as_ref().and_then(|c| c.diff) else {
        return Command::done();
    };
    let generation = model.generation;
    let Some(row) = model.row_mut(name) else {
        return Command::done();
    };
    if row.cache != NarCache::Cached || !matches!(row.diff, DiffSlot::Idle) {
        return Command::done();
    }
    let (Ok(resolved), Some(EvalOutcome::Success(eval))) = (&row.target, &row.eval) else {
        return Command::done();
    };
    let target = resolved.target.clone();
    let store_path = eval.out_path.clone();
    row.diff = DiffSlot::Collecting {
        deployed: None,
        pending: None,
    };
    let timeout_seconds = diff.timeout_seconds;
    let deployed_id = model.issue(Pending::Deployed(name.to_owned()));
    let pending_id = model.issue(Pending::Update(name.to_owned()));
    Command::all([
        Command::request_from_shell(DeployedClosureRequest {
            id: deployed_id,
            generation,
            machine: name.to_owned(),
            target,
            timeout_seconds,
        })
        .then_send(move |answer| Event::DeployedClosureAnswered {
            generation,
            id: deployed_id,
            answer,
        }),
        Command::request_from_shell(PendingClosureRequest {
            id: pending_id,
            generation,
            machine: name.to_owned(),
            store_path,
            timeout_seconds,
        })
        .then_send(move |answer| Event::PendingClosureAnswered {
            generation,
            id: pending_id,
            answer,
        }),
    ])
}

/// A closure query's answer, if it is the one the core is waiting for (the right kind, the
/// current generation, not cancelled).
fn closure_event(
    model: &mut Model,
    generation: Generation,
    id: RequestId,
    side: Side,
    answer: ClosureAnswer,
) -> Command<Effect, Event> {
    let asked = |p: &Pending| {
        matches!(
            (side, p),
            (Side::Deployed, Pending::Deployed(_)) | (Side::Pending, Pending::Update(_))
        )
    };
    match model.accept(generation, id, asked) {
        Some(Pending::Deployed(name) | Pending::Update(name)) => {
            closure_answered(model, &name, side, answer)
        }
        _ => Command::done(),
    }
}

/// One side of a machine's preview has answered. The first failure ends the preview as
/// `failed` and cancels the other side (nobody is waiting for it any more); when both
/// sides are in, the domain compares them.
///
/// Nothing here touches the machine's status, badges or `timed_out`: a preview that could not
/// be had is information about the preview.
fn closure_answered(
    model: &mut Model,
    name: &str,
    side: Side,
    answer: ClosureAnswer,
) -> Command<Effect, Event> {
    let Some(row) = model.row_mut(name) else {
        return Command::done();
    };
    let DiffSlot::Collecting { deployed, pending } = &mut row.diff else {
        return Command::done();
    };
    let paths = match answer {
        Ok(paths) => paths,
        Err(reason) => {
            row.diff = DiffSlot::Done(PackageDiff::failed(&reason));
            // The other side's request, if it is still out: its answer would be dropped
            // anyway, but its ssh or nix process should not run on.
            let name = name.to_owned();
            let mut commands = model.cancel_where(|p| match p {
                Pending::Deployed(n) | Pending::Update(n) => *n == name,
                _ => false,
            });
            commands.push(render());
            return Command::all(commands);
        }
    };
    match side {
        Side::Deployed => *deployed = Some(paths),
        Side::Pending => *pending = Some(paths),
    }
    if let (Some(deployed), Some(pending)) = (deployed.as_ref(), pending.as_ref()) {
        let diff = compare(deployed, pending);
        let diff = verified(
            row.probe.as_ref(),
            row.eval.as_ref(),
            diff,
            deployed,
            pending,
        );
        row.diff = DiffSlot::Done(diff);
    }
    render()
}

/// Why a closure's system is not the one the scan saw.
pub const SYSTEM_CHANGED: &str = "the machine changed system while it was being queried";
/// The pending side's equivalent: the cache's closure is not the evaluated toplevel's.
pub const PENDING_SYSTEM_MISMATCH: &str =
    "the pending closure is not the system the evaluation produced";

/// A computed preview is only a preview of *this* update if its two closures are the ones the
/// scan saw: the deployed closure must contain the probe's `current_system` and the pending
/// one the evaluation's out path.
///
/// Membership, not uniqueness of "the" system toplevel: a host closure routinely holds other
/// toplevels (a declarative container, a microvm, a specialisation), which the domain calls
/// ambiguous and leaves out of `deployed`/`pending`. Each closure is the closure *of* the
/// queried path, so it contains that path; a deploy or switch between the probe and the query
/// (seconds apart, an operator deploying with auspex open) breaks that, and would otherwise
/// show the diff of a different system, such as "packages unchanged" on a pending row. A
/// mismatch makes the preview `failed`. On success `deployed` and `pending` record the
/// toplevels that were checked.
fn verified(
    probe: Option<&ProbeOutcome>,
    eval: Option<&EvalOutcome>,
    mut diff: PackageDiff,
    deployed: &ClosurePaths,
    pending: &ClosurePaths,
) -> PackageDiff {
    if diff.state != DiffState::Computed {
        return diff;
    }
    let probed = match probe {
        Some(ProbeOutcome::Observed(o)) => o.current_system.as_ref(),
        _ => None,
    };
    let evaluated = match eval {
        Some(EvalOutcome::Success(s)) => Some(&s.out_path),
        _ => None,
    };
    let holds = |closure: &ClosurePaths, system: Option<&StorePath>| {
        system.is_some_and(|s| closure.paths.contains(s))
    };
    if !holds(deployed, probed) {
        return PackageDiff::failed(SYSTEM_CHANGED);
    }
    if !holds(pending, evaluated) {
        return PackageDiff::failed(PENDING_SYSTEM_MISMATCH);
    }
    diff.deployed = probed.map(ToString::to_string);
    diff.pending = evaluated.map(ToString::to_string);
    diff
}

/// The domain's comparison of two closures, with the lines the adapters dropped added to its
/// own count of paths it could not read.
fn compare(deployed: &ClosurePaths, pending: &ClosurePaths) -> PackageDiff {
    fn strings(closure: &ClosurePaths) -> Vec<&str> {
        closure.paths.iter().map(StorePath::as_str).collect()
    }
    match diff_closures(&strings(deployed), Some(&strings(pending))) {
        Ok(Some(mut diff)) => {
            diff.invalid.deployed += deployed.dropped;
            diff.invalid.pending += pending.dropped;
            diff
        }
        // Not reachable: the pending side is always given. Said rather than unwrapped.
        Ok(None) => PackageDiff::failed("the pending closure was not compared"),
        Err(e) => PackageDiff::failed(&format!("the closures are too large to compare: {e}")),
    }
}

fn eval_error(machine: &str, reason: &str) -> EvalOutcome {
    EvalOutcome::Error(EvalFailure {
        attr: machine.to_owned(),
        attr_path: Vec::new(),
        error_tail: vec![reason.to_owned()],
    })
}

/// A preview query holds a slot ([`Event::PreviewStarted`]); a stale or cancelled one is ignored.
fn preview_started(
    model: &mut Model,
    generation: Generation,
    id: RequestId,
) -> Command<Effect, Event> {
    if generation == model.generation && model.outstanding.contains_key(&id) {
        model.started.insert(id);
    }
    Command::done()
}

/// The previews' backstop deadline ([`Event::PreviewDeadlinePassed`]): whatever previews are
/// still out are cancelled and failed. Nothing else changes: the scan's own deadline is
/// [`deadline`]'s, and a preview never touches `timed_out`.
fn preview_deadline(model: &mut Model, generation: Generation) -> Command<Effect, Event> {
    if generation != model.generation || model.inventory == Inventory::NotStarted {
        return Command::done();
    }
    // Before the cancels forget the requests: which machines never got a slot at all.
    let ran: BTreeSet<String> = model
        .outstanding
        .iter()
        .filter(|(id, _)| model.started.contains(id))
        .filter_map(|(_, p)| match p {
            Pending::Deployed(name) | Pending::Update(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    let mut commands = model.cancel_where(Pending::is_closure);
    for row in &mut model.rows {
        if matches!(row.diff, DiffSlot::Collecting { .. }) {
            let reason = if ran.contains(&row.machine.name) {
                PREVIEW_TIMED_OUT
            } else {
                PREVIEW_NO_SLOT
            };
            row.diff = DiffSlot::Done(PackageDiff::failed(reason));
        }
    }
    commands.push(render());
    Command::all(commands)
}

/// The global deadline: cancel everything outstanding, and mark what each request was
/// waiting for as timed out, so that nothing is left spinning.
fn deadline(model: &mut Model, generation: Generation) -> Command<Effect, Event> {
    if generation != model.generation
        || model.inventory == Inventory::NotStarted
        || model.deadline_passed
    {
        return Command::done();
    }
    model.deadline_passed = true;
    // Everything but the package previews: they have `timeouts.diff` of their own, and a
    // slow preview must not turn a finished scan into a timed-out one.
    let outstanding: BTreeMap<RequestId, Pending> = model
        .outstanding
        .iter()
        .filter(|(_, p)| !p.is_closure())
        .map(|(id, p)| (*id, p.clone()))
        .collect();
    let mut commands = model.cancel_where(|p| !p.is_closure());
    model.timed_out = !outstanding.is_empty();
    for pending in outstanding.into_values() {
        match pending {
            Pending::Inventory => {
                model.inventory = Inventory::Failed;
                model.fleet_errors.push(FleetError::new(
                    FleetErrorKind::InventoryTimedOut,
                    INVENTORY_TIMED_OUT,
                ));
                model.prometheus_not_asked();
            }
            Pending::Prometheus => {
                model.prometheus = PrometheusStatus::Unavailable {
                    reason: PROMETHEUS_TIMED_OUT.to_owned(),
                };
            }
            // A lookup that did not answer in time failed, as far as the badge goes.
            Pending::Cache(name) => {
                if let Some(row) = model.row_mut(&name) {
                    row.cache = NarCache::Failed;
                }
            }
            // An unresolved hostname simply cannot collide. Rows are marked below, whatever
            // was outstanding: after a quit nothing is, and the rows still spin.
            //
            // The previews' queries are not here: filtered out above, they keep their own
            // deadline.
            Pending::Resolve(_)
            | Pending::Probe(_)
            | Pending::Eval
            | Pending::EvalOne(_)
            | Pending::Deployed(_)
            | Pending::Update(_) => {}
        }
    }
    for row in &mut model.rows {
        if row.probe.is_none() && row.target.is_ok() {
            model.timed_out = true;
            row.probe = Some(ProbeOutcome::Unreachable {
                reason: PROBE_TIMED_OUT.to_owned(),
            });
        }
        if row.eval.is_none() {
            model.timed_out = true;
            row.eval = Some(eval_error(&row.machine.name, EVAL_TIMED_OUT));
        }
    }
    commands.push(render());
    Command::all(commands)
}

fn view_update(model: &mut Model, event: Event) {
    model.view.notice = None;
    let names: Vec<String> = model.rows.iter().map(|r| r.machine.name.clone()).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let v = &mut model.view;
    let rows = |n: usize| isize::try_from(n).unwrap_or(isize::MAX);
    match event {
        Event::MoveDown(n) => v.move_by(&names, rows(n)),
        Event::MoveUp(n) => v.move_by(&names, -rows(n)),
        Event::MoveTop => v.top(&names),
        Event::MoveBottom => v.bottom(&names),
        Event::HalfPageDown(page) => v.move_by(&names, rows((page / 2).max(1))),
        Event::HalfPageUp(page) => v.move_by(&names, -rows((page / 2).max(1))),
        Event::Search(query) => v.search(&names, query),
        Event::SearchNext => v.next_match(&names),
        Event::SearchPrevious => v.previous_match(&names),
        Event::ToggleHelp => v.help_open = !v.help_open,
        Event::CloseHelp => v.help_open = false,
        // Handled in `update`; listed so a new event is a compile error here, not a no-op.
        Event::Start(_)
        | Event::Refresh
        | Event::DeadlinePassed { .. }
        | Event::PreviewDeadlinePassed { .. }
        | Event::PreviewStarted { .. }
        | Event::InventoryAnswered { .. }
        | Event::ProbeAnswered { .. }
        | Event::EvalAnswered { .. }
        | Event::PrometheusAnswered { .. }
        | Event::CacheAnswered { .. }
        | Event::DeployedClosureAnswered { .. }
        | Event::PendingClosureAnswered { .. }
        | Event::AddressesResolved { .. }
        | Event::ToggleAct
        | Event::UpdateSelected
        | Event::BuildSelected
        | Event::ConfirmAnswer(_)
        | Event::ActionEnded { .. }
        | Event::Terminate
        | Event::Quit => {}
    }
}
