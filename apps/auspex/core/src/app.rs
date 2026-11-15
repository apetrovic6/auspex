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
//!
//! # Settling
//!
//! [`Model::fleet_settled`] is what headless mode waits for. It always becomes true: every
//! request either answers or is cancelled by [`Event::DeadlinePassed`], which also marks
//! whatever is still pending as timed out.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use auspex_domain::eval::{EvalFailure, EvalOutcome};
use auspex_domain::fleet::{Machine, ResolveError, ResolvedTarget, resolve_target, shared_ips};
use auspex_domain::observe::{Health, ProbeOutcome, PromSample, merge_health};
use auspex_domain::report::{FleetFacts, fleet_report, machine_report};
use auspex_domain::status::{MachineFacts, NarCache, Outstanding, machine_status};
use crux_core::render::render;
use crux_core::{App, Command};

use crate::config::Config;
use crate::effects::{
    CacheRequest, CancelRequest, Effect, EvalAnswer, EvalMachine, EvalRequest, Generation,
    InventoryAnswer, InventoryRequest, ProbeRequest, PrometheusAnswer, PrometheusRequest,
    RequestId, ResolveRequest,
};
use crate::view::{
    FleetError, Mode, Phase, PrometheusStatus, RefreshView, SearchView, ViewModel, ViewState,
};

/// The reason a probe still out at the global deadline is shown with.
pub const PROBE_TIMED_OUT: &str = "timed out: no answer from the probe before the global deadline";
/// The reason an evaluation still out at the global deadline is shown with.
pub const EVAL_TIMED_OUT: &str = "error: the evaluation did not finish before the global deadline";
/// The reason for a machine the evaluator finished without answering for.
pub const EVAL_MISSING: &str = "error: the evaluator finished without a result for this machine";
/// The Prometheus reason when the deadline passed first.
pub const PROMETHEUS_TIMED_OUT: &str = "no answer before the global deadline";

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
    /// cancelled and marked as timed out, so the fleet settles. A deadline for an older
    /// generation is ignored.
    DeadlinePassed {
        /// The refresh the deadline was started for.
        generation: Generation,
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
    /// Quit: cancel everything outstanding and set [`ViewModel::quit`].
    Quit,
}

/// What a request id is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Inventory,
    /// The probe for this machine.
    Probe(String),
    /// The evaluation stream; ends with [`EvalAnswer::Finished`].
    Eval,
    Prometheus,
    /// The cache lookup for this machine.
    Cache(String),
    Resolve,
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
    /// The cache question has been asked this generation; it is asked at most once.
    cache_asked: bool,
}

