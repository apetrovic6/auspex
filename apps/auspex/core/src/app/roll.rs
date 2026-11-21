//! `U`: update every machine that has an update pending, rolling per tag, Kubernetes-aware.
//!
//! # What the core guarantees
//!
//! - **One confirmation for the whole plan.** `U` collects the machines with an update pending,
//!   leaves out the frozen ones (and refuses outright while it is unknown which are frozen,
//!   exactly as `u` does: [`FreezeView::frozen`](crate::view::FreezeView::frozen) is `None`),
//!   splits the rest by the configured rolling tags ([`auspex_domain::roll::plan`]) and asks one
//!   question that lists the order, what runs at once, what is skipped and the warnings of the
//!   fleet repository (dirty, unpushed), each with its reason, and which cluster (context and
//!   API server) the Kubernetes groups will talk to. Nothing runs before the `y`.
//! - **One auspex updates a fleet at a time.** The `y` takes the fleet's update lock
//!   ([`FleetLockRequest`]); a second `U` or `u` from another auspex is refused while it is held.
//!   It is held for the whole roll.
//! - **Machines that share a rolling tag never update at the same time**, and neither do any two
//!   machines of the groups that use the cluster (one kubeconfig, one etcd). A group is one
//!   machine after another, in name order; machines with no rolling tag update a few at a time
//!   (`update.parallel`).
//! - **Each machine is checked at its turn, as `u` checks it.** The checks of PR #26 run for it
//!   when its turn comes, and the freeze is looked at again, in the core and by the shell right
//!   before the process starts. A refusal **skips that machine and the roll goes on**; a failure
//!   of a started update, a failed drain, a cluster that is no longer healthy, or a machine that
//!   does not become healthy **stops its rolling group** (the rest is skipped) and the others carry
//!   on. A warning about the fleet repository that was not in the confirmed plan (someone edited
//!   the tree after the `y`) refuses the machine. A machine whose name has to be typed
//!   (`public`) is skipped: that confirmation is for `u`.
//! - **Kubernetes.** The cluster is read again before every machine that uses it (and so right
//!   after the `y` for the first of each group): any node that is `NotReady` or cordoned, or
//!   `Ready` with a kubelet silent for more than 25 s against the newest lease of the same
//!   read ([`roll::silent_kubelets`]), stops the group, and a node is drained only when it is
//!   schedulable at that moment, which is what makes it auspex's to uncordon. Every call is
//!   pinned to the context the plan showed. Between machines the roll waits ([`AwaitRequest`])
//!   until the updated one runs the deployed system, has no failed unit it did not already have
//!   when its turn began, and its node is `Ready` with a heartbeat newer than the one it had when
//!   the wait began (its own earlier one: no clock is compared with another).
//! - **A node is put back where it is safe to.** After a failed drain, or a quit before the update
//!   started, the node is uncordoned. After an update or a wait that failed the node **stays
//!   cordoned**: the machine may be broken, and workloads must not return to it unasked. That is
//!   said in the log, the status bar, the action log and on stderr at exit, with the command.
//! - **Quit asks, and stops cleanly.** `q` during a roll asks; on `y` nothing new starts, the
//!   running `clan` processes are cancelled ([`ActionRequest`] is answered when the process
//!   group is gone), a drain in progress is cancelled and its node uncordoned, and
//!   [`ViewModel::quit`](crate::ViewModel::quit) becomes true only when every machine has ended.
//!   A second quit forces (the shell then uncordons what it still can, within a bound).
//! - **While it runs, nothing else changes the world under it**: `u`, `b`, `r` (refresh) and `A`
//!   say that a roll is running, and `U` says so again.
//!
//! The log: the roll has a run of its own in the Log tab (the plan first, one line per machine as
//! it ends, the totals), and each machine has one (its checks' warnings, the drain, `clan`'s
//! output, the wait, the uncordon). A machine that was drained or started has one action-log row
//! (the shell writes it, and its detail says the node was drained and how the machine ended); a
//! machine that was refused or skipped before that was never tried and has none.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use auspex_domain::preflight::TreeState;
use auspex_domain::roll::{self, Candidate, ClusterId, EtcdPod, Group, Member, Node};
use auspex_domain::text::shell_quote;
use crux_core::Command;
use crux_core::render::render;

use super::act::{ACT_MODE_OFF, Confirm, article, check, not_yet, review};
use super::{Event, Model, Pending};
use crate::effects::{
    ActionEnd, ActionKind, ActionResult, AwaitAnswer, AwaitRequest, CancelRequest, Effect,
    EndResult, FleetLockRequest, FleetUnlockRequest, KubeAnswer, KubeOp, KubeReply, KubeRequest,
    LogOp, LogRequest, PreflightAnswer, PreflightRequest, ProbeRequest, RequestId, TreeRequest,
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

    const fn result(&self) -> EndResult {
        match self {
            Self::Done => EndResult::Done,
            Self::Refused(_) => EndResult::Refused,
            Self::Failed(_) => EndResult::Failed,
            Self::Skipped(_) => EndResult::Skipped,
            Self::Cancelled => EndResult::Cancelled,
        }
    }
}

