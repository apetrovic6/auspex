//! Which nodes auspex cordoned and has not given back: the shell's own record, independent of the
//! core, so that **every way out of auspex** can uncordon what it still holds, or say which nodes
//! it could not.
//!
//! The core uncordons a node when it ends a machine that is safe to put back (a failed drain, a
//! quit before the update started). A termination signal, a second `q` and a panic do not wait for
//! the core: the loop ends, the shell's tasks are cancelled, and the uncordon the core asked for
//! is cancelled with them. This registry is what is left to act on at that point:
//!
//! - a node is **registered** when its drain starts (the drain cordons first, and leaves the node
//!   cordoned when it fails or is killed);
//! - it is **released** when an uncordon of it succeeds;
//! - it is **committed** when its machine's update starts: from then on the machine may be half
//!   updated, the core's rule is to leave the node cordoned, and the exit does not undo that. It is
//!   still *reported*;
//! - at exit [`Cordons::last_resort`] runs `kubectl uncordon` for every node still registered, with
//!   a plain child process and a bound (the async machinery is gone or cancelled by then, and a
//!   panic hook has none), and returns the lines that say which nodes remain cordoned and the
//!   command that gives each back.
//!
//! `kubectl` is the one the run uses, with the same kubeconfig and context.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one last-resort `kubectl uncordon` may take before it is killed.
pub const LAST_RESORT_BOUND: Duration = Duration::from_secs(15);

/// A node, and how to reach its cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    node: String,
    kubectl: OsString,
    kubeconfig: PathBuf,
    context: Option<String>,
}

impl Held {
    /// The arguments of the uncordon, as the adapter builds them.
    fn args(&self) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("--kubeconfig"),
            self.kubeconfig.clone().into_os_string(),
        ];
        if let Some(context) = &self.context {
            args.push(OsString::from(format!("--context={context}")));
        }
        args.push(OsString::from("uncordon"));
        args.push(OsString::from(&self.node));
        args
    }

    /// The command an operator would type.
    fn by_hand(&self) -> String {
        let context = self
            .context
            .as_ref()
            .map_or_else(String::new, |c| format!(" --context {c}"));
        format!(
            "kubectl --kubeconfig {}{context} uncordon {}",
            self.kubeconfig.display(),
            self.node
        )
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// Cordoned by auspex, update not started: to be uncordoned at exit.
    pending: Vec<Held>,
    /// Cordoned by auspex, update started: left on purpose, reported at exit.
    left: Vec<Held>,
}

/// The record. One per [`crate::shell::Shell`], so two shells in one process (the tests) never
/// touch each other's nodes.
#[derive(Debug, Default)]
pub struct Cordons {
    inner: Mutex<Inner>,
}

impl Cordons {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// `node`'s drain is starting: it is cordoned by auspex from now on.
    pub fn register(
        &self,
        node: &str,
        kubectl: OsString,
        kubeconfig: PathBuf,
        context: Option<String>,
    ) {
        self.with(|i| {
            i.pending.retain(|h| h.node != node);
            i.pending.push(Held {
                node: node.to_owned(),
                kubectl,
                kubeconfig,
                context,
            });
        });
    }

    /// An uncordon of `node` succeeded: it is nobody's to give back.
    pub fn release(&self, node: &str) {
        self.with(|i| {
            i.pending.retain(|h| h.node != node);
            i.left.retain(|h| h.node != node);
        });
    }

    /// `node`'s machine is being updated: the node stays cordoned if auspex goes away, and says so.
    pub fn commit(&self, node: &str) {
        self.with(|i| {
            if let Some(at) = i.pending.iter().position(|h| h.node == node) {
                let held = i.pending.remove(at);
                i.left.push(held);
            }
        });
    }

    /// Nodes that are still cordoned by auspex, in either state.
    #[must_use]
    pub fn held(&self) -> Vec<String> {
        self.with(|i| {
            i.pending
                .iter()
                .chain(&i.left)
                .map(|h| h.node.clone())
                .collect()
        })
    }

    /// Uncordons every node still pending, one at a time, each within `bound`; the result is a
    /// line for every node that is **still** cordoned by auspex afterwards (the ones that could
    /// not be uncordoned, and the ones left on purpose), with the command that gives it back.
    ///
    /// Plain `std` processes: this runs after the runtime's tasks are cancelled, and from a panic
    /// hook. Output is discarded, stdin is `/dev/null`, and a `kubectl` past the bound is killed.
    pub fn last_resort(&self, bound: Duration) -> Vec<String> {
        let pending = self.with(|i| std::mem::take(&mut i.pending));
        let mut lines = Vec::new();
        let mut failed = Vec::new();
        for held in pending {
            match run_bounded(&held, bound) {
                Ok(()) => {}
                Err(why) => {
                    lines.push(format!(
                        "node {} is STILL CORDONED ({why}): {}",
                        held.node,
                        held.by_hand()
                    ));
                    failed.push(held);
                }
            }
        }
        self.with(|i| {
            i.pending.extend(failed);
            for held in &i.left {
                lines.push(format!(
                    "node {} was left cordoned (its update was stopped or failed; the machine \
                     may be broken): {}",
                    held.node,
                    held.by_hand()
                ));
            }
        });
        lines
    }
}

/// Runs the uncordon of `held`, killing it at `bound`.
fn run_bounded(held: &Held, bound: Duration) -> Result<(), String> {
    // An absolute path as given, a bare name on an absolute `PATH` entry (as the adapters do).
    let program = if held.kubectl.as_encoded_bytes().contains(&b'/') {
        PathBuf::from(&held.kubectl)
    } else {
        crate::backend::on_path(&held.kubectl.to_string_lossy())
            .ok_or_else(|| "kubectl is not on PATH".to_owned())?
    };
    let mut child = Command::new(program)
        .args(held.args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("kubectl cannot be started: {e}"))?;
    let until = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("kubectl uncordon failed: {status}")),
            Ok(None) if Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "kubectl uncordon took more than {}s",
                    bound.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("waiting for kubectl failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(c: &Cordons, node: &str) {
        c.register(
            node,
            OsString::from("/nonexistent/kubectl"),
            PathBuf::from("/k/config"),
            Some("rke2".to_owned()),
        );
    }

    /// Committed nodes are left, not uncordoned; released ones are nobody's; a pending one whose
    /// uncordon cannot be made is reported with the command, and stays pending.
    #[test]
    fn the_record_says_what_is_left_and_what_could_not_be_given_back() {
        let c = Cordons::default();
        held(&c, "luna");
        held(&c, "sol");
        held(&c, "mars");
        c.commit("luna");
        c.release("mars");
        assert_eq!(c.held(), ["sol", "luna"]);
        let report = c.last_resort(Duration::from_secs(1));
        assert_eq!(report.len(), 2, "{report:?}");
        assert!(
            report[0].starts_with("node sol is STILL CORDONED (kubectl cannot be started")
                && report[0]
                    .ends_with("kubectl --kubeconfig /k/config --context rke2 uncordon sol"),
            "{report:?}"
        );
        assert!(
            report[1].starts_with("node luna was left cordoned")
                && report[1].contains("uncordon luna"),
            "{report:?}"
        );
        // Still there for a second attempt (a panic hook after the exit path, say).
        assert_eq!(c.held(), ["sol", "luna"]);
        // Releasing a node it never held is nothing.
        c.release("nobody");
    }
}
