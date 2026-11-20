//! `U`: update every machine that has an update pending, rolling per tag, Kubernetes-aware.
//!
//! # What the core guarantees
//!
//! - **One confirmation for the whole plan.** `U` collects the machines with an update pending,
//!   leaves out the frozen ones (and refuses outright while it is unknown which are frozen,
//!   exactly as `u` does: [`FreezeView::frozen`](crate::view::FreezeView::frozen) is `None`),
//!   splits the rest by the configured rolling tags ([`auspex_domain::roll::plan`]) and asks one
//!   question that lists the order, what runs in parallel and what is skipped, each with its
//!   reason. Nothing runs before the `y`.
//! - **Machines that share a rolling tag never update at the same time.** A rolling group is one
//!   machine after another, in name order; machines with no rolling tag update a few at a time
//!   (`update.parallel`). Groups run beside each other.
//! - **Each machine is checked at its turn, as `u` checks it.** The checks of PR #26 run for it
//!   when its turn comes (the fleet tree as it is then, the closure's size, the free disk, the
//!   duplicate-address rule), and the freeze is looked at again, in the core and by the shell
//!   right before the process starts. A refusal **skips that machine and the roll goes on**; a
//!   failure of a started update, a failed drain, or a machine that does not become healthy
//!   **stops its rolling group** (the rest of it is skipped) and the other groups carry on.
//!   Warnings of the checks do not stop a machine: they are written to its part of the Log tab.
//!   A machine whose name has to be typed (`public`) is skipped: that confirmation is for `u`.
//! - **Kubernetes.** A group with a `kubernetes = true` tag is refused up front if any node of the
//!   cluster is `NotReady` or cordoned, or a machine is no node of it (it is listed as skipped,
//!   with the reason). Between its machines the roll waits until the updated one runs the new
//!   system, its probe is healthy and its node is `Ready` ([`AwaitRequest`]). With `drain = true`
//!   the node is drained before its update and uncordoned after; **a node auspex cordoned is
//!   never left cordoned by auspex**: after a failed drain, a failed update, a machine that never
//!   became healthy, and a quit, the uncordon is asked for before the machine ends (and if it
//!   fails, the machine is failed and the log says to uncordon by hand).
//! - **Quit asks, and stops cleanly.** `q` during a roll asks; on `y` nothing new starts, the
//!   running `clan` processes are cancelled ([`ActionRequest`] is answered when the process
//!   group is gone), a drain in progress is cancelled and its node uncordoned, and
//!   [`ViewModel::quit`](crate::ViewModel::quit) becomes true only when every machine has ended.
//!   A second quit forces.
//! - **While it runs, nothing else changes the world under it**: `u`, `b`, `r` (refresh) and `A`
//!   say that a roll is running, and `U` says so again.
//!
//! The log: the roll has a run of its own in the Log tab (the plan first, one line per machine as
//! it ends, the totals), and each machine has one (its checks' warnings, the drain, `clan`'s
//! output, the wait, the uncordon). Each machine that is started is one action-log row (the
//! shell writes it, as for `u`); a machine that was refused or skipped was never tried and has
//! none.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use auspex_domain::roll::{self, Candidate, Group, Member, Node};
use crux_core::Command;
use crux_core::render::render;

use super::act::{ACT_MODE_OFF, Confirm, article, check, not_yet, review};
use super::{Event, Model, Pending};
use crate::effects::{
    ActionEnd, ActionKind, ActionResult, AwaitAnswer, AwaitRequest, CancelRequest, Effect,
    KubeAnswer, KubeOp, KubeReply, KubeRequest, LogOp, LogRequest, PreflightAnswer,
    PreflightRequest, RequestId,
};
use crate::view::{RollMachineView, RollState, RollStatus, RollView};

/// How a machine of the roll ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Done,
    Refused(String),
    Failed(String),
    Skipped(String),
    Cancelled,
}

impl Outcome {
    fn line(&self) -> String {
        match self {
            Self::Done => "done".to_owned(),
            Self::Refused(why) => format!("refused: {why}"),
            Self::Failed(why) => format!("failed: {why}"),
            Self::Skipped(why) => format!("skipped: {why}"),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }

    fn state(&self) -> RollState {
        match self {
            Self::Done => RollState::Done,
            Self::Refused(w) => RollState::Refused(w.clone()),
            Self::Failed(w) => RollState::Failed(w.clone()),
            Self::Skipped(w) => RollState::Skipped(w.clone()),
            Self::Cancelled => RollState::Cancelled,
        }
    }
}

/// Where one machine is in its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Queued,
    /// The checks before an update are running in the shell.
    Checking,
    /// `kubectl drain` is running. `cancelling`: the operator quit; the answer will say the
    /// process is gone, and the node is then uncordoned.
    Draining {
        cancelling: bool,
    },
    Updating,
    Waiting,
    /// `kubectl uncordon` is running; `then` is how the machine ends when it is over.
    Uncordoning {
        then: Outcome,
    },
    Ended(Outcome),
}

