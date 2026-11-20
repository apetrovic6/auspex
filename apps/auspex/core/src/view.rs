//! What a shell draws, and the pure view state behind it: selection, search, help, quit.

use std::collections::BTreeSet;

use auspex_domain::report::FleetReport;

use crate::effects::{ActionEnd, ActionKind, Generation};

/// Everything a shell draws, and nothing it has to compute.
///
/// The machine rows are [`FleetReport::machines`]: the same values `--json` prints, so the
/// TUI and headless output agree by construction rather than by two renderings of one model.
/// Indices below ([`ViewModel::selected`], [`SearchView::matches`]) index that list.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewModel {
    /// The fleet report: per machine, the domain's [`MachineReport`]
    /// (`auspex_domain::report::machine_report`), sorted by name, plus the shared-IP list.
    /// `--json` prints exactly this. Fleet-level failures (the machine list failed or timed
    /// out, the evaluator could not run) are its `errors`; there is no second copy of them
    /// here. Its `settled` is the domain's, "the scan completed cleanly": the core tells it
    /// which fleet-wide requests (list, evaluator, Prometheus, address resolution) and which
    /// per-machine ones (cache lookups, Prometheus for the row's health) are still out. It
    /// therefore equals [`RefreshView::settled`] at every step, except that it stays `false`
    /// when the deadline cut the scan short or a fleet error is present. Headless mode waits
    /// for [`RefreshView::settled`].
    ///
    /// [`MachineReport`]: auspex_domain::report::MachineReport
    pub report: FleetReport,
    /// Where the current refresh is.
    pub refresh: RefreshView,
    /// Whether the Prometheus fallback is in effect, for the status bar.
    pub prometheus: PrometheusStatus,
    /// Whether a binary cache is configured (`cache.url`), so a shell can say which of the two
    /// reasons a preview is `not_cached`: the cache does not have the pending closure, or there
    /// is no cache to ask.
    pub binary_cache: bool,
    /// The selected row, or `None` when there are no rows.
    pub selected: Option<usize>,
    /// The search, when a query is set.
    pub search: Option<SearchView>,
    /// Whether the help overlay is open.
    pub help_open: bool,
    /// The user asked to quit and nothing is left running. The shell exits once it has drawn
    /// this: every outstanding request has been cancelled, and an action that was running has
    /// been killed, reaped and answered.
    pub quit: bool,
    /// Observe or act mode.
    pub mode: Mode,
    /// The action in progress, or the last one to end; `None` before the first.
    pub action: Option<ActionView>,
    /// The update of all (`U`) in progress, or the last one to end; `None` before the first.
    pub roll: Option<RollView>,
    /// A question waiting for y or N.
    pub confirm: Option<ConfirmView>,
    /// A one-line refusal or note about the last thing the operator asked for ("act mode is
    /// off (A)"), until their next request.
    pub notice: Option<String>,
    /// Which machines are frozen, and from where.
    pub freeze: FreezeView,
}

/// Which machines are frozen: what the report's `frozen` flags and `f` are drawn from.
///
/// The report says *whether* each machine is frozen; this says *why*, for the detail panel, and
/// whether the answer can be trusted at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreezeView {
    /// Frozen by `machines.<m>.frozen = true` in `config.toml`: `f` cannot undo it.
    pub declared: BTreeSet<String>,
    /// Frozen with `f` (the state file), when it could be read; empty when it could not.
    pub saved: BTreeSet<String>,
    /// Frozen with `f` in this run, but not kept: the save failed, so the file does not hold
    /// them and a restart forgets them. They count as frozen until the operator unfreezes them.
    pub run_only: BTreeSet<String>,
    /// Why the freezes made with `f` are unknown (the state file is corrupt or newer, or there
    /// is no state directory), or `None`. While set, `u` and `f` are refused for every machine.
    pub unknown: Option<String>,
    /// A freeze or unfreeze is being saved.
    pub saving: bool,
}

impl FreezeView {
    /// Whether `machine` is known to be frozen, by any source. While [`FreezeView::unknown`] is
    /// set this says nothing about a machine frozen with `f`: ask [`FreezeView::frozen`], or
    /// refuse outright.
    #[must_use]
    pub fn is_frozen(&self, machine: &str) -> bool {
        self.declared.contains(machine)
            || self.saved.contains(machine)
            || self.run_only.contains(machine)
    }

    /// Every machine known to be frozen, which `U` skips, or `None` while it is not known which
    /// machines were frozen with `f` ([`FreezeView::unknown`]). `None` is not "none are frozen":
    /// a `U` that updates the rest then would update the machine the operator froze, so it must
    /// refuse outright, as `u` does (`act::check`).
    #[must_use]
    pub fn frozen(&self) -> Option<BTreeSet<&str>> {
        self.unknown.is_none().then(|| {
            self.declared
                .iter()
                .chain(&self.saved)
                .chain(&self.run_only)
                .map(String::as_str)
                .collect()
        })
    }
}