impl Row {
    const fn facts(&self) -> MachineFacts<'_> {
        MachineFacts {
            probe: self.probe.as_ref(),
            eval: self.eval.as_ref(),
            cache: self.cache,
            frozen: false,
            alerts_firing: 0,
            outstanding: Outstanding {
                prometheus: false,
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
            ..self.facts()
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
    /// ... with something still outstanding.
    timed_out: bool,
    view: ViewState,
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
            return true;
        }
        match self.inventory {
            Inventory::NotStarted | Inventory::Pending => false,
            Inventory::Failed => true,
            Inventory::Loaded => {
                self.outstanding.is_empty() && !self.rows.iter().any(Row::spinning)
            }
        }
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
        std::mem::take(&mut self.outstanding)
            .into_keys()
            .map(|id| Command::notify_shell(CancelRequest { id }).into())
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
            } => match model.accept(generation, id, |p| *p == Pending::Prometheus) {
                Some(_) => {
                    match answer {
                        Ok(samples) => {
                            model.samples = samples;
                            model.prometheus = PrometheusStatus::Available;
                        }
                        Err(reason) => model.prometheus = PrometheusStatus::Unavailable { reason },
                    }
                    render()
                }
                None => Command::done(),
            },
            Event::CacheAnswered {
                generation,
                id,
                cache,
            } => match model.accept(generation, id, |p| matches!(p, Pending::Cache(_))) {
                Some(Pending::Cache(name)) => {
                    if let Some(row) = model.row_mut(&name) {
                        row.cache = cache;
                    }
                    render()
                }
                _ => Command::done(),
            },
            Event::AddressesResolved {
                generation,
                id,
                addresses,
            } => match model.accept(generation, id, |p| *p == Pending::Resolve) {
                Some(_) => {
                    for (name, ips) in addresses {
                        // Only machines of this generation: an adapter answering for a name it
                        // was not asked about must not create a collision with a ghost.
                        if model.rows.iter().any(|r| r.machine.name == name) {
                            model.addresses.insert(name, ips);
                        }
                    }
                    render()
                }
                None => Command::done(),
            },
            Event::Quit => {
                model.view.quit = true;
                let mut commands = model.cancel_all();
                commands.push(render());
                Command::all(commands)
            }
            view_event => {
                view_update(model, view_event);
                render()
            }
        }
    }

    fn view(&self, model: &Model) -> ViewModel {
        let shared = shared_ips(&model.addresses);
        let machines = model
            .rows
            .iter()
            .map(|r| machine_report(&r.machine, &r.target, &r.facts(), model.health(r), &shared))
            .collect();
        let mut report = fleet_report(machines, shared, FleetFacts::default());
        let settled = model.fleet_settled();
        // `fleet_report` decides `settled` from badges alone, and after the deadline has
        // marked everything as timed out there are no spinners left. The contract says a run
        // the deadline cut short is `false`, and only the core knows that it was.
        report.settled = settled && !model.timed_out;

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
                settled,
                timed_out: model.timed_out,
            },
            report,
            fleet_errors: model.fleet_errors.clone(),
            prometheus: model.prometheus.clone(),
            selected: model.view.index(&names),
            search: (!model.view.query.is_empty()).then(|| SearchView {
                query: model.view.query.clone(),
                matches: model.view.matches(&names),
            }),
            help_open: model.view.help_open,
            quit: model.view.quit,
            mode: Mode::Observe,
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
    model.timed_out = false;
    model.prometheus = PrometheusStatus::NotConfigured;

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
                .push(FleetError::InventoryFailed { reason });
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
                cache_asked: false,
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
        return None;
    }
    let generation = model.generation;
    let instances: BTreeSet<String> = model
        .rows
        .iter()
        .map(|r| prom.instance_for(&r.machine.name).to_owned())
        .collect();
    model.prometheus = PrometheusStatus::Pending;
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
    let id = model.issue(Pending::Resolve);
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
    // A stream: per-machine items leave the request outstanding, `Finished` ends it.
    if generation != model.generation || model.outstanding.get(&id) != Some(&Pending::Eval) {
        return Command::done();
    }
    match answer {
        EvalAnswer::Machine { machine, outcome } => {
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
            for row in model.rows.iter_mut().filter(|r| r.eval.is_none()) {
                row.eval = Some(eval_error(&row.machine.name, &reason));
            }
            if let Some(reason) = error {
                model
                    .fleet_errors
                    .push(FleetError::EvaluationFailed { reason });
            }
            render()
        }
    }
}

/// The cache question, when it applies to `name` now: a cache is configured, it has not been
/// asked this generation, the toplevel evaluated, and the machine runs something else.
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
    if !binary_cache || row.cache_asked || !pending {
        return Command::done();
    }
    row.cache_asked = true;
    let store_path = eval.out_path.clone();
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

fn eval_error(machine: &str, reason: &str) -> EvalOutcome {
    EvalOutcome::Error(EvalFailure {
        attr: machine.to_owned(),
        attr_path: Vec::new(),
        error_tail: vec![reason.to_owned()],
    })
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
    let outstanding = model.outstanding.clone();
    let mut commands = model.cancel_all();
    model.timed_out = !outstanding.is_empty();
    for pending in outstanding.into_values() {
        match pending {
            Pending::Inventory => {
                model.inventory = Inventory::Failed;
                model.fleet_errors.push(FleetError::InventoryTimedOut);
            }
            Pending::Prometheus => {
                model.prometheus = PrometheusStatus::Unavailable {
                    reason: PROMETHEUS_TIMED_OUT.to_owned(),
                };
            }
            // A cache question left unanswered stays `NotApplicable`, as before the rename. An
            // unresolved hostname simply cannot collide. Rows are marked below, whatever was
            // outstanding: after a quit nothing is, and the rows still spin.
            Pending::Cache(_) | Pending::Resolve | Pending::Probe(_) | Pending::Eval => {}
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
        | Event::InventoryAnswered { .. }
        | Event::ProbeAnswered { .. }
        | Event::EvalAnswered { .. }
        | Event::PrometheusAnswered { .. }
        | Event::CacheAnswered { .. }
        | Event::AddressesResolved { .. }
        | Event::Quit => {}
    }
}