/// One machine of the roll.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    member: Member,
    /// The index of its rolling group, or `None` for a free machine.
    group: Option<usize>,
    /// Its run in the Log tab, once its turn has begun.
    log: Option<RequestId>,
    step: Step,
    /// The pending system the checks were asked about, and what the update deploys.
    path: Option<String>,
    /// auspex cordoned its node (the drain starts with a cordon) and has not uncordoned it.
    cordoned: bool,
}

/// A rolling group while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupRun {
    label: String,
    queue: VecDeque<String>,
    /// The machine whose turn it is.
    busy: Option<String>,
    /// A machine of it failed: nothing more of it starts.
    stopped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Stopping { quit: bool },
    Ended,
}

/// An update of all, from the `y` to its last machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Roll {
    /// The roll's own run in the Log tab.
    log: RequestId,
    status: Status,
    /// The plan's order, for the view.
    order: Vec<String>,
    entries: BTreeMap<String, Entry>,
    groups: Vec<GroupRun>,
    free: VecDeque<String>,
    parallel: usize,
    ready_timeout_seconds: u64,
    /// Which machine each request of this roll belongs to.
    ids: BTreeMap<RequestId, String>,
}

impl Roll {
    pub(super) const fn active(&self) -> bool {
        !matches!(self.status, Status::Ended)
    }

    pub(super) const fn stopping(&self) -> bool {
        matches!(self.status, Status::Stopping { .. })
    }

    fn all_ended(&self) -> bool {
        self.entries
            .values()
            .all(|e| matches!(e.step, Step::Ended(_)))
    }

    fn running_free(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.group.is_none() && !matches!(e.step, Step::Queued | Step::Ended(_)))
            .count()
    }

    fn counts(&self) -> String {
        let mut done = 0;
        let (mut refused, mut failed, mut skipped, mut cancelled) = (0, 0, 0, 0);
        for e in self.entries.values() {
            if let Step::Ended(o) = &e.step {
                match o {
                    Outcome::Done => done += 1,
                    Outcome::Refused(_) => refused += 1,
                    Outcome::Failed(_) => failed += 1,
                    Outcome::Skipped(_) => skipped += 1,
                    Outcome::Cancelled => cancelled += 1,
                }
            }
        }
        format!(
            "{done} done, {refused} refused, {failed} failed, {skipped} skipped, \
             {cancelled} cancelled"
        )
    }

    pub(super) fn view(&self) -> RollView {
        RollView {
            status: match self.status {
                Status::Running => RollStatus::Running,
                Status::Stopping { .. } => RollStatus::Stopping,
                Status::Ended => RollStatus::Ended,
            },
            machines: self
                .order
                .iter()
                .filter_map(|n| self.entries.get(n).map(|e| (n, e)))
                .map(|(name, e)| RollMachineView {
                    name: name.clone(),
                    state: match &e.step {
                        Step::Queued => RollState::Queued,
                        Step::Checking => RollState::Checking,
                        Step::Draining { .. } => RollState::Draining,
                        Step::Updating => RollState::Updating,
                        Step::Waiting => RollState::Waiting,
                        Step::Uncordoning { .. } => RollState::Uncordoning,
                        Step::Ended(o) => o.state(),
                    },
                })
                .collect(),
        }
    }

    /// The machines that are not over, for the quit question.
    fn running_names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, e)| !matches!(e.step, Step::Queued | Step::Ended(_)))
            .map(|(n, _)| n.as_str())
            .collect()
    }
}

impl Model {
    pub(super) fn roll_active(&self) -> bool {
        self.roll.as_ref().is_some_and(Roll::active)
    }
}

// ---------------------------------------------------------------------------------------
// The plan, and the question about it
// ---------------------------------------------------------------------------------------

/// A rolling group as the question lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlannedGroup {
    group: Group,
}

/// What `U` is about to ask, after the cluster was looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Planned {
    groups: Vec<PlannedGroup>,
    free: Vec<String>,
    /// Machines with an update pending that this `U` leaves alone, and why.
    skipped: Vec<(String, String)>,
    parallel: usize,
    ready_timeout_seconds: u64,
}

impl Planned {
    pub(super) fn machines(&self) -> usize {
        self.groups
            .iter()
            .map(|g| g.group.members.len())
            .sum::<usize>()
            + self.free.len()
    }