/// Where a refresh is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshView {
    /// The current refresh. The shell needs it for [`Event::DeadlinePassed`](crate::Event::DeadlinePassed).
    pub generation: Generation,
    /// What is being waited for.
    pub phase: Phase,
    /// Machines still showing a spinner (checking or evaluating).
    pub pending_machines: usize,
    /// Machines whose package preview is still being collected (their closure queries are
    /// out). Not part of `pending_machines`: the status and badges are final already.
    pub diffs_pending: usize,
    /// `Model::fleet_settled`: headless mode prints the report once this is `true`.
    pub settled: bool,
    /// The global deadline passed with something still outstanding, which was then marked as
    /// timed out.
    pub timed_out: bool,
}

/// The stage of a refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No [`Event::Start`](crate::Event::Start) yet.
    NotStarted,
    /// Waiting for the machine list. There are no rows yet.
    ListingMachines,
    /// The rows exist and answers are arriving.
    Scanning,
    /// Nothing more will change until the next refresh.
    Settled,
}

/// The state of the Prometheus source in this refresh.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PrometheusStatus {
    /// Not configured: health comes from ssh.
    #[default]
    NotConfigured,
    /// Configured, but not asked this refresh: the machine list failed, timed out or is
    /// empty, so there was no instance to ask about.
    NotAsked,
    /// Configured and asked (or about to be, once the list arrives), no answer yet: health
    /// comes from ssh meanwhile.
    Pending,
    /// Answered. Machines with a fresh sample use it; the rest fall back to ssh.
    Available,
    /// Failed, or timed out. Not an error: every machine falls back to ssh.
    Unavailable {
        /// What the adapter reported, or that the deadline passed.
        reason: String,
    },
}

/// The search over machine names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchView {
    /// The query as typed.
    pub query: String,
    /// The rows whose name contains it, ignoring case, in row order.
    pub matches: Vec<usize>,
}

/// The interaction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Read-only: every key changes nothing on any machine. The default, never persisted.
    #[default]
    Observe,
    /// Act mode: `u` can update the selected machine. Entered by the operator, after a
    /// confirmation, for the life of this run only; never in headless mode.
    Act,
}

/// Which question is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// Turn act mode on.
    Act,
    /// The checks before an update are running (the shell is reading the fleet repository and
    /// sizing the closure). Any key cancels them; nothing is decided yet.
    Checking,
    /// Update a machine: `y` confirms.
    Update,
    /// Update a machine marked public: the machine's name has to be typed, then Enter.
    UpdateTyped,
    /// The checks refused the update. There is no way to proceed; any key closes the dialog.
    Refused,
    /// Quit while an action runs.
    Quit,
    /// Unfreeze a machine (`f` on a frozen one).
    Unfreeze,
    /// The cluster is being read before an update of all is planned (any key cancels).
    CheckingCluster,
    /// Update every machine with an update pending, as [`ConfirmView::lines`] lays out: `y`
    /// confirms.
    UpdateAll,
}

/// A question the operator has to answer before anything happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmView {
    /// What is being asked.
    pub kind: ConfirmKind,
    /// The whole question. Ends in `y/N` for the questions that `y` answers.
    pub question: String,
    /// Which question this is: every question asked gets a new number, so a shell can tell
    /// that the one it drew is the one it is answering.
    pub serial: u64,
    /// What the checks before an update refused, each a full sentence: the update cannot
    /// proceed while any is here. Shown first.
    pub refusals: Vec<String>,
    /// What the checks before an update found that the operator should read before confirming.
    pub warnings: Vec<String>,
    /// The typed confirmation, when the question asks for one ([`ConfirmKind::UpdateTyped`]).
    pub typed: Option<TypedView>,
    /// What the question is about, one line each: for an update of all, the plan (the order, what
    /// runs in parallel, what is skipped and why). Empty for the other questions.
    pub lines: Vec<String>,
}

/// A name the operator has to type to confirm an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedView {
    /// What has to be typed: the machine's name, exactly.
    pub expected: String,
    /// What has been typed so far.
    pub text: String,
    /// The last Enter did not match `expected`.
    pub mismatch: bool,
}

/// The action in progress, or the last one to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionView {
    /// Update or build.
    pub kind: ActionKind,
    /// The machine.
    pub machine: String,
    /// Where it is.
    pub status: ActionStatus,
}

/// Where an action is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionStatus {
    /// The process is running.
    Running,
    /// The operator asked to stop it; the process group is being killed and reaped.
    Stopping,
    /// Over.
    Ended(ActionEnd),
}

impl ActionView {
    /// Whether the process is still there (running, or being stopped).
    #[must_use]
    pub const fn active(&self) -> bool {
        matches!(self.status, ActionStatus::Running | ActionStatus::Stopping)
    }
}

