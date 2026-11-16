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

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auspex_core::effects::{EvalAnswer, Generation, RequestId};
use auspex_core::{Auspex, Core, Effect, Event, ViewModel};
use auspex_effects::nix::EvalItem;
use auspex_effects::{CancellationToken, Cancelled};
use crux_core::Request;
use crux_core::capability::Operation;
use tokio::sync::Notify;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::log::Log;

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
        })
    }

    /// What to draw.
    #[must_use]
    pub fn view(&self) -> ViewModel {
        self.core.view()
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
            Effect::Inventory(request) => {
                let op = request.operation.clone();
                self.arm_deadline(op.generation);
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                self.one_shot(op.id, request, async move {
                    shell.backend.inventory(&op, &token).await
                });
            }
            Effect::Probe(request) => {
                let op = request.operation.clone();
                let token = self.open(op.id);
                let shell = Arc::clone(self);
                self.one_shot(op.id, request, async move {
                    shell.backend.probe(&op, &token).await
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
                self.one_shot(op.id, request, async move {
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
            Effect::Cancel(request) => self.cancel(request.operation.id),
        }
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

    /// `timeouts.settle` after a refresh starts, its deadline. A deadline for an older
    /// generation is ignored by the core, so an old timer needs no cancelling; the root token
    /// stops them all on shutdown.
    fn arm_deadline(self: &Arc<Self>, generation: Generation) {
        let shell = Arc::clone(self);
        let root = self.root.clone();
        let settle = self.settle;
        self.tasks.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(settle) => shell.send(Event::DeadlinePassed { generation }),
                () = root.cancelled() => {}
            }
        });
    }
}