    /// The question's body: the order, what runs in parallel, what is skipped.
    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "update {} machine{}:",
            self.machines(),
            if self.machines() == 1 { "" } else { "s" }
        )];
        for g in &self.groups {
            let group = &g.group;
            let order = group
                .members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(" → ");
            let mut how = vec!["one at a time".to_owned()];
            if group.kubernetes() {
                how.push("each node must be Ready before the next".to_owned());
            }
            let drains: Vec<String> = group
                .members
                .iter()
                .filter_map(|m| m.drain.map(|t| format!("{} ({t}s)", m.name)))
                .collect();
            if !drains.is_empty() {
                how.push(format!("drains and uncordons {}", drains.join(", ")));
            }
            lines.push(format!(
                "rolling {} ({}): {order}",
                group.label(),
                how.join("; ")
            ));
        }
        if !self.free.is_empty() {
            lines.push(format!(
                "in parallel, at most {} at a time: {}",
                self.parallel.min(self.free.len()),
                self.free.join(", ")
            ));
        }
        for (name, why) in &self.skipped {
            lines.push(format!("skipped {name}: {why}"));
        }
        lines.push(
            "each machine is checked at its turn: a refusal skips it, a failure stops its \
             rolling group"
                .to_owned(),
        );
        lines
    }
}

/// `U` before the cluster has been looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Draft {
    plan: roll::Plan,
    members: BTreeMap<String, Member>,
    skipped: Vec<(String, String)>,
    parallel: usize,
    ready_timeout_seconds: u64,
}

impl Draft {
    /// Applies what the cluster said (or why it could not be asked) to every Kubernetes group:
    /// a group the cluster refuses is not rolled, and its machines are listed as skipped.
    fn finish(
        mut self,
        cluster: Option<&Result<Vec<Node>, String>>,
        unavailable: Option<&str>,
    ) -> Planned {
        let mut groups = Vec::new();
        for group in std::mem::take(&mut self.plan.groups) {
            if group.kubernetes() {
                let why = match (unavailable, cluster) {
                    (Some(why), _) => Some(format!("kubernetes is not usable here: {why}")),
                    (None, None) => Some("the cluster was not asked".to_owned()),
                    (None, Some(Err(why))) => Some(format!("cannot read the cluster: {why}")),
                    (None, Some(Ok(nodes))) => {
                        let members: Vec<&Member> = group.members.iter().collect();
                        let why = roll::cluster_refusals(nodes, &members);
                        (!why.is_empty()).then(|| why.join("; "))
                    }
                };
                if let Some(why) = why {
                    for m in &group.members {
                        self.skipped.push((
                            m.name.clone(),
                            format!("not rolled with {}: {why}", group.label()),
                        ));
                    }
                    continue;
                }
            }
            groups.push(PlannedGroup { group });
        }
        self.skipped.sort();
        Planned {
            groups,
            free: self.plan.free,
            skipped: self.skipped,
            parallel: self.parallel,
            ready_timeout_seconds: self.ready_timeout_seconds,
        }
    }
}

/// Whether `row`'s machine has an update pending, as the status badges say.
fn update_pending(row: &super::Row) -> bool {
    use auspex_domain::status::{Badge, MachineFacts, machine_status};
    let error = row.target.as_ref().err().map(|e| e.error.to_string());
    let facts = MachineFacts {
        target_error: error.as_deref(),
        ..row.facts(false)
    };
    machine_status(&facts)
        .badges
        .contains(&Badge::UpdatePending)
}

/// `U`.
pub(super) fn request(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    let Some(config) = model.config.clone() else {
        return model.notice("nothing is loaded yet");
    };
    if !config.allow_act {
        return model.notice("act mode is not available in this run");
    }
    if !model.act {
        return model.notice(ACT_MODE_OFF);
    }
    if model.roll_active() {
        return model.notice("an update of all machines is already running");
    }
    if let Some(a) = model.action.as_ref().filter(|a| a.active()) {
        let text = format!("{} of {} is already running", article(a.kind), a.machine);
        return model.notice(text);
    }
    if model.freeze.rechecking() {
        return model
            .notice("still checking which machines are frozen before an update; ask again");
    }
    if model.inventory != super::Inventory::Loaded
        || model
            .outstanding
            .values()
            .any(|p| matches!(p, Pending::Inventory | Pending::Eval | Pending::EvalOne(_)))
    {
        return model.notice(super::act::REFRESH_RUNNING);
    }
    // The same answer `u` gives, once for everything: while it is unknown which machines are
    // frozen, updating "the rest" would update the one the operator froze.
    let view = super::view_freeze(model);
    let Some(frozen) = view.frozen() else {
        let why = view.unknown.as_deref().unwrap_or("unknown");
        return model.notice(format!(
            "which machines are frozen is unknown ({why}); not updating anything"
        ));
    };

    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    let mut members = BTreeMap::new();
    for row in model.rows.iter().filter(|r| update_pending(r)) {
        let name = row.machine.name.as_str();
        if frozen.contains(name) {
            skipped.push((name.to_owned(), "frozen".to_owned()));
        } else if config.public_machines.contains(name) {
            skipped.push((
                name.to_owned(),
                "marked public: its name has to be typed, so update it with u".to_owned(),
            ));
        } else if let Err(why) = check(model, ActionKind::Update, name) {
            skipped.push((name.to_owned(), why));
        } else {
            candidates.push(Candidate {
                name,
                tags: &row.machine.tags,
            });
        }
    }
    if candidates.is_empty() {
        let mut text = "no machine has an update that U can start".to_owned();
        if !skipped.is_empty() {
            let names: Vec<&str> = skipped.iter().map(|(n, _)| n.as_str()).collect();
            let _ = write!(text, " (left alone: {})", names.join(", "));
        }
        return model.notice(text);
    }
    let plan = roll::plan(&candidates, &config.rolling.rules);
    for g in &plan.groups {
        for m in &g.members {
            members.insert(m.name.clone(), m.clone());
        }
    }
    let draft = Draft {
        plan,
        members,
        skipped,
        parallel: config.rolling.parallel.max(1),
        ready_timeout_seconds: config.rolling.ready_timeout_seconds,
    };

    let mut commands = super::act::drop_confirm(model);
    let kube = draft.plan.groups.iter().any(Group::kubernetes);
    if kube && config.rolling.kubernetes_unavailable.is_none() {
        // One look at the cluster for every Kubernetes group: they share the kubeconfig.
        let id = model.fresh_id();
        model.ask(Confirm::CheckingCluster {
            id,
            draft: Box::new(draft),
        });
        commands.push(
            Command::request_from_shell(KubeRequest {
                id,
                log: None,
                op: KubeOp::Nodes,
            })
            .then_send(move |answer| Event::KubeAnswered { id, answer }),
        );
        commands.push(render());
        return Command::all(commands);
    }
    let planned = draft.finish(None, config.rolling.kubernetes_unavailable.as_deref());
    commands.push(open_question(model, planned));
    Command::all(commands)
}