/// The update of all (`U`) in progress, or the last one to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollView {
    /// Where it is.
    pub status: RollStatus,
    /// Every machine of the plan, in the order the plan lists them (rolling groups first, each in
    /// its own order, then the free machines), with where it is.
    pub machines: Vec<RollMachineView>,
}

impl RollView {
    /// Machines that have ended, however they ended.
    #[must_use]
    pub fn finished(&self) -> usize {
        self.machines.iter().filter(|m| m.state.ended()).count()
    }

    /// Whether anything of it is still running (or being stopped).
    #[must_use]
    pub const fn active(&self) -> bool {
        matches!(self.status, RollStatus::Running | RollStatus::Stopping)
    }
}

/// Where an update of all is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollStatus {
    /// Machines are being updated, or are waiting for their turn.
    Running,
    /// The operator chose to quit: nothing new starts, the running updates are being stopped and
    /// a node auspex cordoned is being uncordoned.
    Stopping,
    /// Every machine has ended.
    Ended,
}

/// One machine of an update of all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollMachineView {
    /// The machine.
    pub name: String,
    /// Where it is.
    pub state: RollState,
}

/// Where one machine of an update of all is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollState {
    /// Waiting for its turn.
    Queued,
    /// The checks before an update are running.
    Checking,
    /// Its node is being drained.
    Draining,
    /// `clan machines update` is running.
    Updating,
    /// Updated; waiting for it (and its node) to be healthy.
    Waiting,
    /// Its node is being uncordoned.
    Uncordoning,
    /// Updated, healthy and (if it was drained) uncordoned.
    Done,
    /// The checks (or the freeze) refused it; it was not touched and the roll went on.
    Refused(String),
    /// It was started and failed: the rest of its rolling group was not started.
    Failed(String),
    /// Not started, because of what happened to another machine, or a quit.
    Skipped(String),
    /// Stopped by the operator.
    Cancelled,
}

impl RollState {
    /// The machine will not change any more.
    #[must_use]
    pub const fn ended(&self) -> bool {
        matches!(
            self,
            Self::Done | Self::Refused(_) | Self::Failed(_) | Self::Skipped(_) | Self::Cancelled
        )
    }

