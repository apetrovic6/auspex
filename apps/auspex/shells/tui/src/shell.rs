//! Fulfilling the core's effects: the one place every `Effect` meets its adapter.
//!
//! [`Shell::fulfil`] matches every variant, exhaustively, so a variant added to the core is
//! a compile error here and never a request dropped on the floor. For each request it:
//!
//! - opens a [`CancellationToken`] for the request id, a child of the shell's root token;
//! - starts the adapter call as a task and returns at once, so the UI keeps drawing;
//! - moves the Crux `Request` into that task, which resolves it with the adapter's answer
//!   (the core builds the answer event itself, so an answer cannot reach the wrong row);
//! - forgets the token when the call is over.
//!
//! `Effect::Cancel` cancels that id's token: the adapter kills and reaps whatever it
//! started and returns `Err(Cancelled)`, and the task drops the request unanswered. The
//! evaluation is a stream: resolved once per machine as results arrive, then once with
//! `Finished`, after which the request is dropped. If the evaluator's stream ever ended
//! without a `Finished`, the shell sends one with an error rather than leave the core
//! waiting, so the core's "no result for this machine" rule always gets to run.
//!
//! When a refresh starts (its machine-list request appears), the shell arms the global
//! deadline for that generation: `timeouts.settle` later it sends `DeadlinePassed`.
//!
//! The package preview's two closure queries are the exception: the core leaves them
//! running past that deadline, and the shell bounds each by `timeouts.diff` instead
//! ([`within`]): ssh's approval phase and the query itself count, not the wait for a slot,
//! and when it passes the adapter kills and reaps what it started and the request is answered
//! as a failure, which the core turns into a `failed` preview.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auspex_core::effects::{
    EvalAnswer, Generation, LoadFreezesAnswer, RequestId, SaveFreezeAnswer, ShellEnd, ShellRequest,
};
use auspex_core::{ActionEnd, ActionKind, ActionResult, Auspex, Core, Effect, Event, ViewModel};
use auspex_effects::clan::{ClanRun, RunEnd};
use auspex_effects::deploylog::{
    ActionKind as LoggedKind, ActionLog, ActionResult as LoggedResult, NewAction, OpenParams,
    Outcome,
};
use auspex_effects::freeze::FreezeStore;
use auspex_effects::nix::EvalItem;
use auspex_effects::{CancellationToken, Cancelled};
use crux_core::Request;
use crux_core::capability::Operation;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::{Notify, OnceCell, OwnedSemaphorePermit, Semaphore, oneshot};
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::handoff::Handoff;
use crate::log::Log;
use crate::output::{Output, Stream};

/// How many per-machine calls (probes, cache lookups) may run at once.
///
/// Each probe holds
/// an ssh process and its pipes, so a fleet of thousands would otherwise run out of file
/// descriptors and fail the evaluator and git with it (review C2). Fleet-wide calls (the list,
/// the evaluation, Prometheus, DNS) are not limited: they must not queue behind hung probes.
pub const MAX_CONCURRENT: usize = 32;

/// How many package-preview queries may run at once: 8.
///
/// Each query (deployed or pending) takes one slot of its own; nothing pairs a machine's two.
///
/// A pool of their own, never the one above. A preview holds its permits for up to
/// `timeouts.diff`, and the global deadline deliberately leaves it running, so sharing would let
/// a fleet of slow previews hold every permit past `timeouts.settle` and starve another
/// machine's probe or cache lookup: the deadline would then cut that lookup (the machine
/// loses its `cached` badge) and time the whole scan out.
pub const MAX_PREVIEW_QUERIES: usize = 8;

/// How long the note waits for the fleet to be counted: 500 ms.
///
/// It is sent after the first host starts waiting for the ssh agent, and says how many do.
/// Each host first checks for a live master (milliseconds), so by now every host that needs
/// an approval is counted.
pub const APPROVAL_NOTE_DELAY: Duration = Duration::from_millis(500);

/// How long a shutdown waits for adapter calls to kill and reap what they started.
pub const REAP_GRACE: Duration = Duration::from_secs(5);