/// Opens the question, or says there is nothing to roll.
fn open_question(model: &mut Model, planned: Planned) -> Command<Effect, Event> {
    if planned.machines() == 0 {
        let why: Vec<String> = planned
            .skipped
            .iter()
            .map(|(n, w)| format!("{n}: {w}"))
            .collect();
        return model.notice(format!("nothing to update: {}", why.join("; ")));
    }
    model.ask(Confirm::UpdateAll(Box::new(planned)));
    render()
}

/// The cluster's answer, for the plan: from [`Confirm::CheckingCluster`].
fn cluster_answered(
    model: &mut Model,
    id: RequestId,
    answer: &KubeAnswer,
) -> Option<Command<Effect, Event>> {
    let Some(Confirm::CheckingCluster { id: waiting, .. }) = &model.confirm else {
        return None;
    };
    if *waiting != id {
        return None;
    }
    let Some(Confirm::CheckingCluster { draft, .. }) = model.confirm.take() else {
        return None;
    };
    let unavailable = model
        .config
        .as_ref()
        .and_then(|c| c.rolling.kubernetes_unavailable.clone());
    let nodes = match answer {
        Ok(KubeReply::Nodes(nodes)) => Ok(nodes.clone()),
        Ok(KubeReply::Cancelled) => Err("the request was cancelled".to_owned()),
        Ok(KubeReply::Done) => Err("the shell answered a node list with nothing".to_owned()),
        Err(why) => Err(why.clone()),
    };
    let planned = draft.finish(Some(&nodes), unavailable.as_deref());
    Some(open_question(model, planned))
}

// ---------------------------------------------------------------------------------------
// The roll
// ---------------------------------------------------------------------------------------

fn log(run: RequestId, op: LogOp) -> Command<Effect, Event> {
    Command::notify_shell(LogRequest { run, op }).into()
}

fn say(run: RequestId, text: impl Into<String>) -> Command<Effect, Event> {
    log(run, LogOp::Line { text: text.into() })
}

/// The operator confirmed the plan: the roll starts. The world may have moved while the question
/// was open, so the guards are decided again; each machine is decided again at its turn anyway.
pub(super) fn confirmed(model: &mut Model, planned: &Planned) -> Command<Effect, Event> {
    if !model.act {
        return model.notice(ACT_MODE_OFF);
    }
    if model.roll_active()
        || model
            .action
            .as_ref()
            .is_some_and(super::act::Action::active)
    {
        return model.notice("another action is already running");
    }
    if model.freeze.rechecking() {
        return model
            .notice("still checking which machines are frozen before an update; ask again");
    }
    let run = model.fresh_id();
    let mut entries = BTreeMap::new();
    let mut order = Vec::new();
    let mut groups = Vec::new();
    for (index, planned_group) in planned.groups.iter().enumerate() {
        let group = &planned_group.group;
        groups.push(GroupRun {
            label: group.label(),
            queue: group.members.iter().map(|m| m.name.clone()).collect(),
            busy: None,
            stopped: false,
        });
        for m in &group.members {
            order.push(m.name.clone());
            entries.insert(
                m.name.clone(),
                Entry {
                    member: m.clone(),
                    group: Some(index),
                    log: None,
                    step: Step::Queued,
                    path: None,
                    cordoned: false,
                },
            );
        }
    }
    for name in &planned.free {
        order.push(name.clone());
        entries.insert(
            name.clone(),
            Entry {
                member: Member {
                    name: name.clone(),
                    kubernetes: false,
                    drain: None,
                },
                group: None,
                log: None,
                step: Step::Queued,
                path: None,
                cordoned: false,
            },
        );
    }
    let mut roll = Roll {
        log: run,
        status: Status::Running,
        order,
        entries,
        groups,
        free: planned.free.iter().cloned().collect(),
        parallel: planned.parallel,
        ready_timeout_seconds: planned.ready_timeout_seconds,
        ids: BTreeMap::new(),
    };
    let mut out = vec![log(
        run,
        LogOp::Begin {
            title: "update all".to_owned(),
            group: true,
        },
    )];
    for line in planned.lines() {
        out.push(say(run, line));
    }
    advance(model, &mut roll, &mut out);
    model.roll = Some(roll);
    out.push(render());
    Command::all(out)
}