    /// One word for it, for the status bar and the log.
    #[must_use]
    pub const fn word(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Checking => "checking",
            Self::Draining => "draining",
            Self::Updating => "updating",
            Self::Waiting => "waiting for health",
            Self::Uncordoning => "uncordoning",
            Self::Done => "done",
            Self::Refused(_) => "refused",
            Self::Failed(_) => "failed",
            Self::Skipped(_) => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The pure view state, kept by machine *name* so a refresh that reorders or removes rows
/// leaves the selection where the user put it.
#[derive(Debug, Default)]
pub(crate) struct ViewState {
    /// The selected machine's name. Kept while the list is being re-read, so it can be found
    /// again when the new list arrives.
    pub selected: Option<String>,
    pub query: String,
    pub help_open: bool,
    pub quit: bool,
    pub notice: Option<String>,
}

fn matches(name: &str, query: &str) -> bool {
    !query.is_empty() && name.to_lowercase().contains(&query.to_lowercase())
}

impl ViewState {
    /// The selected row in `names` (sorted), if the selected machine is there.
    pub fn index(&self, names: &[&str]) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        names.iter().position(|n| *n == selected)
    }

    /// After the machine list changes: keep the same machine if it is still there; if it
    /// was removed, select the row that took its place (the first name after it), or the
    /// last row; with nothing selected, the first row. An empty list leaves the name alone,
    /// so a refresh that briefly has no rows does not lose it.
    pub fn reanchor(&mut self, names: &[&str]) {
        if names.is_empty() || self.index(names).is_some() {
            return;
        }
        let next = self.selected.as_deref().map_or_else(
            || names.first(),
            |old| names.iter().find(|n| **n > old).or_else(|| names.last()),
        );
        self.selected = next.map(|n| (*n).to_owned());
    }

    fn select(&mut self, names: &[&str], index: usize) {
        if let Some(n) = names.get(index) {
            self.selected = Some((*n).to_owned());
        }
    }

    /// Moves the selection by `delta` rows, clamped to the ends. Nothing happens with no rows.
    pub fn move_by(&mut self, names: &[&str], delta: isize) {
        let Some(last) = names.len().checked_sub(1) else {
            return;
        };
        let current = self.index(names).unwrap_or(0);
        let target = current.saturating_add_signed(delta).min(last);
        self.select(names, target);
    }

    /// Selects the first row.
    pub fn top(&mut self, names: &[&str]) {
        self.select(names, 0);
    }

    /// Selects the last row.
    pub fn bottom(&mut self, names: &[&str]) {
        if let Some(last) = names.len().checked_sub(1) {
            self.select(names, last);
        }
    }

    /// The rows matching the query.
    pub fn matches(&self, names: &[&str]) -> Vec<usize> {
        names
            .iter()
            .enumerate()
            .filter(|(_, n)| matches(n, &self.query))
            .map(|(i, _)| i)
            .collect()
    }

    /// Sets the query and jumps to the first match at or after the selection, wrapping, as
    /// vim's incremental `/` does. No match leaves the selection where it is.
    pub fn search(&mut self, names: &[&str], query: String) {
        self.query = query;
        let found = self.matches(names);
        let current = self.index(names).unwrap_or(0);
        if let Some(&i) = found
            .iter()
            .find(|&&i| i >= current)
            .or_else(|| found.first())
        {
            self.select(names, i);
        }
    }

    /// `n`: the next match after the selection, wrapping to the first.
    pub fn next_match(&mut self, names: &[&str]) {
        let found = self.matches(names);
        let current = self.index(names);
        let next = found
            .iter()
            .find(|&&i| current.is_none_or(|c| i > c))
            .or_else(|| found.first());
        if let Some(&i) = next {
            self.select(names, i);
        }
    }

    /// `N`: the previous match before the selection, wrapping to the last.
    pub fn previous_match(&mut self, names: &[&str]) {
        let found = self.matches(names);
        let current = self.index(names);
        let previous = found
            .iter()
            .rev()
            .find(|&&i| current.is_none_or(|c| i < c))
            .or_else(|| found.last());
        if let Some(&i) = previous {
            self.select(names, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: &[&str] = &["alfrost", "cerberus", "luna", "mars", "sol", "terra"];

    fn at(name: &str) -> ViewState {
        ViewState {
            selected: Some(name.to_owned()),
            ..ViewState::default()
        }
    }

    fn selected(v: &ViewState) -> &str {
        v.selected.as_deref().unwrap()
    }

    #[test]
    fn counts_clamp_at_both_ends() {
        let mut v = at("luna");
        v.move_by(NAMES, 100);
        assert_eq!(selected(&v), "terra");
        v.move_by(NAMES, -100);
        assert_eq!(selected(&v), "alfrost");
        v.move_by(NAMES, isize::MAX);
        assert_eq!(selected(&v), "terra");
        v.move_by(NAMES, isize::MIN);
        assert_eq!(selected(&v), "alfrost");
        v.move_by(NAMES, 2);
        assert_eq!(selected(&v), "luna");
        v.bottom(NAMES);
        assert_eq!(selected(&v), "terra");
        v.top(NAMES);
        assert_eq!(selected(&v), "alfrost");
    }

    #[test]
    fn movement_with_no_rows_does_nothing() {
        let mut v = at("luna");
        v.move_by(&[], 3);
        v.bottom(&[]);
        v.top(&[]);
        assert_eq!(selected(&v), "luna");
    }

    #[test]
    fn reanchor_keeps_the_name_or_takes_the_row_in_its_place() {
        let mut v = at("luna");
        v.reanchor(&["terra", "luna"]);
        assert_eq!(selected(&v), "luna");
        v.reanchor(&["cerberus", "mars", "terra"]);
        assert_eq!(selected(&v), "mars");
        let mut v = at("zeta");
        v.reanchor(&["alpha", "beta"]);
        assert_eq!(selected(&v), "beta");
        let mut v = ViewState::default();
        v.reanchor(&["alpha", "beta"]);
        assert_eq!(selected(&v), "alpha");
        let mut v = at("luna");
        v.reanchor(&[]);
        assert_eq!(selected(&v), "luna");
    }

    #[test]
    fn search_next_and_previous_wrap() {
        // "r" matches alfrost, cerberus, mars, terra.
        let mut v = at("luna");
        v.search(NAMES, "R".to_owned());
        assert_eq!(selected(&v), "mars", "first match at or after the cursor");
        v.next_match(NAMES);
        assert_eq!(selected(&v), "terra");
        v.next_match(NAMES);
        assert_eq!(selected(&v), "alfrost", "wraps to the first");
        v.previous_match(NAMES);
        assert_eq!(selected(&v), "terra", "wraps to the last");
        v.previous_match(NAMES);
        assert_eq!(selected(&v), "mars");
        assert_eq!(v.matches(NAMES), [0, 1, 3, 5]);
    }

    #[test]
    fn a_search_with_no_match_stays_put() {
        let mut v = at("luna");
        v.search(NAMES, "xyz".to_owned());
        v.next_match(NAMES);
        v.previous_match(NAMES);
        assert_eq!(selected(&v), "luna");
        v.search(NAMES, String::new());
        assert!(
            v.matches(NAMES).is_empty(),
            "an empty query matches nothing"
        );
    }
}
