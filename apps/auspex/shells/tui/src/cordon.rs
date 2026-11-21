//! Which nodes auspex cordoned and has not given back: the shell's own record, independent of the
//! core, so that **every way out of auspex** can uncordon what it still holds, or say which nodes
//! it could not.
//!
//! The core uncordons a node when it ends a machine that is safe to put back (a failed drain, a
//! quit before the update started). A termination signal, a second `q` and a panic do not wait for
//! the core indefinitely: a signal or `q` gives it a grace to stop the roll, and a panic gives it
//! none; then the shell acts on its own record. This registry is what there is to act on:
//!
//! - a node is **registered** when its drain starts (the drain cordons first, and leaves the node
//!   cordoned when it fails or is killed);
//! - it is **released** when an uncordon of it succeeds;
//! - it is **committed** when its machine's update starts: from then on the machine may be half
//!   updated, the core's rule is to leave the node cordoned, and the exit does not undo that. It is
//!   still *reported*;
//! - at exit [`Cordons::last_resort`] runs `kubectl uncordon` for every node still registered, with
//!   a plain child process and a bound, and returns the lines that say which nodes remain cordoned
//!   and the command that gives each back. It is plain `std` because a panic hook has no runtime to
//!   ask. On a signal or `q` it runs **after the core's grace and before the shell cancels its
//!   tasks** (so the rows are closed with what is left, [`crate::shell::Shell::shutdown`]): the
//!   core's own uncordon of the same node may still be running beside it. Two uncordons of one
//!   node do no harm; a node the core's one gives back meanwhile is [`Cordons::release`]d, and
//!   the last resort then does not report it.
//!
//! `kubectl` is the one the run uses, with the same kubeconfig and context.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use auspex_domain::text::shell_quote;

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
            .map_or_else(String::new, |c| format!(" --context {}", shell_quote(c)));
        format!(
            "kubectl --kubeconfig {}{context} uncordon {}",
            shell_quote(&self.kubeconfig.to_string_lossy()),
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
    /// Taken from `pending` by a last resort that is running `kubectl` for them now. A node
    /// released (or registered anew) meanwhile leaves this list, and is then not reported when its
    /// attempt fails.
    trying: Vec<Held>,
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

    /// `node`'s drain is starting: it is cordoned by auspex from now on, and only now: a node an
    /// earlier run left cordoned (and the operator then uncordoned by hand) is not "left" any more,
    /// whatever this drain comes to.
    pub fn register(
        &self,
        node: &str,
        kubectl: OsString,
        kubeconfig: PathBuf,
        context: Option<String>,
    ) {
        self.with(|i| {
            i.pending.retain(|h| h.node != node);
            i.left.retain(|h| h.node != node);
            i.trying.retain(|h| h.node != node);
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
            i.trying.retain(|h| h.node != node);
        });
    }

    /// `node`'s machine is being updated: the node stays cordoned if auspex goes away, and says so.
    pub fn commit(&self, node: &str) {
        self.with(|i| {
            if let Some(at) = i.pending.iter().position(|h| h.node == node) {
                let held = i.pending.remove(at);
                // One entry per node: a second commit (or one after a `left` that was never
                // cleared) would name the node twice at exit.
                i.left.retain(|h| h.node != node);
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

    /// What a panic hook does: [`Cordons::last_resort`] **if the panic is on `loop_thread`**, the
    /// thread that runs the interactive loop, where a panic ends auspex; nothing otherwise.
    ///
    /// The runtime is multi-threaded and a panic in an adapter's task is *not* the end of auspex:
    /// the roll goes on. Uncordoning then would put a drained node back in service just before
    /// its update, and empty the record the exit report is made from.
    #[must_use]
    pub fn on_panic(&self, loop_thread: std::thread::ThreadId, bound: Duration) -> Vec<String> {
        if std::thread::current().id() == loop_thread {
            self.last_resort(bound)
        } else {
            Vec::new()
        }
    }

    /// Uncordons every node still pending, one at a time, each within `bound`; the result is a
    /// line for every node that is **still** cordoned by auspex afterwards (the ones that could
    /// not be uncordoned, and the ones left on purpose), with the command that gives it back.
    ///
    /// Plain `std` processes, because a panic hook has no runtime. Output is discarded, stdin is
    /// `/dev/null`, and a `kubectl` past the bound is killed. A node that is released while its
    /// attempt runs (the core's own uncordon of it succeeded) is not reported even if the attempt
    /// fails: it is back.
    pub fn last_resort(&self, bound: Duration) -> Vec<String> {
        let pending = self.with(|i| {
            let taken = std::mem::take(&mut i.pending);
            i.trying.clone_from(&taken);
            taken
        });
        let mut failures = Vec::new();
        for held in pending {
            if let Err(why) = run_bounded(&held, bound) {
                failures.push((held, why));
            }
        }
        let mut lines = Vec::new();
        self.with(|i| {
            let trying = std::mem::take(&mut i.trying);
            for (held, why) in failures {
                let released = !trying.iter().any(|t| t.node == held.node);
                if released || i.pending.iter().any(|p| p.node == held.node) {
                    continue;
                }
                lines.push(format!(
                    "node {} is STILL CORDONED ({why}): {}",
                    held.node,
                    held.by_hand()
                ));
                i.pending.push(held);
            }
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

    /// The command is pasted into a shell: a kubeconfig with a space and a single quote, and a
    /// context with a space, are quoted; plain ones stay bare.
    #[test]
    fn the_by_hand_command_quotes_what_needs_it() {
        let h = Held {
            node: "luna".to_owned(),
            kubectl: OsString::from("kubectl"),
            kubeconfig: PathBuf::from("/my dir/it's/config"),
            context: Some("my ctx".to_owned()),
        };
        assert_eq!(
            h.by_hand(),
            r"kubectl --kubeconfig '/my dir/it'\''s/config' --context 'my ctx' uncordon luna"
        );
        let plain = Held {
            kubeconfig: PathBuf::from("/k/config"),
            context: Some("rke2".to_owned()),
            ..h
        };
        assert_eq!(
            plain.by_hand(),
            "kubectl --kubeconfig /k/config --context rke2 uncordon luna"
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

    /// N8. A node whose update failed is left cordoned and the operator uncordons it by hand; the next
    /// `U` of the same process drains it again. The old record must not outlive the new drain: the
    /// exit names the node once (and not as "left cordoned" when it was uncordoned at quit), and a
    /// second commit never lists it twice.
    #[test]
    fn a_new_drain_replaces_what_an_earlier_run_left_and_a_commit_never_lists_a_node_twice() {
        let c = Cordons::default();
        held(&c, "luna");
        c.commit("luna");
        assert_eq!(c.held(), ["luna"], "left cordoned by the first run");
        // The operator uncordoned it by hand; a later drain of luna starts.
        held(&c, "luna");
        assert_eq!(c.held(), ["luna"], "named once, as pending");
        // Quit during that drain: the exit uncordons it, and does not also say it was left.
        let report = c.last_resort(Duration::from_secs(1));
        assert_eq!(report.len(), 1, "{report:?}");
        assert!(
            report[0].starts_with("node luna is STILL CORDONED"),
            "{report:?}"
        );
        // `commit` is a guard of its own: a record that already has the node as left (it cannot
        // through `register`, which clears it) still ends with one entry.
        held(&c, "sol");
        c.with(|i| {
            let again = i.pending.last().unwrap().clone();
            i.left.push(again);
        });
        c.commit("sol");
        assert_eq!(
            c.held(),
            ["luna", "sol"],
            "luna is still pending; sol is named once"
        );
    }

    /// R5. On the signal and quit paths the last resort runs while the core's own uncordon of the
    /// same node may still be under way. Suppose the core's succeeds (`release`) while the
    /// shell's attempt fails: the exit must not then say the node is still cordoned. The fake
    /// `kubectl` takes a second and fails; the release lands while it runs.
    #[test]
    fn a_node_the_core_gave_back_while_the_last_resort_failed_is_not_reported_as_still_cordoned() {
        use std::os::unix::fs::PermissionsExt as _;
        let (Some(sh), Some(sleep)) = (
            crate::backend::on_path("sh"),
            crate::backend::on_path("sleep"),
        ) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // Written by a process of its own, as the integration tests do, so a fork in another test
        // thread cannot hold the file open for writing when it is run ("text file busy").
        let src = dir.path().join("kubectl.src");
        let dst = dir.path().join("kubectl");
        std::fs::write(
            &src,
            format!("#!{}\n{} 1\nexit 1\n", sh.display(), sleep.display()),
        )
        .unwrap();
        let copied = Command::new(&sh)
            .arg("-c")
            .arg(
                r#"while IFS= read -r l || [ -n "$l" ]; do printf '%s\n' "$l"; done < "$1" > "$2""#,
            )
            .arg("sh")
            .arg(&src)
            .arg(&dst)
            .status()
            .unwrap();
        assert!(copied.success());
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();

        let c = std::sync::Arc::new(Cordons::default());
        c.register(
            "luna",
            dst.into_os_string(),
            PathBuf::from("/k/config"),
            Some("rke2".to_owned()),
        );
        let last_resort = {
            let c = std::sync::Arc::clone(&c);
            std::thread::spawn(move || c.last_resort(Duration::from_secs(10)))
        };
        // The last resort has taken the entry and is running `kubectl`; the core's own uncordon
        // of luna now succeeds.
        std::thread::sleep(Duration::from_millis(300));
        c.release("luna");
        let report = last_resort.join().unwrap();
        assert_eq!(report, Vec::<String>::new(), "luna is back");
        assert_eq!(c.held(), Vec::<String>::new());
    }

    /// A panic in some other thread (an adapter's task) is not the end of auspex, so the hook must
    /// not uncordon anything; one on the loop's thread is, and does.
    #[test]
    fn a_panic_hook_acts_only_on_the_loop_thread() {
        let c = std::sync::Arc::new(Cordons::default());
        held(&c, "luna");
        let loop_thread = std::thread::current().id();
        let elsewhere = {
            let c = std::sync::Arc::clone(&c);
            std::thread::spawn(move || c.on_panic(loop_thread, Duration::from_secs(1)))
                .join()
                .unwrap()
        };
        assert_eq!(elsewhere, Vec::<String>::new(), "nothing is said or done");
        assert_eq!(c.held(), ["luna"], "and the node is still on the record");
        let here = c.on_panic(loop_thread, Duration::from_secs(1));
        assert!(
            here.iter()
                .any(|l| l.starts_with("node luna is STILL CORDONED")),
            "{here:?}"
        );
    }
}
