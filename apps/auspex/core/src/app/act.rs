//! Act mode, the two actions (`u` update, `b` build), the questions asked before them, and
//! quitting while one runs.
//!
//! # What the core guarantees
//!
//! - **Observe is the default and nothing persists it.** [`Model::act`] starts `false`, is
//!   never read from a config, and can only become `true` when [`Config::allow_act`] is set:
//!   headless runs do not set it, so they cannot be put in act mode.
//! - **An update is asked about, by name.** `u` opens "update luna to nixos-system-luna-…?
//!   y/N". Nothing runs until the operator answers `y`, and the request is re-checked at that
//!   moment (the list may have been refreshed while the question was open).
//! - **One machine, validated.** The machine of an [`ActionRequest`] is the selected row's name,
//!   checked against the domain's machine-name rule and against the inventory (a row exists).
//!   An empty, unknown or malformed name is refused here, before any effect is asked for. The
//!   adapter checks the rule again: `clan machines update` with no name updates the whole
//!   fleet, so neither layer trusts the other.
//! - **One action at a time.** A second `u` or `b` while one runs says so and asks for nothing.
//! - **A build is local and same-system.** `b` needs no act mode (it changes nothing on any
//!   machine) but is refused for a machine whose evaluated system is not this machine's.
//! - **Quitting waits for the process.** With an action running, `q` asks; `y` cancels it
//!   through [`Effect::Cancel`](crate::Effect::Cancel), and [`ViewModel::quit`] only becomes
//!   true when the shell has answered that the process group is killed and reaped. A second
//!   quit while it is stopping forces.
//!
//! # After an update
//!
//! The machine is refreshed on its own (probe and evaluation, [`refresh_machine`]), whatever
//! the outcome: a failed deploy may have switched anyway, and the row should say what is there.
//!
//! [`ViewModel::quit`]: crate::ViewModel::quit

use auspex_domain::eval::EvalOutcome;
use auspex_domain::fleet::is_plain_name;
use auspex_domain::observe::ProbeOutcome;
use auspex_domain::status::NarCache;
use crux_core::Command;
use crux_core::render::render;

use super::{Event, Model, Pending};
use crate::effects::{
    ActionEnd, ActionKind, ActionRequest, CancelRequest, Effect, EvalMachine, EvalRequest,
    ProbeRequest, RequestId,
};
use crate::view::{ActionStatus, ActionView, ConfirmKind, ConfirmView, Mode};

/// The notice of an action key pressed in observe mode.
pub const ACT_MODE_OFF: &str = "act mode is off (A)";

/// The notice of a `y` while the machine's evaluation is being refreshed.
pub const REFRESH_RUNNING: &str = "a refresh is running; ask again when it has finished";

/// A question waiting for y or N.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Confirm {
    /// Turn act mode on.
    Act,
    /// Update `machine` to the pending system `to` (what the question shows: hash prefix and
    /// name), whose store path was `path` when it was asked.
    Update {
        machine: String,
        to: String,
        path: String,
    },
    /// Quit while an action runs.
    Quit,
}

/// Where the current (or last) action is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Running,
    Stopping,
    Ended(ActionEnd),
}

/// The action in progress, or the last one to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Action {
    id: RequestId,
    kind: ActionKind,
    machine: String,
    phase: Phase,
    /// The operator chose to quit: when the shell answers that the process is over, quit.
    quit_when_ended: bool,
}

impl Action {
    const fn active(&self) -> bool {
        matches!(self.phase, Phase::Running | Phase::Stopping)
    }
}

impl Model {
    fn action_active(&self) -> bool {
        self.action.as_ref().is_some_and(Action::active)
    }

    const fn fresh_id(&mut self) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Whether the list, the fleet's evaluation or `machine`'s own evaluation is out.
    fn refresh_running(&self, machine: &str) -> bool {
        self.outstanding.values().any(|p| {
            matches!(p, Pending::Inventory | Pending::Eval)
                || matches!(p, Pending::EvalOne(n) if n == machine)
        })
    }

    /// Opens `confirm` as a new question: a shell must draw it before it may be answered.
    fn ask(&mut self, confirm: Confirm) {
        self.confirm_serial += 1;
        self.confirm = Some(confirm);
    }

    fn notice(&mut self, text: impl Into<String>) -> Command<Effect, Event> {
        self.view.notice = Some(text.into());
        render()
    }
}