/// The core, the adapters behind it, and every request in flight.
pub struct Shell {
    core: Core<Auspex>,
    backend: Backend,
    /// One token per outstanding request id.
    tokens: Mutex<HashMap<RequestId, CancellationToken>>,
    /// The parent of every token: cancelled on quit and on a termination signal.
    root: CancellationToken,
    tasks: TaskTracker,
    /// The log (stderr or the TUI's list).
    pub log: Log,
    /// Signalled after every change to the core's state, for headless mode's wait.
    changed: Notify,
    /// `timeouts.settle`.
    settle: Duration,
    /// `timeouts.diff`, when the package preview is on: the backstop fires this long after
    /// the global deadline ([`Event::PreviewDeadlinePassed`]).
    preview_grace: Mutex<Option<Duration>>,
    /// The current refresh's deadline timer, cancelled when the next refresh arms its own.
    deadline: Mutex<Option<CancellationToken>>,
    /// Machine names the config has `[machines.<name>]` for, and whether a name the fleet
    /// does not have was already reported this run.
    configured: Mutex<(BTreeSet<String>, bool)>,
    /// Permits for per-machine calls ([`MAX_CONCURRENT`]).
    limit: Arc<Semaphore>,
    /// Permits for package-preview queries ([`MAX_PREVIEW_QUERIES`]), never taken from `limit`.
    previews: Arc<Semaphore>,
    approvals: AtomicUsize,
    approval_noted: AtomicBool,
    /// What the Log tab draws: the output of the current and last action.
    output: Output,
    /// How to open the action log, when one is configured.
    action_log_params: Mutex<Option<OpenParams>>,
    /// The action log, opened on first use (`None` inside: it could not be).
    action_log: OnceCell<Option<ActionLog>>,
    /// The last thing auspex could not do besides the action itself, for the status bar.
    warning: Mutex<Option<String>>,
    /// Where `s` asks the TUI loop for the terminal. `None` in every run that has no loop to
    /// give it: headless, and tests of the core's effects.
    handoff: Mutex<Option<UnboundedSender<Handoff>>>,
    /// Where `f` keeps a freeze, when a state file is configured.
    freeze_store: Mutex<Option<FreezeStore>>,
    /// What `f` froze in a run with no state file (the demo, most tests): kept for the run, so
    /// that freezing and unfreezing work as they do with a file.
    frozen_in_memory: Mutex<BTreeSet<String>>,
    /// How many times the freeze state was read for an update (the core's look at the `y` and the
    /// shell's before the spawn), for tests.
    freeze_reads: AtomicUsize,
}

impl Shell {
    /// A shell over `backend`, logging to `log`.
    #[must_use]
    pub fn new(backend: Backend, log: Log, settle: Duration) -> Arc<Self> {
        Arc::new(Self {
            core: Core::new(),
            backend,
            tokens: Mutex::new(HashMap::new()),
            root: CancellationToken::new(),
            tasks: TaskTracker::new(),
            log,
            changed: Notify::new(),
            settle,
            preview_grace: Mutex::new(None),
            deadline: Mutex::new(None),
            configured: Mutex::new((BTreeSet::new(), false)),
            limit: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            previews: Arc::new(Semaphore::new(MAX_PREVIEW_QUERIES)),
            approvals: AtomicUsize::new(0),
            approval_noted: AtomicBool::new(false),
            output: Output::default(),
            action_log_params: Mutex::new(None),
            action_log: OnceCell::new(),
            warning: Mutex::new(None),
            handoff: Mutex::new(None),
            freeze_store: Mutex::new(None),
            frozen_in_memory: Mutex::new(BTreeSet::new()),
            freeze_reads: AtomicUsize::new(0),
        })
    }

    /// The machine names the config has settings for. When the first machine list arrives,
    /// any name it lacks is reported once in the log: a typo there silently drops an override.
    pub fn expect_machines(&self, names: BTreeSet<String>) {
        *lock(&self.configured) = (names, false);
    }

    fn check_configured(&self, listed: &[auspex_domain::fleet::Machine]) {
        let mut configured = lock(&self.configured);
        if configured.1 {
            return;
        }
        configured.1 = true;
        for name in &configured.0 {
            if !listed.iter().any(|m| &m.name == name) {
                self.log.line(&format!(
                    "config: `machines.{name}` names no machine in the fleet, so its settings are unused"
                ));
            }
        }
    }

    /// What to draw.
    #[must_use]
    pub fn view(&self) -> ViewModel {
        self.core.view()
    }

    /// Draws the core view together with the current shell-only connection count.
    pub fn draw(
        &self,
        frame: &mut ratatui::Frame<'_>,
        ui: &mut crate::ui::Ui,
        ctx: &crate::ui::Context<'_>,
    ) {
        ui.approvals = self.approvals();
        ui.warning = self.warning();
        crate::ui::draw(frame, &self.view(), ui, ctx);
    }

    /// Probes currently connecting/authenticating, outside the core's report contract.
    #[must_use]
    pub fn approvals(&self) -> usize {
        self.approvals.load(Ordering::Relaxed)
    }

    fn approval(self: &Arc<Self>, waiting: bool) {
        if waiting {
            self.approvals.fetch_add(1, Ordering::Relaxed);
            if !self.approval_noted.swap(true, Ordering::Relaxed) {
                // One note for the whole round, naming how many hosts are connecting. Every
                // host's phase is counted as it starts, so the count is only the fleet's
                // after a beat; and a connection that is over in that beat needs no note.
                let shell = Arc::clone(self);
                self.tasks.spawn(async move {
                    loop {
                        tokio::select! {
                            () = shell.root.cancelled() => return,
                            () = tokio::time::sleep(APPROVAL_NOTE_DELAY) => {}
                        }
                        match shell.approvals() {
                            0 => {
                                // Nothing to say, and the next round may need a note. A phase
                                // that began between the count and this store saw the flag
                                // still set and spawned no task, so look again: if one is
                                // there and nobody else has taken the note, it is ours.
                                shell.approval_noted.store(false, Ordering::Relaxed);
                                if shell.approvals() == 0
                                    || shell.approval_noted.swap(true, Ordering::Relaxed)
                                {
                                    return;
                                }
                            }
                            1 => {
                                shell.log.line(
                                    "ssh: connecting to 1 host (ssh agent approval may be needed)",
                                );
                                return;
                            }
                            n => {
                                shell.log.line(&format!(
                                    "ssh: connecting to {n} hosts (ssh agent approval may be \
                                     needed; the agent shows prompts one at a time)"
                                ));
                                return;
                            }
                        }
                    }
                });
            }
        } else {
            self.approvals.fetch_sub(1, Ordering::Relaxed);
        }
        self.changed.notify_one();
    }

