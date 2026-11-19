//! What an action (`u`, `b`) says about how it ended: the short summary that is logged and
//! shown, and the plain explanation of a deploy that failed at ssh.
//!
//! Pure over lines that are already sanitised (the adapter runs every line of a process's
//! output through [`crate::text::sanitize_line`] before anything else sees it).
//!
//! # Why ssh failures get their own words
//!
//! `clan machines update` goes through `nixos-rebuild`, which strips `SSH_AUTH_SOCK` from the
//! environment of the ssh it starts, and an agent holding many keys makes a server give up
//! before the right one is offered ("Too many authentication failures"). Both look like
//! "Permission denied" and send the operator to the agent. The plan's "Known traps" names
//! them, and [`explain_ssh_failure`] says so at the place the operator is looking.

/// How many of the last lines make an action's summary.
pub const SUMMARY_LINES: usize = 3;

/// The last [`SUMMARY_LINES`] lines of what an action printed, as a short summary: stderr's
/// when it printed any (the cause of a failure is there, and last), else stdout's.
///
/// Never the whole output: the summary is written to a log other programs read, and the
/// output of a deploy carries whatever the machine's activation scripts print.
#[must_use]
pub fn summary_lines(stderr_tail: &[String], stdout_tail: &[String]) -> Vec<String> {
    let from = if stderr_tail.iter().any(|l| !l.is_empty()) {
        stderr_tail
    } else {
        stdout_tail
    };
    let lines: Vec<&String> = from.iter().filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(SUMMARY_LINES)..]
        .iter()
        .map(|l| (*l).clone())
        .collect()
}

/// Why a deploy failed at ssh, and what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SshExplanation {
    /// What went wrong, as the operator should read it.
    pub what: &'static str,
    /// The fix.
    pub fix: &'static str,
}

impl SshExplanation {
    /// One line: what, then the fix.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{} {}", self.what, self.fix)
    }
}

/// Reads the tail of a failed action's output for an ssh failure.
///
/// Matched on ssh's own wording, case-insensitively, most specific first (too many keys also
/// says "permission denied" further down). `None` when the output shows no ssh failure: the
/// caller then shows the summary and nothing more.
#[must_use]
pub fn explain_ssh_failure(lines: &[String]) -> Option<SshExplanation> {
    // The last lines only: a long deploy mentions ssh all through its output, and what
    // matters is what ended it.
    let tail = lines[lines.len().saturating_sub(40)..]
        .iter()
        .map(|l| l.to_lowercase())
        .collect::<Vec<_>>();
    let has = |needle: &str| tail.iter().any(|l| l.contains(needle));
    if has("too many authentication failures") {
        return Some(SshExplanation {
            what: "The deploy failed at ssh: the server gave up after too many authentication \
                   failures (the ssh agent offered more keys than it allows).",
            fix: "Offer one key: set `IdentitiesOnly yes` and `IdentityFile` for this host in \
                  ~/.ssh/config. nixos-rebuild strips SSH_AUTH_SOCK, so the agent is not what \
                  answers here.",
        });
    }
    if has("permission denied (") || has("permission denied, please try again") {
        return Some(SshExplanation {
            what: "The deploy failed at ssh: no key was accepted.",
            fix: "nixos-rebuild strips SSH_AUTH_SOCK, so the agent's keys may never have been \
                  offered: name the key with `IdentityFile` (and `IdentitiesOnly yes`) for \
                  this host in ~/.ssh/config, and check the ssh agent's approval prompt.",
        });
    }
    if has("host key verification failed")
        || has("remote host identification has changed")
        || has("no matching host key")
    {
        return Some(SshExplanation {
            what: "The deploy failed at ssh: the host key was not accepted. auspex deploys with \
                   --host-key-check strict and never trusts a new key by itself.",
            fix: "Check the key is the machine's, then add it to known_hosts (ssh to the host \
                  once from a terminal) or correct the stale entry.",
        });
    }
    if has("connection refused")
        || has("connection timed out")
        || has("no route to host")
        || has("network is unreachable")
        || has("could not resolve hostname")
        || has("connection reset by")
        || has("connection closed by")
    {
        return Some(SshExplanation {
            what: "The deploy failed at ssh: the machine did not answer.",
            fix: "Check it is up and reachable, and that its target host is right (the \
                  detail panel shows where it came from).",
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn the_summary_is_the_last_few_stderr_lines() {
        let err = v(&["a", "", "b", "c", "d"]);
        let out = v(&["noise"]);
        assert_eq!(summary_lines(&err, &out), ["b", "c", "d"]);
    }

    #[test]
    fn with_no_stderr_the_summary_is_stdout() {
        assert_eq!(summary_lines(&[], &v(&["one", "two"])), ["one", "two"]);
        assert_eq!(summary_lines(&v(&[""]), &v(&["one"])), ["one"]);
        assert_eq!(summary_lines(&[], &[]), Vec::<String>::new());
    }

    #[test]
    fn ssh_failures_are_named_most_specific_first() {
        let many = explain_ssh_failure(&v(&[
            "Received disconnect from 10.0.0.1 port 22:2: Too many authentication failures",
            "root@10.0.0.1: Permission denied (publickey).",
        ]))
        .unwrap();
        assert!(many.what.contains("too many authentication failures"));
        assert!(many.fix.contains("IdentitiesOnly"));

        let denied =
            explain_ssh_failure(&v(&["root@10.0.0.1: Permission denied (publickey)."])).unwrap();
        assert!(denied.fix.contains("SSH_AUTH_SOCK"));

        let hostkey = explain_ssh_failure(&v(&["Host key verification failed."])).unwrap();
        assert!(hostkey.what.contains("host key"));

        let down = explain_ssh_failure(&v(&[
            "ssh: connect to host 10.0.0.1 port 22: Connection refused",
        ]))
        .unwrap();
        assert!(down.what.contains("did not answer"));
    }

    #[test]
    fn a_failure_that_is_not_ssh_is_not_blamed_on_ssh() {
        assert_eq!(
            explain_ssh_failure(&v(&["error: builder for '/nix/store/x.drv' failed"])),
            None
        );
        assert_eq!(explain_ssh_failure(&[]), None);
    }

    #[test]
    fn only_the_end_of_the_output_counts() {
        let mut lines = v(&["Permission denied (publickey)."]);
        lines.extend((0..100).map(|i| format!("building {i}")));
        assert_eq!(explain_ssh_failure(&lines), None);
    }
}