/// What the view shows of act mode, the action and the open question.
pub(super) fn views(model: &Model) -> (Mode, Option<ActionView>, Option<ConfirmView>) {
    let mode = if model.act { Mode::Act } else { Mode::Observe };
    let action = model.action.as_ref().map(|a| ActionView {
        kind: a.kind,
        machine: a.machine.clone(),
        status: match &a.phase {
            Phase::Running => ActionStatus::Running,
            Phase::Stopping => ActionStatus::Stopping,
            Phase::Ended(end) => ActionStatus::Ended(end.clone()),
        },
    });
    let confirm = model.confirm.as_ref().map(|c| ConfirmView {
        kind: match c {
            Confirm::Act => ConfirmKind::Act,
            Confirm::Update { .. } => ConfirmKind::Update,
            Confirm::Quit => ConfirmKind::Quit,
        },
        question: question(model, c),
        serial: model.confirm_serial,
    });
    (mode, action, confirm)
}

fn question(model: &Model, confirm: &Confirm) -> String {
    match confirm {
        Confirm::Act => "turn on act mode? updates will be possible. y/N".to_owned(),
        Confirm::Update { machine, to, .. } => format!("update {machine} to {to}? y/N"),
        Confirm::Quit => {
            let what = model.action.as_ref().map_or_else(
                || "an action".to_owned(),
                |a| {
                    let article = if a.kind == ActionKind::Update {
                        "an"
                    } else {
                        "a"
                    };
                    format!("{article} {} of {}", a.kind.label(), a.machine)
                },
            );
            // Only an update has a remote side: killing clan's group ends the local ssh, but an
            // activation already running on the machine can finish without it.
            let remote = match model.action.as_ref() {
                Some(a) if a.kind == ActionKind::Update => {
                    " (an activation already under way on the machine may complete)"
                }
                _ => "",
            };
            format!("{what} is running; quit and stop it?{remote} y/N")
        }
    }
}

/// `A`: toggles act mode. Turning it off is immediate; turning it on asks.
pub(super) fn toggle_act(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    if !model.config.as_ref().is_some_and(|c| c.allow_act) {
        return model.notice("act mode is not available in this run");
    }
    if model.act {
        model.act = false;
        model.confirm = None;
        return render();
    }
    model.ask(Confirm::Act);
    render()
}

/// `u` or `b` on the selected machine.
pub(super) fn request(model: &mut Model, kind: ActionKind) -> Command<Effect, Event> {
    model.view.notice = None;
    if kind == ActionKind::Update && !model.act {
        return model.notice(ACT_MODE_OFF);
    }
    if let Some(a) = model.action.as_ref().filter(|a| a.active()) {
        let text = format!("{} of {} is already running", article(a.kind), a.machine);
        return model.notice(text);
    }
    let names = model.names();
    let Some(machine) = model
        .view
        .index(&names)
        .and_then(|i| names.get(i))
        .map(|n| (*n).to_owned())
    else {
        return model.notice("no machine selected");
    };
    match check(model, kind, &machine) {
        Err(why) => model.notice(why),
        Ok(Shown { shown, path }) if kind == ActionKind::Update => {
            model.ask(Confirm::Update {
                machine,
                to: shown,
                path,
            });
            render()
        }
        Ok(_) => launch(model, kind, machine),
    }
}

fn article(kind: ActionKind) -> String {
    let a = if kind == ActionKind::Update {
        "an"
    } else {
        "a"
    };
    format!("{a} {}", kind.label())
}

/// The system a machine's evaluation produced: what an update would deploy.
struct Shown {
    /// As the question shows it, as the detail panel does: the hash's first 8 characters and
    /// the name (the name alone does not identify a system: two configurations on one nixpkgs
    /// share it).
    shown: String,
    /// The whole store path, to tell at `y` whether it is still the one that was asked about.
    path: String,
}

/// Everything that must hold before an action on `machine` may be asked for. On success, the
/// pending system the machine's evaluation produced.
fn check(model: &Model, kind: ActionKind, machine: &str) -> Result<Shown, String> {
    if !is_plain_name(machine) {
        return Err(format!(
            "{machine:?} is not a plain machine name; not touching it"
        ));
    }
    let Some(row) = model.rows.iter().find(|r| r.machine.name == machine) else {
        return Err(format!(
            "{machine} is not in the inventory; not touching it"
        ));
    };
    let Some(EvalOutcome::Success(eval)) = &row.eval else {
        return Err(format!(
            "{machine} has no evaluated system (still evaluating, or it failed): nothing to {}",
            kind.label()
        ));
    };
    if kind == ActionKind::Build {
        let local = model
            .config
            .as_ref()
            .map_or("", |c| c.local_system.as_str());
        match eval.system.as_deref() {
            Some(s) if !local.is_empty() && s == local => {}
            Some(s) => {
                return Err(format!(
                    "{machine} is {s} and this machine is {}: not building another system's \
                     derivations",
                    if local.is_empty() { "unknown" } else { local }
                ));
            }
            None => {
                return Err(format!(
                    "the system of {machine} is not known: not building what cannot be \
                     checked against this machine"
                ));
            }
        }
    }
    Ok(Shown {
        shown: format!("{} {}", &eval.out_path.hash()[..8], eval.out_path.name()),
        path: eval.out_path.as_str().to_owned(),
    })
}