    /// Sends `event` to the core and fulfils whatever it asks for.
    pub fn send(self: &Arc<Self>, event: Event) {
        let effects = self.core.process_event(event);
        self.fulfil_all(effects);
        self.changed.notify_one();
    }

    /// Waits until something changed since the last call (or returns at once if it did).
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    /// Cancels every request in flight and waits (at most [`REAP_GRACE`]) for the adapters to
    /// kill and reap what they started. Used on quit, after the core has already emitted its
    /// own cancels, and on a signal.
    pub async fn shutdown(&self) {
        self.root.cancel();
        self.tasks.close();
        if tokio::time::timeout(REAP_GRACE, self.tasks.wait())
            .await
            .is_err()
        {
            self.log
                .line("some adapter calls did not finish within the grace period");
        }
    }

    fn fulfil_all(self: &Arc<Self>, effects: Vec<Effect>) {
        for effect in effects {
            self.fulfil(effect);
        }
    }

    /// The mapping, exhaustively.
    fn fulfil(self: &Arc<Self>, effect: Effect) {
        match effect {
            // The TUI's frame timer repaints; headless mode waits on `changed`.
            Effect::Render(_) => {}
            Effect::Inventory(request) => self.inventory(request),
            Effect::Probe(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                let gate = self.gate(&token);
                self.one_shot(op.id, request, async move {
                    let _permit = gate.await?;
                    shell
                        .backend
                        .probe(&op, &token, &|waiting| shell.approval(waiting))
                        .await
                });
            }
            Effect::Evaluate(request) => self.evaluate(request),
            Effect::Prometheus(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                self.one_shot(op.id, request, async move {
                    shell.backend.prometheus(&op, &token).await
                });
            }
            Effect::CacheLookup(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                let gate = self.gate(&token);
                self.one_shot(op.id, request, async move {
                    let _permit = gate.await?;
                    shell.backend.cache_lookup(&op, &token).await
                });
            }
            Effect::ResolveAddresses(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                self.one_shot(op.id, request, async move {
                    shell.backend.resolve(&op, &token).await
                });
            }
            Effect::DeployedClosure(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                self.one_shot(op.id, request, async move {
                    // The wait for a slot is bounded by the core's backstop, not by
                    // `timeouts.diff`, which starts once the query holds one.
                    let _permit = shell.preview_gate(&token).await?;
                    shell.send(Event::PreviewStarted {
                        generation: op.generation,
                        id: op.id,
                    });
                    let timeout = Duration::from_secs(op.timeout_seconds);
                    within(&token, timeout, "deployed closure query", |token| {
                        let shell = Arc::clone(&shell);
                        let op = op.clone();
                        async move {
                            shell
                                .backend
                                .deployed_closure(&op, &token, &|waiting| shell.approval(waiting))
                                .await
                        }
                    })
                    .await
                });
            }
            Effect::PendingClosure(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                self.one_shot(op.id, request, async move {
                    let _permit = shell.preview_gate(&token).await?;
                    shell.send(Event::PreviewStarted {
                        generation: op.generation,
                        id: op.id,
                    });
                    let timeout = Duration::from_secs(op.timeout_seconds);
                    within(&token, timeout, "pending closure query", |token| {
                        let shell = Arc::clone(&shell);
                        let op = op.clone();
                        async move { shell.backend.pending_closure(&op, &token).await }
                    })
                    .await
                });
            }
            Effect::Preflight(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                self.one_shot(op.id, request, async move {
                    shell.backend.preflight(&op, &token).await
                });
            }
            Effect::RunAction(request) => self.run_action(request),
            Effect::OpenShell(request) => self.open_shell(request),
            Effect::SaveFreeze(request) => self.save_freeze(request),
            Effect::LoadFreezes(request) => self.load_freezes(request),
            Effect::Cancel(request) => self.cancel(request.operation.id),
        }
    }

    /// The machine list, which also arms the global deadline for its refresh.
    fn inventory(self: &Arc<Self>, request: Request<auspex_core::effects::InventoryRequest>) {
        let op = request.operation.clone();
        self.arm_deadline(op.generation);
        let token = self.open(op.id);
        let shell = Arc::clone(self);
        self.one_shot(op.id, request, async move {
            let answer = shell.backend.inventory(&op, &token).await;
            if let Ok(Ok(machines)) = &answer {
                shell.check_configured(machines);
            }
            answer
        });
    }