/// Where one machine is in its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Queued,
    /// The checks before an update are running in the shell.
    Checking,
    /// A fresh probe, for the failed units the machine has before anything is done to it.
    Baseline,
    /// The cluster's nodes are being read before the machine's node goes out of service.
    Reading,
    /// The nodes were read and are fine; the etcd pods are being read before the machine's node
    /// goes out of service. Holds the nodes the pods are judged against.
    ReadingEtcd(Vec<Node>),
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
    /// The failed units it had when its turn began.
    baseline: Vec<String>,
    /// auspex cordoned its node (the drain starts with a cordon) and has not uncordoned it.
    cordoned: bool,
    /// Its node was drained: said in its log and its action-log row.
    drained: bool,
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
    /// Its machines use the cluster: at most one of all such groups at a time.
    cluster: bool,
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
    poll_interval_seconds: u64,
    /// The cluster the plan named; every call after it is pinned to its context.
    context: Option<String>,
    /// The kubeconfig the shell resolved, for the command that gives a node back.
    kubeconfig: Option<String>,
    /// The fleet repository's warnings in the confirmed plan: a machine's turn may show these and
    /// no others.
    tree_warnings: Vec<String>,
    /// The fleet lock, released when the roll is over.
    lock: RequestId,
    /// Nodes left cordoned on purpose, with the command that gives them back.
    left_cordoned: Vec<String>,
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

    /// A machine that uses the cluster is in its turn: no other such machine may start.
    fn cluster_busy(&self) -> bool {
        self.entries
            .values()
            .any(|e| e.member.uses_cluster() && !matches!(e.step, Step::Queued | Step::Ended(_)))
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

    /// The `kubectl` words that name the cluster the roll works in, for the command that gives a
    /// node back.
    fn kubectl_prefix(&self) -> String {
        let mut prefix = "kubectl".to_owned();
        if let Some(k) = &self.kubeconfig {
            let _ = write!(prefix, " --kubeconfig {}", shell_quote(k));
        }
        if let Some(c) = &self.context {
            let _ = write!(prefix, " --context {}", shell_quote(c));
        }
        prefix
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
                        Step::Checking | Step::Baseline | Step::Reading | Step::ReadingEtcd(_) => {
                            RollState::Checking
                        }
                        Step::Draining { .. } => RollState::Draining,
                        Step::Updating => RollState::Updating,
                        Step::Waiting => RollState::Waiting,
                        Step::Uncordoning { .. } => RollState::Uncordoning,
                        Step::Ended(o) => o.state(),
                    },
                })
                .collect(),
            left_cordoned: self.left_cordoned.clone(),
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

/// What `U` is about to ask, after the cluster and the fleet repository were looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Planned {
    groups: Vec<PlannedGroup>,
    free: Vec<String>,
    /// Machines with an update pending that this `U` leaves alone, and why.
    skipped: Vec<(String, String)>,
    parallel: usize,
    ready_timeout_seconds: u64,
    poll_interval_seconds: u64,
    /// The cluster the groups that use it will talk to, when any does.
    cluster: Option<ClusterId>,
    /// What the fleet repository's state warns about, once, for the whole roll.
    tree_warnings: Vec<String>,
    /// Machines that are nodes of the cluster yet roll with no group, or only in a group that does
    /// not use the cluster (its label is the second field), so they update beside the cluster's
    /// group: shown in the plan, one warning each. Not held to the plan at a machine's turn (the
    /// cluster does not change under it the way the repository does).
    ungrouped_nodes: Vec<(String, Option<String>)>,
}

impl Planned {
    pub(super) fn machines(&self) -> usize {
        self.groups
            .iter()
            .map(|g| g.group.members.len())
            .sum::<usize>()
            + self.free.len()
    }

    /// The question's body: the order, what runs at once, what is skipped, the warnings.
    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "update {} machine{}:",
            self.machines(),
            if self.machines() == 1 { "" } else { "s" }
        )];
        if let Some(c) = &self.cluster {
            lines.push(format!(
                "cluster: context {} ({}); every kubectl call is pinned to it",
                c.context, c.server
            ));
        }
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
            if group.etcd() {
                how.push(
                    "every etcd member must be healthy before each machine goes out, and its etcd \
                     Ready again after"
                        .to_owned(),
                );
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
        lines.push(self.concurrency());
        for (name, why) in &self.skipped {
            lines.push(format!("skipped {name}: {why}"));
        }
        for w in &self.tree_warnings {
            lines.push(format!("warning: {w}"));
        }
        for (name, group) in &self.ungrouped_nodes {
            lines.push(group.as_ref().map_or_else(
                || {
                    format!(
                        "warning: {name} is a node of the cluster but rolls with no group, so it \
                         updates in parallel with the cluster's group (give it a rolling tag to \
                         roll it one at a time)"
                    )
                },
                |group| {
                    format!(
                        "warning: {name} is a node of the cluster but rolls in group {group}, \
                         which does not use the cluster, so it updates in parallel with the \
                         cluster's group (give that group `kubernetes = true`, or the machine a \
                         tag that does)"
                    )
                },
            ));
        }
        lines.push(
            "each machine is checked at its turn: a refusal skips it, a failure stops its \
             rolling group, and a new warning about the fleet repository refuses it"
                .to_owned(),
        );
        lines
    }

    /// What can be out of service at the same moment, said plainly.
    fn concurrency(&self) -> String {
        let cluster = self
            .groups
            .iter()
            .filter(|g| g.group.uses_cluster())
            .count();
        let others: Vec<String> = self
            .groups
            .iter()
            .filter(|g| !g.group.uses_cluster())
            .map(|g| format!("one of {}", g.group.label()))
            .collect();
        let free = self.parallel.min(self.free.len());
        let mut parts = others;
        if cluster > 0 {
            parts.push(if cluster == 1 {
                "one of the cluster's group".to_owned()
            } else {
                format!("one of the {cluster} cluster groups together (they share one cluster)")
            });
        }
        if free > 0 {
            parts.push(format!("{free} of the machines in parallel"));
        }
        let at_once = parts.len() + free.saturating_sub(1);
        format!(
            "at most {at_once} machine{} updating at once: {}",
            if at_once == 1 { "" } else { "s" },
            parts.join(" + ")
        )
    }
}