/// Issues the request. Called only after [`check`] and, for an update, the operator's `y`.
fn launch(model: &mut Model, kind: ActionKind, machine: String) -> Command<Effect, Event> {
    let (class, system) = {
        let Some(row) = model.rows.iter().find(|r| r.machine.name == machine) else {
            return model.notice(format!(
                "{machine} is not in the inventory; not touching it"
            ));
        };
        let system = match &row.eval {
            Some(EvalOutcome::Success(e)) => e.system.clone(),
            _ => None,
        };
        (row.machine.class, system)
    };
    let flake = model
        .config
        .as_ref()
        .map(|c| c.flake.clone())
        .unwrap_or_default();
    let id = model.fresh_id();
    model.action = Some(Action {
        id,
        kind,
        machine: machine.clone(),
        phase: Phase::Running,
        quit_when_ended: false,
    });
    Command::all([
        Command::request_from_shell(ActionRequest {
            id,
            kind,
            machine,
            class,
            system,
            flake,
        })
        .then_send(move |end| Event::ActionEnded { id, end }),
        render(),
    ])
}

/// `y` or anything else, to the open question.
pub(super) fn answer(model: &mut Model, yes: bool) -> Command<Effect, Event> {
    model.view.notice = None;
    let Some(confirm) = model.confirm.take() else {
        return Command::done();
    };
    if !yes {
        return render();
    }
    match confirm {
        Confirm::Act => {
            // Re-checked: the question may have been answered after nothing could ask it.
            model.act = model.config.as_ref().is_some_and(|c| c.allow_act);
            render()
        }
        Confirm::Update { machine, path, .. } => {
            // The world may have moved while the question was open: act mode turned off, an
            // action started, the list refreshed. Everything is checked again.
            if !model.act {
                return model.notice(ACT_MODE_OFF);
            }
            if model.action_active() {
                return model.notice("another action is already running");
            }
            // The tree is being read again (the list, the fleet's evaluation or this machine's
            // own): what would be deployed is not settled, and the evaluation the question
            // showed may be the one being replaced. The operator asks again once it is.
            if model.refresh_running(&machine) {
                return model.notice(REFRESH_RUNNING);
            }
            match check(model, ActionKind::Update, &machine) {
                Ok(now) if now.path == path => launch(model, ActionKind::Update, machine),
                // The pending system is not the one the operator was shown (a refresh
                // evaluated the tree again): what they said yes to is not what would be
                // deployed, so ask again about the new one.
                Ok(now) => {
                    let note = format!("the pending system of {machine} changed; asking again");
                    model.ask(Confirm::Update {
                        machine,
                        to: now.shown,
                        path: now.path,
                    });
                    model.view.notice = Some(note);
                    render()
                }
                Err(why) => model.notice(why),
            }
        }
        Confirm::Quit => stop_and_quit(model),
    }
}

/// The operator's request to quit (`q`, Esc, Ctrl-C, `:q`).
pub(super) fn quit(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    match model.action.as_ref().map(|a| &a.phase) {
        Some(Phase::Running) => {
            if model.confirm != Some(Confirm::Quit) {
                model.ask(Confirm::Quit);
            }
            render()
        }
        // The second quit: the process is not dying, the operator does not want to wait.
        Some(Phase::Stopping) => force_quit(model),
        _ => finish_quit(model),
    }
}

/// A termination signal: stop what runs and quit, without a question (nobody can answer one).
pub(super) fn terminate(model: &mut Model) -> Command<Effect, Event> {
    match model.action.as_ref().map(|a| &a.phase) {
        Some(Phase::Running) => stop_and_quit(model),
        Some(Phase::Stopping) => force_quit(model),
        _ => finish_quit(model),
    }
}

/// Nothing is running: cancel the rest and quit.
fn finish_quit(model: &mut Model) -> Command<Effect, Event> {
    model.confirm = None;
    model.view.quit = true;
    let mut commands = model.cancel_all();
    commands.push(render());
    Command::all(commands)
}