    fn open(&self, id: RequestId) -> CancellationToken {
        let token = self.root.child_token();
        self.lock_tokens().insert(id, token.clone());
        token
    }

    fn forget(&self, id: RequestId) {
        self.lock_tokens().remove(&id);
    }

    fn cancel(&self, id: RequestId) {
        let token = self.lock_tokens().remove(&id);
        if let Some(token) = token {
            token.cancel();
        }
    }

    fn lock_tokens(&self) -> std::sync::MutexGuard<'_, HashMap<RequestId, CancellationToken>> {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The number of tasks still running (adapter calls and deadline timers), for tests.
    #[must_use]
    pub fn tasks(&self) -> usize {
        self.tasks.len()
    }

    /// The number of requests with a live token, for tests.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.lock_tokens().len()
    }

    fn resolve<Op: Operation>(self: &Arc<Self>, request: &mut Request<Op>, output: Op::Output) {
        match self.core.resolve(request, output) {
            Ok(effects) => self.fulfil_all(effects),
            // Only a request that cannot be resolved again (a one-shot resolved twice) gets
            // here, which this shell never does; say so rather than hide it.
            Err(e) => self.log.line(&format!(
                "internal: an answer could not be delivered: {e:?}"
            )),
        }
        self.changed.notify_one();
    }

    /// Runs `work` for a one-shot request and resolves it, unless it was cancelled.
    fn one_shot<Op>(
        self: &Arc<Self>,
        id: RequestId,
        mut request: Request<Op>,
        work: impl Future<Output = Result<Op::Output, Cancelled>> + Send + 'static,
    ) where
        Op: Operation,
        Op::Output: Send,
    {
        let shell = Arc::clone(self);
        self.tasks.spawn(async move {
            let answer = work.await;
            shell.forget(id);
            if let Ok(answer) = answer {
                shell.resolve(&mut request, answer);
            }
            // Cancelled: the request is dropped here, unanswered.
        });
    }

    fn evaluate(self: &Arc<Self>, mut request: Request<auspex_core::effects::EvalRequest>) {
        let op = request.operation.clone();
        let id = op.id;
        let token = self.open(id);
        let shell = Arc::clone(self);
        self.tasks.spawn(async move {
            let (tx, mut rx) = unbounded_channel();
            let work = shell.backend.evaluate(&op, &token, tx);
            tokio::pin!(work);
            let mut outcome = None;
            let mut finished = false;
            loop {
                tokio::select! {
                    item = rx.recv() => match item {
                        Some(EvalItem::Machine { machine, outcome, from_cache }) => {
                            if from_cache {
                                shell.log.line(&format!("eval cache hit: {machine}"));
                            }
                            shell.resolve(&mut request, EvalAnswer::Machine { machine, outcome });
                        }
                        Some(EvalItem::Note(note)) => shell.log.line(&note),
                        Some(EvalItem::Finished { error }) => {
                            finished = true;
                            shell.resolve(&mut request, EvalAnswer::Finished { error });
                        }
                        // Every sender is gone, so the call is over and nothing is queued.
                        None => break,
                    },
                    r = &mut work, if outcome.is_none() => outcome = Some(r),
                }
            }
            let outcome = match outcome {
                Some(o) => o,
                None => work.await,
            };
            shell.forget(id);
            if outcome.is_ok() && !finished {
                shell.resolve(
                    &mut request,
                    EvalAnswer::Finished {
                        error: Some("the evaluator stopped without a final answer".to_owned()),
                    },
                );
            }
            // The stream is over (or cancelled): the request is dropped here.
        });
    }

    /// `timeouts.settle` after a refresh starts, its deadline. The previous refresh's timer
    /// is cancelled (the core would ignore it anyway), so holding `r` does not leave a timer
    /// per press alive for `settle` seconds; the root token stops the last one on shutdown.
    fn arm_deadline(self: &Arc<Self>, generation: Generation) {
        let shell = Arc::clone(self);
        let root = self.root.child_token();
        let old = lock(&self.deadline).replace(root.clone());
        if let Some(old) = old {
            old.cancel();
        }
        let settle = self.settle;
        let grace = *lock(&self.preview_grace);
        self.tasks.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(settle) => shell.send(Event::DeadlinePassed { generation }),
                () = root.cancelled() => return,
            }
            // The core's own backstop for the previews, which the global deadline leaves
            // running: `timeouts.diff` after it, whatever is still out is failed.
            if let Some(grace) = grace {
                tokio::select! {
                    () = tokio::time::sleep(grace) => {
                        shell.send(Event::PreviewDeadlinePassed { generation });
                    }
                    () = root.cancelled() => {}
                }
            }
        });
    }

    /// Turns on the previews' backstop deadline: `timeouts.settle + grace` after a refresh
    /// starts the core fails any preview still out. `grace` is `timeouts.diff`. Call before
    /// the first refresh; without it the previews are bounded by [`within`] alone.
    pub fn set_preview_grace(&self, grace: Duration) {
        *lock(&self.preview_grace) = Some(grace);
    }
}