/// Which look the question is waiting for: the cluster, then its nodes, then the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Context,
    Nodes,
    Etcd,
    Tree,
}

/// `U` before everything it needs to ask has been answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Draft {
    plan: roll::Plan,
    skipped: Vec<(String, String)>,
    parallel: usize,
    ready_timeout_seconds: u64,
    poll_interval_seconds: u64,
    cluster: Option<Result<ClusterId, String>>,
    nodes: Option<Result<Vec<Node>, String>>,
    etcd: Option<Result<Vec<EtcdPod>, String>>,
    tree: Option<Result<TreeState, String>>,
}

impl Draft {
    fn context(&self) -> Option<String> {
        match &self.cluster {
            Some(Ok(c)) => Some(c.context.clone()),
            _ => None,
        }
    }

    /// Applies what was found to every group that uses the cluster: one the cluster refuses is not
    /// rolled, and its machines are listed as skipped.
    fn finish(mut self, unavailable: Option<&str>) -> Planned {
        let mut groups = Vec::new();
        for group in std::mem::take(&mut self.plan.groups) {
            if group.uses_cluster() {
                let why = match (unavailable, &self.cluster, &self.nodes) {
                    (Some(why), _, _) => Some(format!("kubernetes is not usable here: {why}")),
                    (None, Some(Err(why)), _) | (None, _, Some(Err(why))) => {
                        Some(format!("cannot read the cluster: {why}"))
                    }
                    (None, Some(Ok(_)), Some(Ok(nodes))) => {
                        let members: Vec<&Member> = group.members.iter().collect();
                        let mut why = roll::cluster_refusals(nodes, &members);
                        if group.etcd() && why.is_empty() {
                            match &self.etcd {
                                Some(Ok(pods)) => why.extend(roll::etcd_unhealthy(nodes, pods)),
                                Some(Err(e)) => why.push(format!("cannot read etcd: {e}")),
                                None => why.push("etcd was not asked".to_owned()),
                            }
                        }
                        (!why.is_empty()).then(|| why.join("; "))
                    }
                    _ => Some("the cluster was not asked".to_owned()),
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
        let cluster = groups
            .iter()
            .any(|g| g.group.uses_cluster())
            .then(|| match &self.cluster {
                Some(Ok(c)) => Some(c.clone()),
                _ => None,
            })
            .flatten();
        // A machine that is a node of the cluster and rolls with no group, or only in a group that
        // does not use the cluster, would go out of service beside the cluster group's machine,
        // which is what the group exists to prevent. The node list is already in hand, so say so.
        let ungrouped_nodes = match &self.nodes {
            Some(Ok(nodes)) if groups.iter().any(|g| g.group.uses_cluster()) => {
                let is_node = |name: &str| nodes.iter().any(|n| n.name == name);
                let mut found: Vec<(String, Option<String>)> = groups
                    .iter()
                    .filter(|g| !g.group.uses_cluster())
                    .flat_map(|g| {
                        g.group
                            .members
                            .iter()
                            .filter(|m| is_node(&m.name))
                            .map(|m| (m.name.clone(), Some(g.group.label())))
                    })
                    .chain(
                        self.plan
                            .free
                            .iter()
                            .filter(|f| is_node(f))
                            .map(|f| (f.clone(), None)),
                    )
                    .collect();
                found.sort();
                found
            }
            _ => Vec::new(),
        };
        let tree_warnings = auspex_domain::preflight::tree_warnings(match &self.tree {
            Some(Ok(t)) => Ok(t),
            Some(Err(why)) => Err(why.as_str()),
            None => Err("the fleet repository was not looked at"),
        });
        Planned {
            groups,
            free: self.plan.free,
            skipped: self.skipped,
            parallel: self.parallel,
            ready_timeout_seconds: self.ready_timeout_seconds,
            poll_interval_seconds: self.poll_interval_seconds,
            cluster,
            tree_warnings,
            ungrouped_nodes,
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
    if model.roll_active() || model.lock_wait.is_some() {
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
    let draft = Draft {
        plan,
        skipped,
        parallel: config.rolling.parallel.max(1),
        ready_timeout_seconds: config.rolling.ready_timeout_seconds,
        poll_interval_seconds: config.rolling.poll_interval_seconds,
        cluster: None,
        nodes: None,
        etcd: None,
        tree: None,
    };

    let mut commands = super::act::drop_confirm(model);
    let cluster = draft.plan.groups.iter().any(Group::uses_cluster)
        && config.rolling.kubernetes_unavailable.is_none();
    // Which cluster, then its nodes, then the repository: each look is its own request, and the
    // question opens when the last is in.
    commands.push(ask_stage(
        model,
        draft,
        if cluster { Stage::Context } else { Stage::Tree },
    ));
    Command::all(commands)
}

/// Opens the checking dialog for `stage` and asks the shell for what it needs.
fn ask_stage(model: &mut Model, draft: Draft, stage: Stage) -> Command<Effect, Event> {
    let id = model.fresh_id();
    let context = draft.context();
    let (flake, timeout_seconds) = model.config.as_ref().map_or_else(
        || (String::new(), 0),
        |c| (c.flake.clone(), c.preflight_timeout_seconds),
    );
    model.ask(Confirm::CheckingCluster {
        id,
        draft: Box::new(draft),
        stage,
    });
    let asked = match stage {
        Stage::Context | Stage::Nodes | Stage::Etcd => Command::request_from_shell(KubeRequest {
            id,
            log: None,
            context,
            op: match stage {
                Stage::Context => KubeOp::Context,
                Stage::Nodes => KubeOp::Nodes,
                _ => KubeOp::EtcdPods,
            },
        })
        .then_send(move |answer| Event::KubeAnswered { id, answer }),
        Stage::Tree => Command::request_from_shell(TreeRequest {
            id,
            flake,
            timeout_seconds,
        })
        .then_send(move |answer| Event::TreeChecked { id, answer }),
    };
    Command::all([asked, render()])
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

/// What a look at the world answered, for [`stage_answered`].
enum Found<'a> {
    Kube(&'a KubeAnswer),
    Tree(&'a Result<TreeState, String>),
}

/// An answer to the plan's own questions. `None`: it is not the one the dialog waits for.
fn stage_answered(
    model: &mut Model,
    id: RequestId,
    found: &Found<'_>,
) -> Option<Command<Effect, Event>> {
    let Some(Confirm::CheckingCluster {
        id: waiting, stage, ..
    }) = &model.confirm
    else {
        return None;
    };
    if *waiting != id {
        return None;
    }
    let stage = *stage;
    let Some(Confirm::CheckingCluster { mut draft, .. }) = model.confirm.take() else {
        return None;
    };
    let unavailable = model
        .config
        .as_ref()
        .and_then(|c| c.rolling.kubernetes_unavailable.clone());
    match (stage, found) {
        (Stage::Context, Found::Kube(answer)) => {
            let cluster = match answer {
                Ok(KubeReply::Cluster(c)) => Ok(c.clone()),
                Ok(_) => Err(
                    "the shell answered the question which cluster with something else".to_owned(),
                ),
                Err(why) => Err(why.clone()),
            };
            let next = if cluster.is_ok() {
                Stage::Nodes
            } else {
                Stage::Tree
            };
            draft.cluster = Some(cluster);
            Some(ask_stage(model, *draft, next))
        }
        (Stage::Nodes, Found::Kube(answer)) => {
            draft.nodes = Some(match answer {
                Ok(KubeReply::Nodes(nodes)) => Ok(nodes.clone()),
                Ok(KubeReply::Cancelled) => Err("the request was cancelled".to_owned()),
                Ok(_) => Err("the shell answered a node list with something else".to_owned()),
                Err(why) => Err(why.clone()),
            });
            let next = if draft.plan.groups.iter().any(Group::etcd)
                && matches!(draft.nodes, Some(Ok(_)))
            {
                Stage::Etcd
            } else {
                Stage::Tree
            };
            Some(ask_stage(model, *draft, next))
        }
        (Stage::Etcd, Found::Kube(answer)) => {
            draft.etcd = Some(match answer {
                Ok(KubeReply::EtcdPods(pods)) => Ok(pods.clone()),
                Ok(KubeReply::Cancelled) => Err("the request was cancelled".to_owned()),
                Ok(_) => Err("the shell answered a pod list with something else".to_owned()),
                Err(why) => Err(why.clone()),
            });
            Some(ask_stage(model, *draft, Stage::Tree))
        }
        (Stage::Tree, Found::Tree(tree)) => {
            draft.tree = Some((*tree).clone());
            let planned = draft.finish(unavailable.as_deref());
            Some(open_question(model, planned))
        }
        // The shell answered something that was not asked at this stage: leave the dialog as
        // it was (it is cancelled by any key).
        _ => None,
    }
}

/// The fleet repository's answer, for the plan.
pub(super) fn tree_checked(
    model: &mut Model,
    id: RequestId,
    answer: &Result<TreeState, String>,
) -> Option<Command<Effect, Event>> {
    stage_answered(model, id, &Found::Tree(answer))
}

// ---------------------------------------------------------------------------------------
// The lock
// ---------------------------------------------------------------------------------------

/// What is waiting for the fleet's update lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LockWait {
    /// A `u` of this machine, confirmed.
    Update { id: RequestId, machine: String },
    /// A `U`, confirmed.
    Roll {
        id: RequestId,
        planned: Box<Planned>,
    },
}

impl LockWait {
    const fn id(&self) -> RequestId {
        match self {
            Self::Update { id, .. } | Self::Roll { id, .. } => *id,
        }
    }
}

fn lock_command(id: RequestId, what: String) -> Command<Effect, Event> {
    Command::request_from_shell(FleetLockRequest { id, what })
        .then_send(move |answer| Event::FleetLockAnswered { id, answer })
}

/// A `u` has passed every rule and the last look at the freeze file: take the fleet's lock, then
/// launch.
pub(super) fn lock_for_update(model: &mut Model, machine: String) -> Command<Effect, Event> {
    let id = model.fresh_id();
    let what = format!("update {machine}");
    model.lock_wait = Some(LockWait::Update { id, machine });
    Command::all([lock_command(id, what), render()])
}

/// The shell's answer about the lock.
pub(super) fn lock_answered(
    model: &mut Model,
    id: RequestId,
    answer: &Result<(), String>,
) -> Command<Effect, Event> {
    let Some(wait) = model.lock_wait.take_if(|w| w.id() == id) else {
        return Command::done();
    };
    if let Err(why) = answer {
        return model.notice(format!("not updating: {why}"));
    }
    let unlock =
        || -> Command<Effect, Event> { Command::notify_shell(FleetUnlockRequest { id }).into() };
    // Quit while the lock was being taken: nothing is started on the way out.
    if model.view.quit || !model.act {
        return Command::all([unlock(), render()]);
    }
    match wait {
        LockWait::Update { machine, .. } => {
            // The world may have moved while the lock was taken: decided again.
            if let Err(why) = check(model, ActionKind::Update, &machine) {
                return Command::all([unlock(), model.notice(why)]);
            }
            super::act::launch_locked(model, machine, id)
        }
        LockWait::Roll { planned, .. } => start(model, &planned, id),
    }
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

/// The operator confirmed the plan: the fleet's lock is taken, and then the roll starts. The
/// world may have moved while the question was open, so the guards are decided again; each
/// machine is decided again at its turn anyway.
pub(super) fn confirmed(model: &mut Model, planned: &Planned) -> Command<Effect, Event> {
    if !model.act {
        return model.notice(ACT_MODE_OFF);
    }
    if model.roll_active()
        || model.lock_wait.is_some()
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
    let id = model.fresh_id();
    model.lock_wait = Some(LockWait::Roll {
        id,
        planned: Box::new(planned.clone()),
    });
    Command::all([lock_command(id, "update all".to_owned()), render()])
}

/// The lock is held: the roll starts.
fn start(model: &mut Model, planned: &Planned, lock: RequestId) -> Command<Effect, Event> {
    let run = model.fresh_id();
    let mut entries = BTreeMap::new();
    let mut order = Vec::new();
    let mut groups = Vec::new();
    let entry = |member: Member, group: Option<usize>| Entry {
        member,
        group,
        log: None,
        step: Step::Queued,
        path: None,
        baseline: Vec::new(),
        cordoned: false,
        drained: false,
    };
    for (index, planned_group) in planned.groups.iter().enumerate() {
        let group = &planned_group.group;
        groups.push(GroupRun {
            label: group.label(),
            queue: group.members.iter().map(|m| m.name.clone()).collect(),
            busy: None,
            stopped: false,
            cluster: group.uses_cluster(),
        });
        for m in &group.members {
            order.push(m.name.clone());
            entries.insert(m.name.clone(), entry(m.clone(), Some(index)));
        }
    }
    for name in &planned.free {
        order.push(name.clone());
        entries.insert(
            name.clone(),
            entry(
                Member {
                    name: name.clone(),
                    kubernetes: false,
                    drain: None,
                    etcd: false,
                },
                None,
            ),
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
        poll_interval_seconds: planned.poll_interval_seconds,
        context: planned.cluster.as_ref().map(|c| c.context.clone()),
        kubeconfig: planned.cluster.as_ref().map(|c| c.kubeconfig.clone()),
        tree_warnings: planned.tree_warnings.clone(),
        lock,
        left_cordoned: Vec::new(),
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
                // The groups that use the cluster share it: one machine of all of them at a time.
                if roll.groups[g].cluster && roll.cluster_busy() {
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
    let mut text = format!("update all over: {totals}");
    if !roll.left_cordoned.is_empty() {
        let _ = write!(text, "; LEFT CORDONED: {}", roll.left_cordoned.join(", "));
    }
    out.push(log(
        roll.log,
        LogOp::End {
            text: text.clone(),
            result: EndResult::Done,
        },
    ));
    out.push(Command::notify_shell(FleetUnlockRequest { id: roll.lock }).into());
    if quit {
        model.view.quit = true;
    } else {
        model.view.notice = Some(text);
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
            end_member(model, roll, name, Outcome::Refused(why), false, out);
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
            Err(why) => return end_member(model, roll, &name, Outcome::Refused(why), false, out),
        };
        if asked.as_deref() != Some(now.path.as_str()) {
            return end_member(
                model,
                roll,
                &name,
                Outcome::Refused(
                    "its pending system changed while it was being checked".to_owned(),
                ),
                false,
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
                false,
                out,
            );
        }
        if review.typed_confirmation {
            return end_member(
                model,
                roll,
                &name,
                Outcome::Refused("its name has to be typed to update it: use u on it".to_owned()),
                false,
                out,
            );
        }
        // The warnings about the fleet repository were shown in the plan the operator confirmed.
        // One that is not among them is news: someone changed the tree (or its upstream) since,
        // and `clan` deploys the tree as it is now. That machine is not updated on it.
        let tree =
            auspex_domain::preflight::tree_warnings(answer.tree.as_ref().map_err(String::as_str));
        if let Some(new) = tree.iter().find(|w| !roll.tree_warnings.contains(w)) {
            return end_member(
                model,
                roll,
                &name,
                Outcome::Refused(format!(
                    "the fleet repository is not as the confirmed plan found it: {new}"
                )),
                false,
                out,
            );
        }
        for w in review.warnings.iter().filter(|w| !tree.contains(w)) {
            out.push(say(run, format!("warning: {w}")));
        }
        start_baseline(model, roll, &name, out);
    })
}

/// A fresh probe, before anything is done to the machine: which units are already failed.
fn start_baseline(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let machine = model.rows.iter().find(|r| r.machine.name == name);
    let target = machine
        .and_then(|r| r.target.as_ref().ok())
        .map(|t| t.target.clone());
    let class = machine.map(|r| r.machine.class);
    let (Some(target), Some(class)) = (target, class) else {
        return end_member(
            model,
            roll,
            name,
            Outcome::Refused("it has no usable ssh target".to_owned()),
            false,
            out,
        );
    };
    let id = model.fresh_id();
    let generation = model.generation;
    roll.ids.insert(id, name.to_owned());
    if let Some(e) = roll.entries.get_mut(name) {
        e.step = Step::Baseline;
    }
    out.push(
        Command::request_from_shell(ProbeRequest {
            id,
            generation,
            machine: name.to_owned(),
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

/// The baseline probe of a machine of the roll is in. `None`: `id` is not the roll's.
pub(super) fn probed(
    model: &mut Model,
    id: RequestId,
    outcome: &auspex_domain::observe::ProbeOutcome,
) -> Option<Command<Effect, Event>> {
    with_roll(model, id, |model, roll, name, out| {
        if roll.entries.get(&name).map(|e| &e.step) != Some(&Step::Baseline) || roll.stopping() {
            return;
        }
        let run = roll.entries.get(&name).and_then(|e| e.log);
        // A machine that does not answer now is not one to start on: its health afterwards could
        // not be told from its health before.
        if !matches!(outcome, auspex_domain::observe::ProbeOutcome::Observed(_)) {
            let why = match outcome {
                auspex_domain::observe::ProbeOutcome::Unreachable { reason }
                | auspex_domain::observe::ProbeOutcome::Failed { reason } => reason.clone(),
                auspex_domain::observe::ProbeOutcome::Observed(_) => String::new(),
            };
            return end_member(
                model,
                roll,
                &name,
                Outcome::Refused(format!("it does not answer a probe at its turn: {why}")),
                false,
                out,
            );
        }
        let baseline = roll::failed_units(outcome).unwrap_or_default();
        if let (Some(run), false) = (run, baseline.is_empty()) {
            out.push(say(
                run,
                format!("already failed before the update: {}", baseline.join(", ")),
            ));
        }
        if let Some(e) = roll.entries.get_mut(&name) {
            e.baseline = baseline;
        }
        if roll
            .entries
            .get(&name)
            .is_some_and(|e| e.member.uses_cluster())
        {
            start_reading(model, roll, &name, out);
        } else {
            proceed(model, roll, &name, out);
        }
    })
}

/// The cluster's nodes, read again right before this machine's node may go out of service.
fn start_reading(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    let run = roll.entries.get(name).and_then(|e| e.log);
    if let Some(e) = roll.entries.get_mut(name) {
        e.step = Step::Reading;
    }
    out.push(
        Command::request_from_shell(KubeRequest {
            id,
            log: run,
            context: roll.context.clone(),
            op: KubeOp::Nodes,
        })
        .then_send(move |answer| Event::KubeAnswered { id, answer }),
    );
}

/// The etcd pods, read right before this machine's node may go out of service: the nodes were
/// just read and are fine, and `nodes` is what the pods are judged against.
fn start_reading_etcd(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    nodes: Vec<Node>,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    let run = roll.entries.get(name).and_then(|e| e.log);
    if let Some(e) = roll.entries.get_mut(name) {
        e.step = Step::ReadingEtcd(nodes);
    }
    out.push(
        Command::request_from_shell(KubeRequest {
            id,
            log: run,
            context: roll.context.clone(),
            op: KubeOp::EtcdPods,
        })
        .then_send(move |answer| Event::KubeAnswered { id, answer }),
    );
}

/// The drain, or the update itself.
fn proceed(model: &mut Model, roll: &mut Roll, name: &str, out: &mut Vec<Command<Effect, Event>>) {
    match roll.entries.get(name).and_then(|e| e.member.drain) {
        Some(timeout_seconds) => start_drain(model, roll, name, timeout_seconds, out),
        None => start_update(model, roll, name, out),
    }
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
    let context = roll.context.clone();
    let Some(entry) = roll.entries.get_mut(name) else {
        return;
    };
    // `kubectl drain` cordons first and leaves the node cordoned if it fails: from here on the
    // node is ours (it was schedulable a moment ago: the cluster was just read).
    entry.cordoned = true;
    entry.drained = true;
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
            context,
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
        Err(why) => return end_member(model, roll, name, Outcome::Refused(why), false, out),
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
            // Nothing was started, so a drained node is given back.
            ActionResult::Refused => {
                let why = end.summary.last().cloned().unwrap_or_else(describe);
                end_member(model, roll, &name, Outcome::Refused(why), false, out);
            }
            // A failed deploy may have switched anyway: the row is refreshed, as for `u`, and a
            // drained node stays cordoned.
            ActionResult::Failed | ActionResult::TimedOut => {
                out.extend(super::act::refresh_machine(model, &name));
                end_member(model, roll, &name, Outcome::Failed(describe()), true, out);
            }
            ActionResult::Cancelled => {
                out.extend(super::act::refresh_machine(model, &name));
                end_member(model, roll, &name, Outcome::Cancelled, true, out);
            }
        }
    })
}

/// The update is over: wait for the machine to run the new system, have no new failed unit and
/// (Kubernetes) have its node `Ready` with a heartbeat newer than the update.
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
    let etcd = entry.member.etcd;
    let baseline = entry.baseline.clone();
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
            true,
            out,
        );
    };
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    let context = roll.context.clone();
    if let Some(e) = roll.entries.get_mut(name) {
        e.step = Step::Waiting;
    }
    if let Some(run) = run {
        out.push(say(
            run,
            format!(
                "updated; waiting for {name} to be healthy{} (up to {}s)",
                if node.is_some() && etcd {
                    " and its node Ready, with a heartbeat after the update, and its etcd Ready"
                } else if node.is_some() {
                    " and its node Ready, with a heartbeat after the update"
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
            etcd,
            context,
            baseline,
            timeout_seconds: roll.ready_timeout_seconds,
            poll_seconds: roll.poll_interval_seconds,
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
        match answer {
            Ok(()) => end_member(model, roll, &name, Outcome::Done, false, out),
            // The machine took the update and is not well: its node is not put back in service.
            Err(why) => end_member(
                model,
                roll,
                &name,
                Outcome::Failed(format!("not healthy after the update: {why}")),
                true,
                out,
            ),
        }
    })
}

/// A call to the cluster ended: the plan's looks at it, or a read, drain or uncordon of the roll.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per step a call to the cluster can answer for; split, it stops reading as the machine's turn"
)]
pub(super) fn kube_answered(
    model: &mut Model,
    id: RequestId,
    answer: &KubeAnswer,
) -> Option<Command<Effect, Event>> {
    if let Some(command) = stage_answered(model, id, &Found::Kube(answer)) {
        return Some(command);
    }
    with_roll(model, id, |model, roll, name, out| {
        let Some(step) = roll.entries.get(&name).map(|e| e.step.clone()) else {
            return;
        };
        match step {
            Step::Reading => {
                if roll.stopping() {
                    return;
                }
                let nodes = match answer {
                    Ok(KubeReply::Nodes(nodes)) => nodes,
                    Ok(_) => {
                        return end_member(
                            model,
                            roll,
                            &name,
                            Outcome::Failed("the cluster could not be read before it".to_owned()),
                            false,
                            out,
                        );
                    }
                    Err(why) => {
                        return end_member(
                            model,
                            roll,
                            &name,
                            Outcome::Failed(format!(
                                "the cluster could not be read before it: {why}"
                            )),
                            false,
                            out,
                        );
                    }
                };
                if !roll::leases_read(nodes)
                    && let Some(run) = roll.entries.get(&name).and_then(|e| e.log)
                {
                    // `cluster_unhealthy` cannot tell a silent kubelet without them: said, so a
                    // roll that went ahead on a cluster nobody could vouch for is not a mystery.
                    out.push(say(
                        run,
                        format!(
                            "{name}: the node leases could not be read, so whether a kubelet has \
                             gone silent was not checked"
                        ),
                    ));
                }
                let mut why = roll::cluster_unhealthy(nodes);
                if !nodes.iter().any(|n| n.name == name) {
                    why.push(format!("{name} is not a node of the cluster"));
                }
                if why.is_empty() {
                    if roll.entries.get(&name).is_some_and(|e| e.member.etcd) {
                        start_reading_etcd(model, roll, &name, nodes.clone(), out);
                    } else {
                        proceed(model, roll, &name, out);
                    }
                } else {
                    // A cluster that is already short of a node is not the moment to take
                    // another out: not this machine's refusal but the group's stop.
                    end_member(
                        model,
                        roll,
                        &name,
                        Outcome::Failed(format!("the cluster is not healthy: {}", why.join("; "))),
                        false,
                        out,
                    );
                }
            }
            Step::ReadingEtcd(nodes) => {
                if roll.stopping() {
                    return;
                }
                let why = match answer {
                    Ok(KubeReply::EtcdPods(pods)) => {
                        let why = roll::etcd_unhealthy(&nodes, pods);
                        if why.is_empty() {
                            return proceed(model, roll, &name, out);
                        }
                        format!("etcd is not healthy: {}", why.join("; "))
                    }
                    Ok(_) => "etcd could not be read before it".to_owned(),
                    Err(why) => format!("etcd could not be read before it: {why}"),
                };
                // Another member out of service while one is down can cost the quorum: the
                // group's stop, as for a sick cluster.
                end_member(model, roll, &name, Outcome::Failed(why), false, out);
            }
            Step::Draining { cancelling } => match answer {
                Ok(KubeReply::Done) if !cancelling && !roll.stopping() => {
                    if let Some(run) = roll.entries.get(&name).and_then(|e| e.log) {
                        out.push(say(run, format!("{name} is drained")));
                    }
                    start_update(model, roll, &name, out);
                }
                // A quit raced the drain's end: the update has not started, so the node is given
                // back.
                Ok(KubeReply::Done | KubeReply::Cancelled) => {
                    end_member(model, roll, &name, Outcome::Cancelled, false, out);
                }
                Ok(_) => end_member(
                    model,
                    roll,
                    &name,
                    Outcome::Failed(
                        "drain failed: kubectl answered with something else".to_owned(),
                    ),
                    false,
                    out,
                ),
                Err(why) => end_member(
                    model,
                    roll,
                    &name,
                    Outcome::Failed(format!("drain failed: {why}")),
                    false,
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
                let by_hand = format!("{} uncordon {name}", roll.kubectl_prefix());
                roll.left_cordoned.push(name.clone());
                let note = format!(
                    "node {name} may still be cordoned: could not uncordon it ({why}); uncordon \
                     it by hand ({by_hand})"
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

/// Ends machine `name` as `outcome`. A node auspex cordoned is uncordoned first, unless `keep`:
/// the update or the wait failed, the machine may be broken, and it stays cordoned, said in the
/// outcome with the command that gives it back.
fn end_member(
    model: &mut Model,
    roll: &mut Roll,
    name: &str,
    outcome: Outcome,
    keep: bool,
    out: &mut Vec<Command<Effect, Event>>,
) {
    let cordoned = roll.entries.get(name).is_some_and(|e| e.cordoned);
    if !cordoned {
        return finalize(roll, name, outcome, out);
    }
    if keep {
        let by_hand = format!("{} uncordon {name}", roll.kubectl_prefix());
        let note = format!("node {name} is left cordoned (the machine may be broken): {by_hand}");
        roll.left_cordoned.push(name.to_owned());
        let outcome = match outcome {
            Outcome::Failed(f) => Outcome::Failed(format!("{f}; {note}")),
            Outcome::Cancelled => Outcome::Failed(format!("cancelled; {note}")),
            other => other,
        };
        return finalize(roll, name, outcome, out);
    }
    let id = model.fresh_id();
    roll.ids.insert(id, name.to_owned());
    let run = roll.entries.get(name).and_then(|e| e.log);
    let context = roll.context.clone();
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
            context,
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
    let mut line = outcome.line();
    if entry.drained {
        line.push_str(" [node drained first]");
    }
    let result = outcome.result();
    let stops_group = matches!(outcome, Outcome::Failed(_) | Outcome::Cancelled);
    entry.step = Step::Ended(outcome);
    let (group, run) = (entry.group, entry.log);
    roll.ids.retain(|_, n| n != name);
    out.push(say(roll.log, format!("{name}: {line}")));
    if let Some(run) = run {
        out.push(log(run, LogOp::End { text: line, result }));
    }
    let Some(g) = group else { return };
    // Only the machine whose turn it is frees the group: another member may still be running
    // (a quit ends the queued ones while one is being stopped).
    if roll.groups[g].busy.as_deref() == Some(name) {
        roll.groups[g].busy = None;
    }
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
         Nothing new starts and running updates are cancelled; a node drained but not yet \
         updated is uncordoned, a node whose update was stopped stays cordoned and is named \
         (an activation already under way on a machine may complete) y/N",
        roll.entries
            .values()
            .filter(|e| matches!(e.step, Step::Ended(_)))
            .count(),
        roll.entries.len()
    )
}

/// Stops the roll: nothing new starts and running work is cancelled. A node drained but not yet
/// updated is uncordoned before its machine ends; one whose update was under way stays cordoned,
/// said in the log. With `quit`, [`ViewModel::quit`](crate::ViewModel::quit) becomes true when the
/// last machine has ended.
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
                // These are dropped by the shell without an answer (or answered after the
                // machine has ended, which is then ignored): nothing is left to wait for. Only
                // the wait follows an update, so only there is a node already ours and the
                // machine already changed: it stays cordoned.
                Step::Checking
                | Step::Baseline
                | Step::Reading
                | Step::ReadingEtcd(_)
                | Step::Waiting => {
                    for id in ids {
                        out.push(Command::notify_shell(CancelRequest { id }).into());
                        roll.ids.remove(&id);
                    }
                    let keep = step == Step::Waiting;
                    end_member(model, &mut roll, &name, Outcome::Cancelled, keep, &mut out);
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
/// uncordons. The shell then uncordons what is still registered as cordoned by auspex, within a
/// bound, and says which nodes it could not.
pub(super) fn force(model: &mut Model) -> Vec<Command<Effect, Event>> {
    let Some(roll) = model.roll.as_mut() else {
        return Vec::new();
    };
    roll.ids
        .keys()
        .map(|id| Command::notify_shell(CancelRequest { id: *id }).into())
        .collect()
}
