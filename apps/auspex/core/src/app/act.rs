//! Act mode, the two actions (`u` update, `b` build), the questions asked before them, and
//! quitting while one runs. Also `s`, the operator's own ssh session ([`open_shell`]), which is
//! not an action: it has no process auspex runs, no output and no log row, only the terminal
//! lent to ssh until it answers.
//!
//! # What the core guarantees
//!
//! - **Observe is the default and nothing persists it.** [`Model::act`] starts `false`, is
//!   never read from a config, and can only become `true` when [`Config::allow_act`] is set:
//!   headless runs do not set it, so they cannot be put in act mode.
//! - **An update is checked, then asked about, by name.** `u` first asks the shell for what
//!   the checks need (the fleet repository's state, the pending closure's size:
//!   [`PreflightRequest`]), then shows **one dialog** with everything the checks found:
//!   refusals first (no way to proceed: any key closes it), then warnings, then the question
//!   ("update luna to 0a1b2c3d nixos-system-luna-…? y/N", or for a machine marked public,
//!   "type luna"). Nothing runs until the operator confirms, and everything is re-checked at
//!   that moment (the list may have been refreshed while the dialog was open). If the refresh
//!   changed the pending system, nothing is deployed and the checks run again for the new one.
//! - **A question is answered only after it was drawn.** Every question the core opens
//!   ([`Model::ask`]) has a new serial, and a shell accepts an answer only to the one it drew.
//!   The checking dialog, the refusal or warning dialog that follows it and the typed
//!   confirmation are each a question of their own: keys typed while the checks ran count for
//!   none of them.
//! - **The rules are the domain's** ([`auspex_domain::preflight`]); this file only gathers
//!   their inputs from what the core holds (the probe's free space on `/nix/store`, the
//!   machines' addresses, the config's `public` flag) and from the shell's answer.
//! - **One machine, validated.** The machine of an [`ActionRequest`] is the selected row's name,
//!   checked against the domain's machine-name rule and against the inventory (a row exists).
//!   An empty, unknown or malformed name is refused here, before any effect is asked for. The
//!   adapter checks the rule again: `clan machines update` with no name updates the whole
//!   fleet, so neither layer trusts the other.
//! - **One action at a time.** A second `u` or `b` while one runs says so and asks for nothing.
//! - **A build is local and same-system.** `b` needs no act mode (it changes nothing on any
//!   machine) but is refused for a machine whose evaluated system is not this machine's. It
//!   runs none of the checks above: they are about what an update does to a machine.
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
use auspex_domain::fleet::{is_plain_name, shared_ips};
use auspex_domain::observe::{ProbeOutcome, StorePath};
use auspex_domain::preflight::{Addresses, Facts, Preflight, estimate, preflight};
use auspex_domain::status::NarCache;
use crux_core::Command;
use crux_core::render::render;

use super::roll::{self, Draft, Planned};
use super::{Event, Model, Pending};
use crate::effects::{
    ActionEnd, ActionKind, ActionRequest, CancelRequest, Effect, EvalMachine, EvalRequest,
    FleetUnlockRequest, PreflightAnswer, PreflightRequest, ProbeRequest, RequestId, ShellEnd,
    ShellRequest,
};
use crate::view::{ActionStatus, ActionView, ConfirmKind, ConfirmView, Mode, TypedView};

/// The notice of an action key pressed in observe mode.
pub const ACT_MODE_OFF: &str = "act mode is off (A)";

/// The notice of a `y` while the machine's evaluation is being refreshed.
pub const REFRESH_RUNNING: &str = "a refresh is running; ask again when it has finished";

/// The longest a typed confirmation can be: a machine name's own limit
/// ([`auspex_domain::fleet::is_plain_name`]).
const MAX_TYPED: usize = 63;

/// A question waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Confirm {
    /// Turn act mode on.
    Act,
    /// The checks before an update of `machine` (to the system named `to`) are running in the
    /// shell as request `id`. Any key cancels them.
    Checking {
        machine: String,
        to: String,
        /// The pending system's store path when the checks were asked for.
        path: String,
        id: RequestId,
    },
    /// The checks are in. `review` is what they decided when they came in (shown); it is
    /// decided again when the operator confirms.
    Update {
        machine: String,
        /// The pending system as the question shows it (hash prefix and name), and its store
        /// path when it was asked about: the answer is for that system, and if a refresh changes
        /// it the question is asked again.
        to: String,
        path: String,
        answer: PreflightAnswer,
        review: Preflight,
        /// What has been typed, when the machine is one whose name has to be typed. Only for
        /// a review with no refusals: there is nothing to confirm otherwise.
        typed: Option<Typed>,
    },
    /// Unfreeze `machine` (`f` on a machine frozen with `f`).
    Unfreeze { machine: String },
    /// The cluster is being read (request `id`) before an update of all is planned. Any key
    /// cancels it.
    CheckingCluster {
        id: RequestId,
        draft: Box<Draft>,
        stage: roll::Stage,
    },
    /// Update everything the plan lists (`U`): one question for the whole roll.
    UpdateAll(Box<Planned>),
    /// Quit while an action runs.
    Quit,
}