/// Runs `work` under `timeout`, answering with a failure when it passes.
///
/// `timeouts.diff` bounds one preview query once it holds a slot: ssh's approval phase and
/// the query itself, not only the query's own deadline. (The wait for a slot is not counted:
/// the core's backstop at `settle + diff` bounds it.) `work` gets a
/// child of `token`; on the timeout that child is cancelled, which makes the adapter kill and
/// reap its process group and return `Cancelled`, and that is then answered as the timeout it
/// was. A cancellation from the core (a refresh, a quit) cancels `token` itself, and stays a
/// cancellation: `Err(Cancelled)`, no answer.
async fn within<T, Fut>(
    token: &CancellationToken,
    timeout: Duration,
    what: &str,
    work: impl FnOnce(CancellationToken) -> Fut,
) -> Result<Result<T, String>, Cancelled>
where
    Fut: Future<Output = Result<Result<T, String>, Cancelled>>,
{
    let inner = token.child_token();
    let work = work(inner.clone());
    tokio::pin!(work);
    tokio::select! {
        answer = &mut work => answer,
        () = tokio::time::sleep(timeout) => {
            inner.cancel();
            match work.await {
                Err(Cancelled) if !token.is_cancelled() => {
                    Ok(Err(format!("{what} timed out after {timeout:?}")))
                }
                finished_anyway => finished_anyway,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Actions: `u` and `b`
// ---------------------------------------------------------------------------------------

/// How many of the last lines the ssh explanation reads.
const EXPLAIN_LINES: usize = 40;

impl Shell {
    /// Where `s` asks for the terminal. The TUI loop is the only one that has a terminal to
    /// lend, so only it calls this, before the first key; without it `s` answers that there is
    /// none, which is the headless case and the one the core already refuses.
    pub fn set_handoff(&self, to: UnboundedSender<Handoff>) {
        *lock(&self.handoff) = Some(to);
    }

    /// `Effect::OpenShell`: asks the TUI loop to lend the terminal to ssh, and answers the core
    /// when the loop says ssh is over.
    ///
    /// Every refusal is an answer ([`ShellEnd::NotStarted`]), delivered from a task like every
    /// other answer: the core is told why nothing happened and the status bar says it. The
    /// request is not cancellable (see [`auspex_core::effects::ShellRequest`]); the only thing
    /// that cuts the wait short is the shell shutting down, which drops it unanswered as a
    /// cancelled request is.
    fn open_shell(self: &Arc<Self>, mut request: Request<ShellRequest>) {
        let op = request.operation.clone();
        let started = self.backend.shell_command(&op).and_then(|command| {
            let sender = lock(&self.handoff)
                .clone()
                .ok_or_else(|| "there is no terminal to give ssh in this run".to_owned())?;
            let (reply, answer) = oneshot::channel();
            let (program, args) = command;
            sender
                .send(Handoff {
                    program,
                    args,
                    reply,
                })
                .map_err(|_| "the terminal is gone".to_owned())?;
            Ok(answer)
        });
        let shell = Arc::clone(self);
        self.tasks.spawn(async move {
            let end = match started {
                Err(reason) => ShellEnd::NotStarted { reason },
                Ok(answer) => tokio::select! {
                    ended = answer => ended.unwrap_or_else(|_| ShellEnd::NotStarted {
                        reason: "the terminal went away before ssh ended".to_owned(),
                    }),
                    () = shell.root.cancelled() => return,
                },
            };
            shell.resolve(&mut request, end);
        });
    }

    /// Where the action log is, and who is writing it. Call before the first action; without
    /// it actions are not recorded (the demo, most tests).
    pub fn set_action_log(&self, params: OpenParams) {
        *lock(&self.action_log_params) = Some(params);
    }

    /// The last warning about something auspex could not do besides the action itself (the
    /// action log failed). Shown in the status bar; never the action's result.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        lock(&self.warning).clone()
    }

    fn warn(&self, text: &str) {
        self.log.line(text);
        *lock(&self.warning) = Some(text.to_owned());
        self.changed.notify_one();
    }

    /// How many times the freeze state was read on the way to an update, for tests: two for
    /// an update that went through (the core's look at the `y`, the shell's before the spawn).
    #[must_use]
    pub fn freeze_reads(&self) -> usize {
        self.freeze_reads.load(Ordering::SeqCst)
    }

    /// The streamed output of updates and builds, for the Log tab.
    #[must_use]
    pub const fn output(&self) -> &Output {
        &self.output
    }

    /// A termination signal, or the end of the run: asks the core to stop whatever runs and
    /// quit (without a question: nobody can answer one), then waits for the adapters to kill
    /// and reap what they started and for the action log to be written. The caller exits
    /// after this returns, so a process group is never left behind by a quit.
    pub async fn terminate(self: &Arc<Self>) {
        self.send(Event::Terminate);
        self.shutdown().await;
    }

    /// The action log, opened on first use. `None` when none was configured or it could not be
    /// opened (said once as a warning: the action itself goes ahead regardless).
    async fn action_log(&self) -> Option<ActionLog> {
        self.action_log
            .get_or_init(|| async {
                let params = lock(&self.action_log_params).clone()?;
                match ActionLog::open(params).await {
                    Ok(log) => Some(log),
                    Err(e) => {
                        self.warn(&format!("action log: not logging: {e}"));
                        None
                    }
                }
            })
            .await
            .clone()
    }

    /// Starts the row for an action beside it, and returns where to send how it ended.
    ///
    /// **Off the action's path** (the rule in `deploylog`'s documentation): the row is
    /// written by a task of its own, the action never waits for it, and a failure is a warning,
    /// never the action's result. The task is tracked, so a quit waits for the row to be
    /// finished (`cancelled`) before the process exits.
    fn log_action(
        self: &Arc<Self>,
        op: &auspex_core::effects::ActionRequest,
    ) -> oneshot::Sender<Outcome> {
        self.log_row(NewAction {
            kind: match op.kind {
                ActionKind::Update => LoggedKind::Deploy,
                ActionKind::Build => LoggedKind::Build,
            },
            machine: op.machine.clone(),
            fleet_commit: None,
            fleet_dirty: None,
        })
    }

    /// The row for `new`, written beside whatever it records: see [`Shell::log_action`], whose
    /// rules every caller follows (a task of its own, a failure is a warning, never the
    /// action's result, tracked so a quit waits for it).
    fn log_row(self: &Arc<Self>, new: NewAction) -> oneshot::Sender<Outcome> {
        let (done, outcome) = oneshot::channel();
        let shell = Arc::clone(self);
        self.tasks.spawn(async move {
            let Some(log) = shell.action_log().await else {
                return;
            };
            let id = match log.start(new).await {
                Ok(id) => id,
                Err(e) => {
                    shell.warn(&format!("action log: this action is not logged: {e}"));
                    return;
                }
            };
            // The sender is dropped without a result only if the action's task is gone
            // (a panic): the row then says so rather than staying "running".
            let outcome = outcome.await.unwrap_or(Outcome {
                result: LoggedResult::Interrupted,
                exit_code: None,
                detail: None,
            });
            if let Err(e) = log.finish(id, outcome).await {
                shell.warn(&format!("action log: the result was not logged: {e}"));
            }
        });
        done
    }

    /// Where `f` keeps freezes: call before the first one. Without it a freeze lasts for the run
    /// (the demo, most tests).
    pub fn set_freeze_store(&self, store: FreezeStore) {
        *lock(&self.freeze_store) = Some(store);
    }

    /// `Effect::SaveFreeze`: one machine's freeze or unfreeze, kept in the state file, and a row
    /// in the action log beside it (kind `freeze` or `unfreeze`).
    ///
    /// The row follows the log's rules (see [`Shell::log_action`]): written by a task of its own,
    /// a failure of the log is a warning and never the freeze's result, and the freeze never
    /// waits for it. The save itself is tracked, so a quit waits for it.
    fn save_freeze(
        self: &Arc<Self>,
        mut request: Request<auspex_core::effects::SaveFreezeRequest>,
    ) {
        let op = request.operation.clone();
        let logged = self.log_row(NewAction {
            kind: if op.frozen {
                LoggedKind::Freeze
            } else {
                LoggedKind::Unfreeze
            },
            machine: op.machine.clone(),
            fleet_commit: None,
            fleet_dirty: None,
        });
        let store = lock(&self.freeze_store).clone();
        let shell = Arc::clone(self);
        self.tasks.spawn(async move {
            let answer: SaveFreezeAnswer = if let Some(store) = store {
                store
                    .set_with(&op.machine, op.frozen, &op.also_frozen)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                let mut set = lock(&shell.frozen_in_memory);
                set.extend(op.also_frozen.iter().cloned());
                if op.frozen {
                    set.insert(op.machine.clone());
                } else {
                    set.remove(&op.machine);
                }
                Ok(set.clone())
            };
            let _ = logged.send(match &answer {
                Ok(_) => Outcome {
                    result: LoggedResult::Succeeded,
                    exit_code: None,
                    detail: Some(if op.frozen { "frozen" } else { "unfrozen" }.to_owned()),
                },
                Err(why) => Outcome {
                    result: LoggedResult::Failed,
                    exit_code: None,
                    detail: Some(why.clone()),
                },
            });
            shell.resolve(&mut request, answer);
        });
    }

    /// The state file as it is now, or why it cannot be read: with no file configured, what this
    /// run froze in memory.
    async fn read_freezes(&self) -> LoadFreezesAnswer {
        self.freeze_reads.fetch_add(1, Ordering::SeqCst);
        let store = lock(&self.freeze_store).clone();
        match store {
            Some(store) => store.load().await.map_err(|e| e.to_string()),
            None => Ok(lock(&self.frozen_in_memory).clone()),
        }
    }

    /// `Effect::LoadFreezes`: the last look at the file before the core requests an update.
    fn load_freezes(
        self: &Arc<Self>,
        mut request: Request<auspex_core::effects::LoadFreezesRequest>,
    ) {
        let shell = Arc::clone(self);
        self.tasks.spawn(async move {
            let answer = shell.read_freezes().await;
            shell.resolve(&mut request, answer);
        });
    }

    /// The last look before `clan` is started: the file is read once more, and a machine frozen
    /// in it, or a file that cannot be read, ends the update here, unstarted. The core looked at
    /// the file at the `y`; this closes the gap between that and the spawn.
    ///
    /// # Errors
    ///
    /// Why the update must not start: the machine is frozen, or the state cannot be read.
    pub async fn freeze_gate(&self, machine: &str) -> Result<(), String> {
        match self.read_freezes().await {
            Ok(set) if set.contains(machine) => {
                Err(format!("{machine} is frozen; not updating it"))
            }
            Ok(_) => Ok(()),
            Err(why) => Err(format!(
                "could not read which machines are frozen ({why}); not updating {machine}"
            )),
        }
    }

    fn run_action(self: &Arc<Self>, mut request: Request<auspex_core::effects::ActionRequest>) {
        let op = request.operation.clone();
        // Answered on cancellation too: quitting waits for this.
        self.start_action(op, move |shell, end| shell.resolve(&mut request, end));
    }

    /// Runs `op` and hands its end to `answer`: [`Shell::run_action`] without the core's
    /// request, so a test can ask for an update directly, as a core that is behind the file
    /// would.
    pub fn start_action(
        self: &Arc<Self>,
        op: auspex_core::effects::ActionRequest,
        answer: impl FnOnce(&Arc<Self>, ActionEnd) + Send + 'static,
    ) {
        let token = self.open(op.id);
        self.output
            .begin(&format!("{} {}", op.kind.label(), op.machine));
        // The action's first line, before clan's: where it builds. Auspex's own words (Note),
        // not clan's output.
        self.output.push(Stream::Note, &op.start_note());
        let shell = Arc::clone(self);
        self.tasks.spawn(async move {
            // An update is the one action that can lift nothing a freeze holds: asked again, here.
            let gate = if op.kind == ActionKind::Update {
                shell.freeze_gate(&op.machine).await
            } else {
                Ok(())
            };
            // The row is written only for an action that starts: a refused one is not a deploy
            // that was tried.
            let (logged, ran) = if let Err(reason) = gate {
                (None, Ok(Err(reason)))
            } else {
                let logged = shell.log_action(&op);
                let ran = shell
                    .backend
                    .run_action(&op, &token, &|stream, text| shell.output.push(stream, text))
                    .await;
                (Some(logged), ran)
            };
            shell.forget(op.id);
            let end = action_end(&shell.output, &op, ran);
            shell.output.finish(&ended_line(&end));
            // The result is the action's. The row is a record of it: a closed receiver (the
            // log task gave up) is not an error here.
            if let Some(logged) = logged {
                let _ = logged.send(Outcome {
                    result: match end.result {
                        ActionResult::Succeeded => LoggedResult::Succeeded,
                        ActionResult::Failed => LoggedResult::Failed,
                        ActionResult::TimedOut => LoggedResult::TimedOut,
                        ActionResult::Cancelled => LoggedResult::Cancelled,
                    },
                    exit_code: end.exit_code,
                    detail: (!end.summary.is_empty()).then(|| end.summary.join(" | ")),
                });
            }
            answer(&shell, end);
        });
    }
}

/// How an action ended, from what the backend said and what it printed.
///
/// The summary is the last few lines (stderr's when there are any), never the output; a
/// failed update also gets the plain words for an ssh failure when its output shows one.
fn action_end(
    output: &Output,
    op: &auspex_core::effects::ActionRequest,
    ran: Result<Result<ClanRun, String>, Cancelled>,
) -> ActionEnd {
    let summary = || {
        auspex_domain::act::summary_lines(
            &output.tail(Stream::Stderr, 8),
            &output.tail(Stream::Stdout, 8),
        )
    };
    match ran {
        Err(Cancelled) => ActionEnd {
            result: ActionResult::Cancelled,
            exit_code: None,
            summary: summary(),
            explanation: None,
        },
        Ok(Err(reason)) => {
            output.push(Stream::Note, &reason);
            ActionEnd {
                result: ActionResult::Failed,
                exit_code: None,
                summary: vec![reason],
                explanation: None,
            }
        }
        Ok(Ok(run)) => {
            let (result, exit_code) = match run.end {
                RunEnd::Exited { code: Some(0), .. } => (ActionResult::Succeeded, Some(0)),
                RunEnd::Exited { code, .. } => (ActionResult::Failed, code),
                RunEnd::TimedOut => (ActionResult::TimedOut, None),
            };
            let ok = result == ActionResult::Succeeded;
            ActionEnd {
                result,
                exit_code,
                // Fixed text, not a line of output: an activation script prints whatever it
                // prints, and the action log holds summaries, never output. The Log tab has the
                // lines.
                summary: if ok {
                    let n = output.printed_lines();
                    vec![format!(
                        "exit 0, {n} {}",
                        if n == 1 { "line" } else { "lines" }
                    )]
                } else {
                    summary()
                },
                explanation: (!ok && op.kind == ActionKind::Update)
                    .then(|| output.last_lines(EXPLAIN_LINES))
                    .and_then(|lines| auspex_domain::act::explain_ssh_failure(&lines))
                    .map(|e| e.line()),
            }
        }
    }
}

/// The line under an action's output.
fn ended_line(end: &ActionEnd) -> String {
    let mut line = end.result.label().to_owned();
    if let Some(code) = end.exit_code {
        let _ = write!(line, ", exit {code}");
    }
    if let Some(why) = &end.explanation {
        let _ = write!(line, ". {why}");
    }
    line
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shell {
    /// Waits for a per-machine permit, or for `token` to be cancelled first (a request
    /// cancelled while queued never starts).
    fn gate(
        &self,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<OwnedSemaphorePermit, Cancelled>> + Send + 'static {
        Self::acquire(Arc::clone(&self.limit), token)
    }

    /// [`Self::gate`] for a package-preview query: the preview pool, not the shared one.
    fn preview_gate(
        &self,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<OwnedSemaphorePermit, Cancelled>> + Send + 'static {
        Self::acquire(Arc::clone(&self.previews), token)
    }

    fn acquire(
        limit: Arc<Semaphore>,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<OwnedSemaphorePermit, Cancelled>> + Send + 'static {
        let token = token.clone();
        async move {
            tokio::select! {
                biased;
                () = token.cancelled() => Err(Cancelled),
                p = limit.acquire_owned() => p.map_err(|_| Cancelled),
            }
        }
    }

    /// Per-machine calls running now, for tests.
    #[must_use]
    pub fn running_per_machine(&self) -> usize {
        MAX_CONCURRENT - self.limit.available_permits()
    }

    /// Preview queries holding a permit now, for tests.
    #[must_use]
    pub fn running_previews(&self) -> usize {
        MAX_PREVIEW_QUERIES - self.previews.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Work that runs until its token is cancelled, then reports having been cancelled.
    async fn until_cancelled(token: CancellationToken) -> Result<Result<u32, String>, Cancelled> {
        token.cancelled().await;
        Err(Cancelled)
    }

    #[tokio::test(start_paused = true)]
    async fn work_that_outlives_timeouts_diff_is_cancelled_and_answered_as_a_timeout() {
        let outer = CancellationToken::new();
        // Bounded from outside too, so a `within` that never times out fails this test
        // instead of hanging it.
        let answer = tokio::time::timeout(
            Duration::from_secs(1000),
            within(
                &outer,
                Duration::from_secs(90),
                "deployed closure query",
                until_cancelled,
            ),
        )
        .await
        .expect("within returns once timeouts.diff has passed");
        assert_eq!(
            answer,
            Ok(Err("deployed closure query timed out after 90s".to_owned()))
        );
        assert!(
            !outer.is_cancelled(),
            "the timeout cancels the work, not the request's token"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancellation_from_the_core_stays_a_cancellation() {
        let outer = CancellationToken::new();
        let canceller = outer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            canceller.cancel();
        });
        let answer = within(&outer, Duration::from_secs(90), "q", until_cancelled).await;
        assert_eq!(
            answer,
            Err(Cancelled),
            "no answer: the core dropped the request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn work_that_finishes_in_time_is_answered_as_it_is() {
        let outer = CancellationToken::new();
        let answer = within(&outer, Duration::from_secs(90), "q", |_| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok(Ok(7))
        })
        .await;
        assert_eq!(answer, Ok(Ok(7)));
        let failed = within(&outer, Duration::from_secs(90), "q", |_| async {
            Ok::<Result<u32, String>, Cancelled>(Err("no".to_owned()))
        })
        .await;
        assert_eq!(failed, Ok(Err("no".to_owned())));
    }

    /// The bound covers the whole request, not only the adapter's own deadline: work that is
    /// still waiting (for a permit, or ssh's approval) when `timeouts.diff` passes is cut.
    #[tokio::test(start_paused = true)]
    async fn the_bound_covers_waiting_as_well_as_running() {
        let outer = CancellationToken::new();
        let semaphore = Arc::new(Semaphore::new(0));
        let waiting = Arc::clone(&semaphore);
        let answer = tokio::time::timeout(
            Duration::from_secs(1000),
            within(
                &outer,
                Duration::from_secs(10),
                "q",
                move |token| async move {
                    tokio::select! {
                        () = token.cancelled() => Err(Cancelled),
                        _ = waiting.acquire_owned() => Ok(Ok(1u32)),
                    }
                },
            ),
        )
        .await
        .expect("within returns once timeouts.diff has passed");
        assert_eq!(answer, Ok(Err("q timed out after 10s".to_owned())));
    }
}