/// Starts whatever may start now, and ends the roll when nothing is left.
fn advance(model: &mut Model, roll: &mut Roll, out: &mut Vec<Command<Effect, Event>>) {
    loop {
        let mut progressed = false;
        if roll.status == Status::Running {
            for g in 0..roll.groups.len() {
                if roll.groups[g].stopped || roll.groups[g].busy.is_some() {
                    continue;
                }
                if let Some(next) = roll.groups[g].queue.pop_front() {
                    roll.groups[g].busy = Some(next.clone());
                    begin(model, roll, &next, out);
                    progressed = true;
                }
            }
            while roll.running_free() < roll.parallel {
                let Some(next) = roll.free.pop_front() else {
                    break;
                };
                begin(model, roll, &next, out);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    if roll.active() && roll.all_ended() {
        finish(model, roll, out);
    }
}

fn finish(model: &mut Model, roll: &mut Roll, out: &mut Vec<Command<Effect, Event>>) {
    let quit = matches!(roll.status, Status::Stopping { quit: true });
    roll.status = Status::Ended;
    let totals = roll.counts();
    out.push(log(
        roll.log,
        LogOp::End {
            text: format!("update all over: {totals}"),
        },
    ));
    if quit {
        model.view.quit = true;
    } else {
        model.view.notice = Some(format!("update of all machines over: {totals}"));
    }
}

/// A machine's turn begins: its run in the Log tab, the guards, and the checks.
fn begin(model: &mut Model, roll: &mut Roll, name: &str, out: &mut Vec<Command<Effect, Event>>) {
    let run = model.fresh_id();
    if let Some(e) = roll.entries.get_mut(name) {
        e.log = Some(run);
        e.step = Step::Checking;
    }
    out.push(log(
        run,
        LogOp::Begin {
            title: format!("update {name}"),
            group: false,
        },
    ));
    out.push(say(roll.log, format!("{name}: starting")));
    // The guards `u` applies before it asks, decided now rather than at the `y`.
    let shown = check(model, ActionKind::Update, name)
        .and_then(|shown| not_yet(model, name).map_or(Ok(shown), Err));
    let shown = match shown {
        Ok(s) => s,
        Err(why) => {
            end_member(model, roll, name, Outcome::Refused(why), out);
            return;
        }
    };
    let (flake, timeout_seconds) = model.config.as_ref().map_or_else(
        || (String::new(), 0),
        |c| (c.flake.clone(), c.preflight_timeout_seconds),
    );
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    if let Some(e) = roll.entries.get_mut(name) {
        e.path = Some(shown.path.clone());
    }
    out.push(say(run, format!("checking {name} before the update")));
    out.push(
        Command::request_from_shell(PreflightRequest {
            id,
            machine: name.to_owned(),
            flake,
            store_path: shown.store_path,
            timeout_seconds,
        })
        .then_send(move |answer| Event::PreflightAnswered { id, answer }),
    );
}

/// The checks of the machine whose request was `id` are in. `None`: `id` is not the roll's.
pub(super) fn preflight_answered(
    model: &mut Model,
    id: RequestId,
    answer: &PreflightAnswer,
) -> Option<Command<Effect, Event>> {
    with_roll(model, id, |model, roll, name, out| {
        let (Some(entry), Some(run)) = (
            roll.entries.get(&name),
            roll.entries.get(&name).and_then(|e| e.log),
        ) else {
            return;
        };
        if entry.step != Step::Checking || roll.stopping() {
            return;
        }
        let asked = entry.path.clone();
        // The world may have moved while the shell looked: decided again, as `u` does at its `y`.
        let now = match check(model, ActionKind::Update, &name) {
            Ok(now) => now,
            Err(why) => return end_member(model, roll, &name, Outcome::Refused(why), out),
        };
        if asked.as_deref() != Some(now.path.as_str()) {
            return end_member(
                model,
                roll,
                &name,
                Outcome::Refused(
                    "its pending system changed while it was being checked".to_owned(),
                ),
                out,
            );
        }
        let review = review(model, &name, answer);
        if !review.refusals.is_empty() {
            return end_member(
                model,
                roll,
                &name,
                Outcome::Refused(review.refusals.join("; ")),
                out,
            );
        }
        if review.typed_confirmation {
            return end_member(
                model,
                roll,
                &name,
                Outcome::Refused("its name has to be typed to update it: use u on it".to_owned()),
                out,
            );
        }
        for w in &review.warnings {
            out.push(say(run, format!("warning: {w}")));
        }
        let drain = roll.entries.get(&name).and_then(|e| e.member.drain);
        match drain {
            Some(timeout_seconds) => start_drain(model, roll, &name, timeout_seconds, out),
            None => start_update(model, roll, &name, out),
        }
    })
}

fn start_drain(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    timeout_seconds: u64,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    let Some(entry) = roll.entries.get_mut(name) else {
        return;
    };
    // `kubectl drain` cordons first and leaves the node cordoned if it fails: from here on the
    // node is ours to uncordon, however this ends.
    entry.cordoned = true;
    entry.step = Step::Draining { cancelling: false };
    let run = entry.log;
    if let Some(run) = run {
        out.push(say(
            run,
            format!("draining node {name} (kubectl's timeout {timeout_seconds}s)"),
        ));
    }
    out.push(
        Command::request_from_shell(KubeRequest {
            id,
            log: run,
            op: KubeOp::Drain {
                node: name.to_owned(),
                timeout_seconds,
            },
        })
        .then_send(move |answer| Event::KubeAnswered { id, answer }),
    );
}

fn start_update(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let id = model.fresh_id();
    let log = roll.entries.get(name).and_then(|e| e.log);
    let request = match super::act::action_request(model, ActionKind::Update, name, id, log) {
        Ok(r) => r,
        Err(why) => return end_member(model, roll, name, Outcome::Refused(why), out),
    };
    roll.ids.insert(id, name.to_owned());
    if let Some(e) = roll.entries.get_mut(name) {
        e.step = Step::Updating;
    }
    out.push(
        Command::request_from_shell(request).then_send(move |end| Event::ActionEnded { id, end }),
    );
}

/// An update of the roll ended. `None`: `id` is not the roll's.
pub(super) fn action_ended(
    model: &mut Model,
    id: RequestId,
    end: &ActionEnd,
) -> Option<Command<Effect, Event>> {
    with_roll(model, id, |model, roll, name, out| {
        if roll.entries.get(&name).map(|e| &e.step) != Some(&Step::Updating) {
            return;
        }
        let describe = || {
            let mut text = end.result.label().to_owned();
            if let Some(code) = end.exit_code {
                let _ = write!(text, ", exit {code}");
            }
            if let Some(last) = end.summary.last() {
                let _ = write!(text, ": {last}");
            }
            if let Some(why) = &end.explanation {
                let _ = write!(text, ". {why}");
            }
            text
        };
        match end.result {
            ActionResult::Succeeded => {
                // What the update changed: probe and evaluate the machine again, whatever the
                // wait below finds.
                out.extend(super::act::refresh_machine(model, &name));
                start_wait(model, roll, &name, out);
            }
            ActionResult::Refused => {
                let why = end.summary.last().cloned().unwrap_or_else(describe);
                end_member(model, roll, &name, Outcome::Refused(why), out);
            }
            ActionResult::Failed | ActionResult::TimedOut => {
                // A failed deploy may have switched anyway; the row is refreshed, as for `u`.
                out.extend(super::act::refresh_machine(model, &name));
                end_member(model, roll, &name, Outcome::Failed(describe()), out);
            }
            ActionResult::Cancelled => {
                out.extend(super::act::refresh_machine(model, &name));
                end_member(model, roll, &name, Outcome::Cancelled, out);
            }
        }
    })
}

/// The update is over: wait for the machine to run the new system, be healthy and (Kubernetes)
/// have its node `Ready`.
fn start_wait(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let Some(entry) = roll.entries.get(name) else {
        return;
    };
    let run = entry.log;
    let node = entry.member.kubernetes.then(|| name.to_owned());
    let expect = entry
        .path
        .as_deref()
        .and_then(|p| auspex_domain::observe::StorePath::new(p).ok());
    let machine = model.rows.iter().find(|r| r.machine.name == name);
    let target = machine
        .and_then(|r| r.target.as_ref().ok())
        .map(|t| t.target.clone());
    let class = machine.map(|r| r.machine.class);
    let (Some(expect), Some(target), Some(class)) = (expect, target, class) else {
        return end_member(
            model,
            roll,
            name,
            Outcome::Failed("updated, but there is nothing to check its health against".to_owned()),
            out,
        );
    };
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    if let Some(e) = roll.entries.get_mut(name) {
        e.step = Step::Waiting;
    }
    if let Some(run) = run {
        out.push(say(
            run,
            format!(
                "updated; waiting for {name} to be healthy{} (up to {}s)",
                if node.is_some() {
                    " and its node Ready"
                } else {
                    ""
                },
                roll.ready_timeout_seconds
            ),
        ));
    }
    out.push(
        Command::request_from_shell(AwaitRequest {
            id,
            log: run,
            machine: name.to_owned(),
            class,
            target,
            expect,
            node,
            timeout_seconds: roll.ready_timeout_seconds,
        })
        .then_send(move |answer| Event::AwaitAnswered { id, answer }),
    );
}

/// The wait for health ended. `None`: `id` is not the roll's.
pub(super) fn awaited(
    model: &mut Model,
    id: RequestId,
    answer: &AwaitAnswer,
) -> Option<Command<Effect, Event>> {
    with_roll(model, id, |model, roll, name, out| {
        if roll.entries.get(&name).map(|e| &e.step) != Some(&Step::Waiting) {
            return;
        }
        let outcome = match answer {
            Ok(()) => Outcome::Done,
            Err(why) => Outcome::Failed(format!("not healthy after the update: {why}")),
        };
        end_member(model, roll, &name, outcome, out);
    })
}

/// A call to the cluster ended: the plan's look at it, or a drain or uncordon of the roll.
pub(super) fn kube_answered(
    model: &mut Model,
    id: RequestId,
    answer: &KubeAnswer,
) -> Option<Command<Effect, Event>> {
    if let Some(command) = cluster_answered(model, id, answer) {
        return Some(command);
    }
    with_roll(model, id, |model, roll, name, out| {
        let Some(step) = roll.entries.get(&name).map(|e| e.step.clone()) else {
            return;
        };
        match step {
            Step::Draining { cancelling } => match answer {
                Ok(KubeReply::Done) if !cancelling && !roll.stopping() => {
                    if let Some(run) = roll.entries.get(&name).and_then(|e| e.log) {
                        out.push(say(run, format!("{name} is drained")));
                    }
                    start_update(model, roll, &name, out);
                }
                // A quit raced the drain's end: the node is drained and cordoned, nothing
                // updates, and the node is given back.
                Ok(KubeReply::Done | KubeReply::Cancelled) => {
                    end_member(model, roll, &name, Outcome::Cancelled, out);
                }
                Ok(KubeReply::Nodes(_)) => end_member(
                    model,
                    roll,
                    &name,
                    Outcome::Failed("drain failed: kubectl answered with a node list".to_owned()),
                    out,
                ),
                Err(why) => end_member(
                    model,
                    roll,
                    &name,
                    Outcome::Failed(format!("drain failed: {why}")),
                    out,
                ),
            },
            Step::Uncordoning { then } => {
                let uncordoned = matches!(answer, Ok(KubeReply::Done));
                if let Some(e) = roll.entries.get_mut(&name) {
                    e.cordoned = !uncordoned;
                }
                if uncordoned {
                    if let Some(run) = roll.entries.get(&name).and_then(|e| e.log) {
                        out.push(say(run, format!("{name} is uncordoned")));
                    }
                    return finalize(roll, &name, then, out);
                }
                let why = match answer {
                    Err(why) => why.clone(),
                    _ => "the request was cancelled".to_owned(),
                };
                let note = format!(
                    "node {name} may still be cordoned: could not uncordon it ({why}); uncordon \
                     it by hand (kubectl uncordon {name})"
                );
                let outcome = match then {
                    Outcome::Done => Outcome::Failed(format!("updated, but {note}")),
                    Outcome::Failed(f) => Outcome::Failed(format!("{f}; {note}")),
                    Outcome::Cancelled => Outcome::Failed(format!("cancelled; {note}")),
                    other => Outcome::Failed(format!("{}; {note}", other.line())),
                };
                finalize(roll, &name, outcome, out);
            }
            _ => {}
        }
    })
}

/// Ends machine `name` as `outcome`; a node auspex cordoned is uncordoned first.
fn end_member(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    outcome: Outcome,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let cordoned = roll.entries.get(name).is_some_and(|e| e.cordoned);
    if !cordoned {
        return finalize(roll, name, outcome, out);
    }
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    let run = roll.entries.get(name).and_then(|e| e.log);
    if let Some(e) = roll.entries.get_mut(name) {
        e.step = Step::Uncordoning { then: outcome };
    }
    if let Some(run) = run {
        out.push(say(run, format!("uncordoning node {name}")));
    }
    out.push(
        Command::request_from_shell(KubeRequest {
            id,
            log: run,
            op: KubeOp::Uncordon {
                node: name.to_owned(),
            },
        })
        .then_send(move |answer| Event::KubeAnswered { id, answer }),
    );
}

/// The machine is over: its line in the roll's run, its own run closed, and the rest of its
/// rolling group skipped when it failed.
fn finalize(roll: &mut Roll, name: &str, outcome: Outcome, out: &mut Vec<Command<Effect, Event>>) {
    let Some(entry) = roll.entries.get_mut(name) else {
        return;
    };
    let line = outcome.line();
    let stops_group = matches!(outcome, Outcome::Failed(_) | Outcome::Cancelled);
    entry.step = Step::Ended(outcome);
    let (group, run) = (entry.group, entry.log);
    roll.ids.retain(|_, n| n != name);
    out.push(say(roll.log, format!("{name}: {line}")));
    if let Some(run) = run {
        out.push(log(run, LogOp::End { text: line }));
    }
    let Some(g) = group else { return };
    roll.groups[g].busy = None;
    if stops_group && !roll.groups[g].stopped {
        roll.groups[g].stopped = true;
        let rest: Vec<String> = roll.groups[g].queue.drain(..).collect();
        let label = roll.groups[g].label.clone();
        for next in rest {
            let why = format!("{name} did not finish, so the rest of {label} was not started");
            finalize(roll, &next, Outcome::Skipped(why), out);
        }
    }
}

/// Runs `f` for the machine whose request `id` was, with the roll taken out of the model so both
/// can be used, then starts what may start. `None` when `id` is not the roll's.
fn with_roll(
    model: &mut Model,
    id: RequestId,
    f: impl FnOnce(&mut Model, &mut Roll, String, &mut Vec<Command<Effect, Event>>),
) -> Option<Command<Effect, Event>> {
    let name = model.roll.as_ref()?.ids.get(&id)?.clone();
    let mut roll = model.roll.take()?;
    roll.ids.remove(&id);
    let mut out = Vec::new();
    f(model, &mut roll, name, &mut out);
    advance(model, &mut roll, &mut out);
    model.roll = Some(roll);
    out.push(render());
    Some(Command::all(out))
}

// ---------------------------------------------------------------------------------------
// Quitting
// ---------------------------------------------------------------------------------------

/// The quit question while a roll runs.
pub(super) fn quit_question(roll: &Roll) -> String {
    let running = roll.running_names();
    let doing = if running.is_empty() {
        "waiting for its turn".to_owned()
    } else {
        running.join(", ")
    };
    format!(
        "an update of all machines is running ({} of {} over; now: {doing}); quit and stop it? \
         Nothing new starts, running updates are cancelled and a node auspex cordoned is \
         uncordoned (an activation already under way on a machine may complete) y/N",
        roll.entries
            .values()
            .filter(|e| matches!(e.step, Step::Ended(_)))
            .count(),
        roll.entries.len()
    )
}

/// Stops the roll: nothing new starts, running work is cancelled, and a node auspex cordoned is
/// uncordoned before its machine ends. With `quit`, [`ViewModel::quit`](crate::ViewModel::quit)
/// becomes true when the last machine has ended.
pub(super) fn stop(model: &mut Model, quit: bool) -> Vec<Command<Effect, Event>> {
    let Some(mut roll) = model.roll.take() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if roll.active() {
        roll.status = Status::Stopping { quit };
        out.push(say(roll.log, "stopping: the operator quit"));
        let names: Vec<String> = roll.entries.keys().cloned().collect();
        for name in names {
            let step = roll.entries[&name].step.clone();
            let ids: Vec<RequestId> = roll
                .ids
                .iter()
                .filter(|(_, n)| **n == name)
                .map(|(id, _)| *id)
                .collect();
            match step {
                Step::Queued => {
                    roll.free.retain(|n| *n != name);
                    for g in &mut roll.groups {
                        g.queue.retain(|n| *n != name);
                    }
                    finalize(
                        &mut roll,
                        &name,
                        Outcome::Skipped("the operator quit before its turn".to_owned()),
                        &mut out,
                    );
                }
                // Only the checks and the wait are dropped by the shell without an answer:
                // nothing is left to wait for, so the machine ends here.
                Step::Checking | Step::Waiting => {
                    for id in ids {
                        out.push(Command::notify_shell(CancelRequest { id }).into());
                        roll.ids.remove(&id);
                    }
                    end_member(model, &mut roll, &name, Outcome::Cancelled, &mut out);
                }
                // These two answer a cancellation: the machine ends when they do.
                Step::Draining { .. } => {
                    for id in ids {
                        out.push(Command::notify_shell(CancelRequest { id }).into());
                    }
                    if let Some(e) = roll.entries.get_mut(&name) {
                        e.step = Step::Draining { cancelling: true };
                    }
                }
                Step::Updating => {
                    for id in ids {
                        out.push(Command::notify_shell(CancelRequest { id }).into());
                    }
                }
                Step::Uncordoning { .. } | Step::Ended(_) => {}
            }
        }
        advance(model, &mut roll, &mut out);
    }
    model.roll = Some(roll);
    out
}

/// A second quit while the first is still stopping the roll: leave now, cancelling even the
/// uncordons.
pub(super) fn force(model: &mut Model) -> Vec<Command<Effect, Event>> {
    let Some(roll) = model.roll.as_mut() else {
        return Vec::new();
    };
    roll.ids
        .keys()
        .map(|id| Command::notify_shell(CancelRequest { id: *id }).into())
        .collect()
}