/// A typed confirmation in progress.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Typed {
    text: String,
    /// The last Enter did not match.
    mismatch: bool,
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
    pub(super) kind: ActionKind,
    pub(super) machine: String,
    phase: Phase,
    /// The operator chose to quit: when the shell answers that the process is over, quit.
    quit_when_ended: bool,
    /// The fleet lock this action holds (a `u`), let go when it ends.
    lock: Option<RequestId>,
}

impl Action {
    pub(super) const fn active(&self) -> bool {
        matches!(self.phase, Phase::Running | Phase::Stopping)
    }
}

impl Model {
    fn action_active(&self) -> bool {
        self.action.as_ref().is_some_and(Action::active)
            || self.roll_active()
            || self.lock_wait.is_some()
    }

    pub(super) const fn fresh_id(&mut self) -> RequestId {
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
    pub(super) fn ask(&mut self, confirm: Confirm) {
        self.confirm_serial += 1;
        self.confirm = Some(confirm);
    }

    pub(super) fn notice(&mut self, text: impl Into<String>) -> Command<Effect, Event> {
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
    let confirm = model.confirm.as_ref().map(|c| confirm_view(model, c));
    (mode, action, confirm)
}

fn confirm_view(model: &Model, confirm: &Confirm) -> ConfirmView {
    let (kind, refusals, warnings, typed) = match confirm {
        Confirm::Act => (ConfirmKind::Act, Vec::new(), Vec::new(), None),
        Confirm::Checking { .. } => (ConfirmKind::Checking, Vec::new(), Vec::new(), None),
        Confirm::Quit => (ConfirmKind::Quit, Vec::new(), Vec::new(), None),
        Confirm::Unfreeze { .. } => (ConfirmKind::Unfreeze, Vec::new(), Vec::new(), None),
        Confirm::CheckingCluster { .. } => {
            (ConfirmKind::CheckingCluster, Vec::new(), Vec::new(), None)
        }
        Confirm::UpdateAll(_) => (ConfirmKind::UpdateAll, Vec::new(), Vec::new(), None),
        Confirm::Update {
            machine,
            review,
            typed,
            ..
        } => {
            let kind = if !review.refusals.is_empty() {
                ConfirmKind::Refused
            } else if typed.is_some() {
                ConfirmKind::UpdateTyped
            } else {
                ConfirmKind::Update
            };
            (
                kind,
                review.refusals.clone(),
                review.warnings.clone(),
                typed.as_ref().map(|t| TypedView {
                    expected: machine.clone(),
                    text: t.text.clone(),
                    mismatch: t.mismatch,
                }),
            )
        }
    };
    ConfirmView {
        kind,
        question: question(model, confirm),
        serial: model.confirm_serial,
        refusals,
        warnings,
        typed,
        lines: match confirm {
            Confirm::UpdateAll(planned) => planned.lines(),
            _ => Vec::new(),
        },
    }
}

fn question(model: &Model, confirm: &Confirm) -> String {
    match confirm {
        Confirm::Act => "turn on act mode? updates will be possible. y/N".to_owned(),
        Confirm::Checking { machine, .. } => {
            format!("checking {machine} before the update… any key cancels")
        }
        Confirm::Update {
            machine,
            to,
            review,
            typed,
            ..
        } => {
            if !review.refusals.is_empty() {
                format!("{machine} will not be updated.")
            } else if typed.is_some() {
                format!(
                    "update {machine} to {to}? it is marked public: type {machine}, then \
                     Enter. Esc cancels"
                )
            } else {
                format!("update {machine} to {to}? y/N")
            }
        }
        Confirm::Unfreeze { machine } => {
            format!("unfreeze {machine}? updates to it will be possible again. y/N")
        }
        Confirm::CheckingCluster { .. } => {
            "looking at the cluster and the fleet repository before the update of all is planned… any key cancels".to_owned()
        }
        Confirm::UpdateAll(planned) => {
            let n = planned.machines();
            format!(
                "update {n} machine{} as listed above? y/N",
                if n == 1 { "" } else { "s" }
            )
        }
        Confirm::Quit => {
            if let Some(roll) = model.roll.as_ref().filter(|r| r.active()) {
                return roll::quit_question(roll);
            }
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

/// Forgets the open question. If it was the checks of an update still running in the shell,
/// they are cancelled: the returned commands must be sent, or the shell keeps a `git` and a
/// `nix` running for a dialog nobody is looking at.
pub(super) fn drop_confirm(model: &mut Model) -> Vec<Command<Effect, Event>> {
    match model.confirm.take() {
        Some(Confirm::Checking { id, .. } | Confirm::CheckingCluster { id, .. }) => {
            vec![Command::notify_shell(CancelRequest { id }).into()]
        }
        _ => Vec::new(),
    }
}

/// `A`: toggles act mode. Turning it off is immediate; turning it on asks.
pub(super) fn toggle_act(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    if !model.config.as_ref().is_some_and(|c| c.allow_act) {
        return model.notice("act mode is not available in this run");
    }
    if model.act && model.roll_active() {
        return model
            .notice("an update of all machines is running: act mode stays on until it ends");
    }
    if model.act {
        model.act = false;
        let mut commands = drop_confirm(model);
        commands.push(render());
        return Command::all(commands);
    }
    let mut commands = drop_confirm(model);
    model.ask(Confirm::Act);
    commands.push(render());
    Command::all(commands)
}

/// `u` or `b` on the selected machine.
pub(super) fn request(model: &mut Model, kind: ActionKind) -> Command<Effect, Event> {
    model.view.notice = None;
    if kind == ActionKind::Update && !model.act {
        return model.notice(ACT_MODE_OFF);
    }
    if model.roll_active() {
        return model.notice("an update of all machines is running");
    }
    if model.freeze.rechecking() {
        return model
            .notice("still checking which machines are frozen before an update; ask again");
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
        Ok(checked) if kind == ActionKind::Update => match not_yet(model, &machine) {
            Some(why) => model.notice(why),
            None => start_checks(model, machine, checked),
        },
        Ok(_) => launch(model, kind, machine),
    }
}

pub(super) fn article(kind: ActionKind) -> String {
    let a = if kind == ActionKind::Update {
        "an"
    } else {
        "a"
    };
    format!("{a} {}", kind.label())
}

/// The system a machine's evaluation produced: what an update would deploy.
pub(super) struct Shown {
    /// As the question shows it, as the detail panel does: the hash's first 8 characters and
    /// the name (the name alone does not identify a system: two configurations on one nixpkgs
    /// share it).
    pub(super) label: String,
    /// The whole store path, to tell at `y` whether it is still the one that was asked about.
    pub(super) path: String,
    /// The same, as the value whose closure the checks size.
    pub(super) store_path: StorePath,
}

/// Everything that must hold before an action on `machine` may be asked for. On success, the
/// pending system the machine's evaluation produced.
pub(super) fn check(model: &Model, kind: ActionKind, machine: &str) -> Result<Shown, String> {
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
    // A frozen machine is never updated, and neither is any machine while it is unknown which
    // are frozen (`freeze.rs`): the question is asked here, so it is asked again at the `y`.
    if kind == ActionKind::Update
        && let Some(why) = model.freeze.refusal(machine)
    {
        return Err(why);
    }
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
        label: format!("{} {}", &eval.out_path.hash()[..8], eval.out_path.name()),
        path: eval.out_path.as_str().to_owned(),
        store_path: eval.out_path.clone(),
    })
}

/// `u` has passed [`check`]: asks the shell for what the checks need, and shows that they are
/// running. The dialog proper opens when [`preflight_answered`] has the answer.
fn start_checks(model: &mut Model, machine: String, shown: Shown) -> Command<Effect, Event> {
    let mut commands = drop_confirm(model);
    let (flake, timeout_seconds) = model.config.as_ref().map_or_else(
        || (String::new(), 0),
        |c| (c.flake.clone(), c.preflight_timeout_seconds),
    );
    let id = model.fresh_id();
    // A new question, so a shell must draw it before a key counts for it.
    model.ask(Confirm::Checking {
        machine: machine.clone(),
        to: shown.label,
        path: shown.path,
        id,
    });
    commands.push(
        Command::request_from_shell(PreflightRequest {
            id,
            machine,
            flake,
            store_path: shown.store_path,
            timeout_seconds,
        })
        .then_send(move |answer| Event::PreflightAnswered { id, answer }),
    );
    commands.push(render());
    Command::all(commands)
}

/// `machine`'s probe is out in this refresh: what it would say is not "unknown", it is not here
/// yet (a refresh clears every probe and asks for them again).
fn probing(model: &Model, machine: &str) -> bool {
    model
        .outstanding
        .values()
        .any(|p| matches!(p, Pending::Probe(n) if n == machine))
}

/// Address resolution is out in this refresh: until it answers, no collision can be ruled out.
fn resolving(model: &Model) -> bool {
    model
        .outstanding
        .values()
        .any(|p| matches!(p, Pending::Resolve(_)))
}

/// `machine`'s probe was cut off at the settle deadline: it never answered. (A probe that
/// answered, even to say the machine is unreachable, is another matter: that is information.)
fn probe_timed_out(model: &Model, machine: &str) -> bool {
    model.rows.iter().any(|r| {
        r.machine.name == machine
            && matches!(
                &r.probe,
                Some(ProbeOutcome::Unreachable { reason }) if reason == super::PROBE_TIMED_OUT
            )
    })
}

/// Address resolution was cut off at this refresh's settle deadline.
fn resolution_timed_out(model: &Model) -> bool {
    model.resolution_timed_out == Some(model.generation)
}

/// Why an update of `machine` cannot be asked about yet, when what the checks need from the
/// core is still on its way. Refused at `u` rather than shown as a warning: "not known yet"
/// must not read as "not known".
pub(super) fn not_yet(model: &Model, machine: &str) -> Option<String> {
    if probing(model, machine) {
        return Some(format!("still probing {machine}; try again in a moment"));
    }
    if probe_timed_out(model, machine) {
        return Some(format!(
            "{machine}'s probe timed out; an update needs ssh: refresh and try again"
        ));
    }
    if resolving(model) {
        return Some("still resolving addresses; try again in a moment".to_owned());
    }
    resolution_timed_out(model)
        .then(|| "address resolution timed out: refresh and try again".to_owned())
}

/// What the checks decide for `machine` now: the shell's `answer` and what the core holds.
///
/// Called when the answer comes in (what the dialog shows) and again when the operator
/// confirms (what decides), so a refresh in between that changed an address or a probe's free
/// space is seen by the second.
pub(super) fn review(model: &Model, machine: &str, answer: &PreflightAnswer) -> Preflight {
    let free_bytes = model
        .rows
        .iter()
        .find(|r| r.machine.name == machine)
        .and_then(|r| match &r.probe {
            Some(ProbeOutcome::Observed(o)) => o.ssh_health.store_disk,
            _ => None,
        })
        .map(|d| d.available_bytes);
    // What the machine already has, when the package preview fetched its closure: the update adds
    // only the rest.
    let deployed = model
        .rows
        .iter()
        .find(|r| r.machine.name == machine)
        .and_then(|r| r.deployed_paths.as_ref());
    let known = |name: &str| model.addresses.get(name).is_some_and(|a| !a.is_empty());
    let shared = shared_ips(&model.addresses);
    let others_unknown = model
        .rows
        .iter()
        // A machine with no target at all has nothing to collide with; one whose hostname has
        // not (yet) resolved might.
        .filter(|r| r.machine.name != machine && r.target.is_ok() && !known(&r.machine.name))
        .count();
    let public = model
        .config
        .as_ref()
        .is_some_and(|c| c.public_machines.contains(machine));
    preflight(&Facts {
        machine,
        tree: answer.tree.as_ref().map_err(String::as_str),
        closure: answer
            .closure
            .as_ref()
            .map(|sizes| estimate(sizes, deployed))
            .map_err(String::as_str),
        free_bytes,
        probing: probing(model, machine),
        probe_timed_out: probe_timed_out(model, machine),
        addresses: Addresses {
            own_known: known(machine),
            shared: &shared,
            others_unknown,
            resolving: resolving(model),
            resolution_timed_out: resolution_timed_out(model),
        },
        public,
    })
}

/// The checks are in: opens the dialog.
fn preflight_answered(
    model: &mut Model,
    id: RequestId,
    answer: PreflightAnswer,
) -> Command<Effect, Event> {
    let Some(Confirm::Checking {
        machine,
        to,
        path,
        id: waiting,
    }) = model.confirm.clone()
    else {
        return Command::done();
    };
    if waiting != id {
        return Command::done();
    }
    // The list may have been refreshed while the shell was looking.
    let now = match check(model, ActionKind::Update, &machine) {
        Ok(now) => now,
        Err(why) => {
            model.confirm = None;
            return model.notice(why);
        }
    };
    // ... and if that changed the pending system, the answer is about another closure: look
    // again, at the new one.
    if now.path != path {
        let note = format!("the pending system of {machine} changed; checking again");
        model.confirm = None;
        let command = start_checks(model, machine, now);
        model.view.notice = Some(note);
        return command;
    }
    let review = review(model, &machine, &answer);
    let typed = (review.refusals.is_empty() && review.typed_confirmation).then(Typed::default);
    // A new question (the dialog proper): a shell must draw it before a key counts for it, so
    // a key typed while the checks ran can neither confirm nor cancel what it never saw.
    model.ask(Confirm::Update {
        machine,
        to,
        path,
        answer,
        review,
        typed,
    });
    render()
}

/// The request to run `kind` on `machine` as request `id`, writing into Log tab run `log` (an
/// update of all) or its own (`None`). Called only after [`check`] and, for an update, the
/// operator's confirmation.
pub(super) fn action_request(
    model: &Model,
    kind: ActionKind,
    machine: &str,
    id: RequestId,
    log: Option<RequestId>,
) -> Result<ActionRequest, String> {
    let Some(row) = model.rows.iter().find(|r| r.machine.name == machine) else {
        return Err(format!(
            "{machine} is not in the inventory; not touching it"
        ));
    };
    let system = match &row.eval {
        Some(EvalOutcome::Success(e)) => e.system.clone(),
        _ => None,
    };
    let flake = model
        .config
        .as_ref()
        .map(|c| c.flake.clone())
        .unwrap_or_default();
    Ok(ActionRequest {
        id,
        kind,
        machine: machine.to_owned(),
        class: row.machine.class,
        system,
        flake,
        build_host: row.machine.build_host.clone(),
        target: row.target.as_ref().ok().map(|t| t.target.to_string()),
        log,
    })
}

/// Issues the request. Called only after [`check`] and, for an update, the operator's
/// confirmation.
fn launch(model: &mut Model, kind: ActionKind, machine: String) -> Command<Effect, Event> {
    launch_with(model, kind, machine, None)
}

/// [`launch`] of an update that holds the fleet's lock `lock`, which it lets go of when it ends.
pub(super) fn launch_locked(
    model: &mut Model,
    machine: String,
    lock: RequestId,
) -> Command<Effect, Event> {
    launch_with(model, ActionKind::Update, machine, Some(lock))
}

fn launch_with(
    model: &mut Model,
    kind: ActionKind,
    machine: String,
    lock: Option<RequestId>,
) -> Command<Effect, Event> {
    let id = model.fresh_id();
    let request = match action_request(model, kind, &machine, id, None) {
        Ok(r) => r,
        Err(why) => {
            let release = lock.map(|id| Command::notify_shell(FleetUnlockRequest { id }).into());
            return Command::all(release.into_iter().chain([model.notice(why)]));
        }
    };
    model.action = Some(Action {
        id,
        kind,
        machine,
        phase: Phase::Running,
        quit_when_ended: false,
        lock,
    });
    Command::all([
        Command::request_from_shell(request).then_send(move |end| Event::ActionEnded { id, end }),
        render(),
    ])
}

/// The operator has confirmed the update of `machine` (a `y`, or the exact name). The world
/// may have moved while the dialog was open: act mode turned off, an action started, the list
/// refreshed, an address changed. Everything is decided again, with the checks' answer the
/// dialog was showing.
/// Whether the file of freezes was read again since the `y`: the first time through, an update that
/// passes every rule asks for that read ([`super::freeze::recheck`]); the second, it is requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    /// Straight from the operator's `y` (or typed name).
    Confirmed,
    /// The file has been read again, and merged into what the core knows.
    FreezesRead,
}

fn confirm_update(model: &mut Model, asked: Asked, stage: Stage) -> Command<Effect, Event> {
    let Asked {
        machine,
        to,
        path,
        answer,
        review: shown,
        typed_given,
    } = asked;
    if !model.act {
        return model.notice(ACT_MODE_OFF);
    }
    if model.action_active() {
        return model.notice("another action is already running");
    }
    // The tree is being read again (the list, the fleet's evaluation or this machine's own):
    // what would be deployed is not settled, and the evaluation the dialog showed may be the one
    // being replaced. The operator asks again once it is.
    if model.refresh_running(&machine) {
        return model.notice(REFRESH_RUNNING);
    }
    let now = match check(model, ActionKind::Update, &machine) {
        Ok(now) => now,
        Err(why) => return model.notice(why),
    };
    // The pending system is not the one the operator was shown (a refresh evaluated the tree
    // again): what they said yes to is not what would be deployed, and the checks were about
    // another closure. Nothing is deployed; the checks run again for the new one.
    if now.path != path {
        let note = format!("the pending system of {machine} changed; checking again");
        let command = start_checks(model, machine, now);
        model.view.notice = Some(note);
        return command;
    }
    let now = review(model, &machine, &answer);
    if !now.refusals.is_empty() {
        return model.notice(format!(
            "not updating {machine}: {}",
            now.refusals.join("; ")
        ));
    }
    // Every rule is decided again, not only the refusals: what the operator confirmed is the
    // dialog they saw. If the rules now say something else, they are asked again about that.
    // The typed name may have become required (a reconfigured `public`, or the AUS-22 hook
    // reading the pending system), and a check may have turned up something new to read.
    let again = if now.typed_confirmation && !typed_given {
        Some(format!(
            "{machine} now has to be confirmed by typing its name"
        ))
    } else if now.warnings != shown.warnings {
        Some("what the checks found changed; asking again".to_owned())
    } else {
        None
    };
    if let Some(note) = again {
        let typed = (now.refusals.is_empty() && now.typed_confirmation).then(Typed::default);
        model.ask(Confirm::Update {
            machine,
            to,
            path,
            answer,
            review: now,
            typed,
        });
        return model.notice(note);
    }
    if stage == Stage::FreezesRead {
        // The last rule: one auspex updates a fleet at a time. The update is launched when the lock
        // is held (`roll::lock_answered`).
        return roll::lock_for_update(model, machine);
    }
    // Everything the core can know says yes. The last look is at the file, which another
    // running auspex may have changed since this one read it (`freeze.rs`); every rule above is
    // decided once more on what the core knows when the answer is in.
    super::freeze::recheck(
        model,
        Asked {
            machine,
            to,
            path,
            answer,
            review: shown,
            typed_given,
        },
    )
}

/// The file was read again and merged: the update is decided as at the `y`, on what the core
/// knows now (the disk and address rules, the refresh and act-mode guards, the pending system,
/// the freeze, the typed name), and requested only if every rule still says yes.
pub(super) fn confirm_update_after_read(model: &mut Model, asked: Asked) -> Command<Effect, Event> {
    confirm_update(model, asked, Stage::FreezesRead)
}

/// What the dialog the operator confirmed was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Asked {
    pub(super) machine: String,
    to: String,
    pub(super) path: String,
    answer: PreflightAnswer,
    /// What the checks decided when the dialog was drawn.
    review: Preflight,
    /// The operator typed the machine's name (so the typed confirmation, if required now, is
    /// satisfied).
    typed_given: bool,
}

/// `s` on the selected machine: the operator's own ssh session, with the terminal.
///
/// Act mode only, like `u`: it is a login to a machine as whatever user the target names, so it
/// belongs behind the same deliberate switch (and a headless run, which cannot be in act mode,
/// never gets here: the first check says so rather than relying on the second). Refused for a
/// machine with no usable target: a target that did not parse is never fallen back from, as for
/// every other use of it. Nothing about the machine's health, its evaluation or an action in
/// progress matters: ssh is for the machine that is broken, or being updated, as much as for the
/// one that is fine.
pub(super) fn open_shell(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    if !model.config.as_ref().is_some_and(|c| c.allow_act) {
        return model.notice("act mode is not available in this run");
    }
    if !model.act {
        return model.notice(ACT_MODE_OFF);
    }
    if let Some((_, machine)) = &model.shell {
        let text = format!("ssh to {machine} is already open");
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
    if !is_plain_name(&machine) {
        return model.notice(format!(
            "{machine:?} is not a plain machine name; not touching it"
        ));
    }
    let target = match model.rows.iter().find(|r| r.machine.name == machine) {
        None => {
            return model.notice(format!(
                "{machine} is not in the inventory; not touching it"
            ));
        }
        Some(row) => match &row.target {
            Ok(resolved) => resolved.target.clone(),
            Err(why) => {
                return model.notice(format!(
                    "{machine} has no usable ssh target ({why}): not opening ssh"
                ));
            }
        },
    };
    let id = model.fresh_id();
    model.shell = Some((id, machine.clone()));
    Command::all([
        Command::request_from_shell(ShellRequest {
            id,
            machine,
            target,
        })
        .then_send(move |end| Event::ShellEnded { id, end }),
        render(),
    ])
}

/// The shell's answer: ssh is over and the terminal is back. Said in the status bar in the
/// words the operator would use for any program: `ssh to luna exited (code 255)`.
pub(super) fn shell_ended(
    model: &mut Model,
    id: RequestId,
    end: &ShellEnd,
) -> Command<Effect, Event> {
    let Some((_, machine)) = model.shell.take_if(|(open, _)| *open == id) else {
        return Command::done();
    };
    let text = match end {
        ShellEnd::Exited { code } => format!("ssh to {machine} exited (code {code})"),
        ShellEnd::Signaled { signal } => {
            format!("ssh to {machine} exited (killed by signal {signal})")
        }
        ShellEnd::NotStarted { reason } => format!("ssh to {machine} did not start: {reason}"),
    };
    model.notice(text)
}

/// `y` or anything else, to the open question.
pub(super) fn answer(model: &mut Model, yes: bool) -> Command<Effect, Event> {
    model.view.notice = None;
    let Some(confirm) = model.confirm.take() else {
        return Command::done();
    };
    match confirm {
        // Anything cancels the checks: nothing was decided yet, so there is nothing to
        // confirm, and a `y` typed ahead of the dialog must not confirm what it never saw.
        Confirm::Checking { id, .. } | Confirm::CheckingCluster { id, .. } => {
            Command::all([Command::notify_shell(CancelRequest { id }).into(), render()])
        }
        // A refusal has no yes.
        Confirm::Update { ref review, .. } if !review.refusals.is_empty() => render(),
        // A typed confirmation is not answered by `y`: only the name confirms. `y` leaves it
        // open and as it was; anything else cancels.
        confirm @ Confirm::Update { typed: Some(_), .. } => {
            if yes {
                model.confirm = Some(confirm);
            }
            render()
        }
        _ if !yes => render(),
        Confirm::Act => {
            // Re-checked: the question may have been answered after nothing could ask it.
            model.act = model.config.as_ref().is_some_and(|c| c.allow_act);
            render()
        }
        Confirm::Update {
            machine,
            to,
            path,
            answer,
            review,
            ..
        } => confirm_update(
            model,
            Asked {
                machine,
                to,
                path,
                answer,
                review,
                typed_given: false,
            },
            Stage::Confirmed,
        ),
        Confirm::Unfreeze { machine } => super::freeze::unfreeze_confirmed(model, machine),
        Confirm::UpdateAll(planned) => roll::confirmed(model, &planned),
        Confirm::Quit => stop_and_quit(model),
    }
}

/// A character typed into the typed confirmation.
pub(super) fn typed_char(model: &mut Model, c: char) -> Command<Effect, Event> {
    if let Some(Confirm::Update {
        typed: Some(typed), ..
    }) = &mut model.confirm
    {
        // Only what a machine name can contain: nothing else can be the name, and nothing else
        // is worth showing back.
        if (c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) && typed.text.len() < MAX_TYPED {
            typed.text.push(c);
            typed.mismatch = false;
        }
    }
    render()
}

/// Backspace in the typed confirmation.
pub(super) fn typed_backspace(model: &mut Model) -> Command<Effect, Event> {
    if let Some(Confirm::Update {
        typed: Some(typed), ..
    }) = &mut model.confirm
    {
        typed.text.pop();
        typed.mismatch = false;
    }
    render()
}

/// Enter in the typed confirmation: the update goes ahead only for the exact name.
pub(super) fn typed_submit(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    let Some(Confirm::Update {
        machine,
        to,
        path,
        answer,
        review,
        typed: Some(typed),
    }) = &mut model.confirm
    else {
        return Command::done();
    };
    if typed.text != *machine {
        typed.mismatch = true;
        return render();
    }
    let asked = Asked {
        machine: machine.clone(),
        to: to.clone(),
        path: path.clone(),
        answer: answer.clone(),
        review: review.clone(),
        typed_given: true,
    };
    model.confirm = None;
    confirm_update(model, asked, Stage::Confirmed)
}

/// The operator's request to quit (`q`, Esc, Ctrl-C, `:q`).
pub(super) fn quit(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    if let Some(roll) = model.roll.as_ref().filter(|r| r.active()) {
        // The second quit while it is being stopped: the operator does not want to wait.
        if roll.stopping() {
            return force_quit(model);
        }
        let mut commands = Vec::new();
        if model.confirm != Some(Confirm::Quit) {
            commands.extend(drop_confirm(model));
            model.ask(Confirm::Quit);
        }
        commands.push(render());
        return Command::all(commands);
    }
    match model.action.as_ref().map(|a| &a.phase) {
        Some(Phase::Running) => {
            let mut commands = Vec::new();
            if model.confirm != Some(Confirm::Quit) {
                commands.extend(drop_confirm(model));
                model.ask(Confirm::Quit);
            }
            commands.push(render());
            Command::all(commands)
        }
        // The second quit: the process is not dying, the operator does not want to wait.
        Some(Phase::Stopping) => force_quit(model),
        _ => finish_quit(model),
    }
}

/// A termination signal: stop what runs and quit, without a question (nobody can answer one).
pub(super) fn terminate(model: &mut Model) -> Command<Effect, Event> {
    if let Some(roll) = model.roll.as_ref().filter(|r| r.active()) {
        return if roll.stopping() {
            force_quit(model)
        } else {
            stop_and_quit(model)
        };
    }
    match model.action.as_ref().map(|a| &a.phase) {
        Some(Phase::Running) => stop_and_quit(model),
        Some(Phase::Stopping) => force_quit(model),
        _ => finish_quit(model),
    }
}

/// Nothing is running: cancel the rest and quit.
fn finish_quit(model: &mut Model) -> Command<Effect, Event> {
    let mut commands = drop_confirm(model);
    model.view.quit = true;
    commands.extend(model.cancel_all());
    commands.push(render());
    Command::all(commands)
}

/// An action is running: cancel everything, and quit when the action has been answered.
fn stop_and_quit(model: &mut Model) -> Command<Effect, Event> {
    let mut commands = drop_confirm(model);
    commands.extend(model.cancel_all());
    if model.roll_active() {
        // The roll ends machine by machine, uncordoning what it cordoned, and sets `quit` when
        // the last one is over.
        commands.extend(roll::stop(model, true));
        commands.push(render());
        return Command::all(commands);
    }
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
    let mut commands = drop_confirm(model);
    commands.extend(roll::force(model));
    model.view.quit = true;
    commands.extend(model.cancel_all());
    commands.push(render());
    Command::all(commands)
}

/// The shell's answer: the process is over (killed and reaped, if it was cancelled).
pub(super) fn ended(model: &mut Model, id: RequestId, end: ActionEnd) -> Command<Effect, Event> {
    if let Some(command) = roll::action_ended(model, id, &end) {
        return command;
    }
    let Some(action) = model.action.as_mut().filter(|a| a.id == id && a.active()) else {
        return Command::done();
    };
    let (kind, machine, quitting) = (action.kind, action.machine.clone(), action.quit_when_ended);
    let lock = action.lock.take();
    action.phase = Phase::Ended(end);
    let mut commands: Vec<Command<Effect, Event>> = lock
        .map(|id| Command::notify_shell(FleetUnlockRequest { id }).into())
        .into_iter()
        .collect();
    if quitting {
        model.view.quit = true;
        commands.push(render());
        return Command::all(commands);
    }
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
    row.deployed_paths = None;
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
        Event::UpdateAll => roll::request(model),
        Event::KubeAnswered { id, answer } => {
            roll::kube_answered(model, id, &answer).unwrap_or_else(Command::done)
        }
        Event::AwaitAnswered { id, answer } => {
            roll::awaited(model, id, &answer).unwrap_or_else(Command::done)
        }
        Event::TreeChecked { id, answer } => {
            roll::tree_checked(model, id, &answer).unwrap_or_else(Command::done)
        }
        Event::FleetLockAnswered { id, answer } => roll::lock_answered(model, id, &answer),
        Event::BuildSelected => request(model, ActionKind::Build),
        Event::ShellSelected => open_shell(model),
        Event::ShellEnded { id, end } => shell_ended(model, id, &end),
        Event::ConfirmAnswer(yes) => answer(model, yes),
        Event::ConfirmChar(c) => typed_char(model, c),
        Event::ConfirmBackspace => typed_backspace(model),
        Event::ConfirmSubmit => typed_submit(model),
        Event::PreflightAnswered { id, answer } => roll::preflight_answered(model, id, &answer)
            .unwrap_or_else(|| preflight_answered(model, id, answer)),
        Event::ActionEnded { id, end } => ended(model, id, end),
        // Not an acting event: `update` never routes one here.
        _ => Command::done(),
    }
}