/// An action is running: cancel everything, and quit when the action has been answered.
fn stop_and_quit(model: &mut Model) -> Command<Effect, Event> {
    model.confirm = None;
    let mut commands = model.cancel_all();
    if let Some(a) = model.action.as_mut().filter(|a| a.active()) {
        a.phase = Phase::Stopping;
        a.quit_when_ended = true;
        commands.push(Command::notify_shell(CancelRequest { id: a.id }).into());
    } else {
        model.view.quit = true;
    }
    commands.push(render());
    Command::all(commands)
}

/// Quit now, whatever the action is doing. Reached only by a second quit while it is being
/// stopped, so the cancel has been asked for already; the shell still waits for the adapter
/// when it shuts down.
fn force_quit(model: &mut Model) -> Command<Effect, Event> {
    model.confirm = None;
    model.view.quit = true;
    let mut commands = model.cancel_all();
    commands.push(render());
    Command::all(commands)
}

/// The shell's answer: the process is over (killed and reaped, if it was cancelled).
pub(super) fn ended(model: &mut Model, id: RequestId, end: ActionEnd) -> Command<Effect, Event> {
    let Some(action) = model.action.as_mut().filter(|a| a.id == id && a.active()) else {
        return Command::done();
    };
    let (kind, machine, quitting) = (action.kind, action.machine.clone(), action.quit_when_ended);
    action.phase = Phase::Ended(end);
    if quitting {
        model.view.quit = true;
        return render();
    }
    let mut commands = Vec::new();
    if kind == ActionKind::Update {
        commands.extend(refresh_machine(model, &machine));
    }
    commands.push(render());
    Command::all(commands)
}

/// Probes and evaluates `name` again, on its own, in the current generation: what an update
/// changed. The rest of the fleet is left as it is.
///
/// The evaluation is skipped while the fleet-wide one is still out: it will deliver this
/// machine's result, and a second evaluator would only race it.
pub(super) fn refresh_machine(model: &mut Model, name: &str) -> Vec<Command<Effect, Event>> {
    if model.inventory != super::Inventory::Loaded {
        return Vec::new();
    }
    let mut commands = model.cancel_where(|p| match p {
        Pending::Probe(n)
        | Pending::Cache(n)
        | Pending::Deployed(n)
        | Pending::Update(n)
        | Pending::EvalOne(n) => n == name,
        _ => false,
    });
    let fleet_eval_out = model.outstanding.values().any(|p| *p == Pending::Eval);
    let Some(row) = model.row_mut(name) else {
        return commands;
    };
    row.probe = None;
    row.cache = NarCache::NotApplicable;
    row.diff = super::DiffSlot::Idle;
    if !fleet_eval_out {
        row.eval = None;
    }
    let probe = row
        .target
        .as_ref()
        .ok()
        .map(|t| (row.machine.class, t.target.clone()));
    let class = row.machine.class;
    let generation = model.generation;
    if let Some((class, target)) = probe {
        let id = model.issue(Pending::Probe(name.to_owned()));
        commands.push(
            Command::request_from_shell(ProbeRequest {
                id,
                generation,
                machine: name.to_owned(),
                class,
                target,
            })
            .then_send(move |outcome: ProbeOutcome| Event::ProbeAnswered {
                generation,
                id,
                outcome,
            }),
        );
    }
    if !fleet_eval_out {
        let flake = model
            .config
            .as_ref()
            .map(|c| c.flake.clone())
            .unwrap_or_default();
        let id = model.issue(Pending::EvalOne(name.to_owned()));
        commands.push(
            Command::stream_from_shell(EvalRequest {
                id,
                generation,
                flake,
                machines: vec![EvalMachine {
                    name: name.to_owned(),
                    class,
                }],
            })
            .then_send(move |answer| Event::EvalAnswered {
                generation,
                id,
                answer,
            }),
        );
    }
    commands
}

/// The events that are the operator's or the shell's word about acting and quitting, routed
/// here from [`super::Auspex::update`].
pub(super) fn event(model: &mut Model, event: Event) -> Command<Effect, Event> {
    match event {
        Event::Quit => quit(model),
        Event::Terminate => terminate(model),
        Event::ToggleAct => toggle_act(model),
        Event::UpdateSelected => request(model, ActionKind::Update),
        Event::BuildSelected => request(model, ActionKind::Build),
        Event::ConfirmAnswer(yes) => answer(model, yes),
        Event::ActionEnded { id, end } => ended(model, id, end),
        // Not an acting event: `update` never routes one here.
        _ => Command::done(),
    }
}
