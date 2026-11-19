//! What the two closure queries run and print, so the adapters only have to obtain it.
//!
//! The package preview ([`crate::diff`]) compares two lists of store paths. The deployed
//! list comes from the machine (`nix-store --query --requisites /run/current-system`, over
//! the probe's ssh control master) and the pending list from a binary cache or the local
//! store (`nix path-info --recursive` or `nix-store --query --requisites`). All of them
//! print one store path per line, which [`parse_closure`] reads. Everything that arrives
//! over that pipe is untrusted text: a machine can print anything, so a line that is not a
//! store path is dropped and counted, never passed on.

use crate::observe::StorePath;

/// The script the deployed-closure query sends on ssh's stdin, to `/bin/sh -s`
/// ([`crate::fleet::PROBE_REMOTE_COMMAND`]), the same way the probe does.
///
/// Why a script and not `nix-store …` as the remote command: the remote command is parsed by
/// the login shell, and fish or csh parse `PATH=… cmd` differently. A script on stdin goes
/// through `/bin/sh` and nothing else, and nothing in it comes from outside auspex.
///
/// Why the PATH extension: a non-interactive ssh session on darwin has only the system
/// `PATH`, which has no `nix-store` (the probe's scripts extend it for the same reason).
/// `exec`, so the shell is not left holding the pipe if ssh closes early.
///
/// Read-only: `--query` never writes the store, and `/run/current-system` is a symlink that
/// `nix-store` resolves itself. A machine without that link makes `nix-store` fail, which is
/// reported as the failure it is.
pub const DEPLOYED_QUERY_SCRIPT: &str = r"PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
export PATH
exec nix-store --query --requisites /run/current-system
";

/// The store paths of one closure, as printed, and how many lines were not store paths.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClosurePaths {
    /// Every line that is a `/nix/store/<hash>-<name>` path, in the order printed.
    pub paths: Vec<StorePath>,
    /// Non-empty lines that were not. Added to [`crate::diff::PackageDiff::invalid`] by the
    /// caller, so the preview says how much of the machine's answer it could not read.
    pub dropped: usize,
}

/// Reads one store path per line. Empty lines (the final newline, a blank between two
/// banners) are neither paths nor counted; a trailing `\r` is not part of the path.
///
/// Every other line goes through [`StorePath::new`]: a banner, a warning that reached stdout,
/// a path with a newline smuggled into its name or a control character is dropped and counted.
/// The text may be lossily decoded, so invalid UTF-8 shows up here as U+FFFD, which is not a
/// store character either.
#[must_use]
pub fn parse_closure(text: &str) -> ClosurePaths {
    let mut closure = ClosurePaths::default();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        match StorePath::new(line) {
            Ok(path) => closure.paths.push(path),
            Err(_) => closure.dropped += 1,
        }
    }
    closure
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-glibc-2.40-66";
    const B: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-systemd-256.10";

    #[test]
    fn paths_are_read_in_order_and_noise_is_counted() {
        let text = format!(
            "Welcome to terra\n{A}\n\n{B}\r\nwarning: something\n/etc/passwd\n\
             /nix/store/short-name\n{A}/bin/../../x\n"
        );
        let closure = parse_closure(&text);
        assert_eq!(
            closure
                .paths
                .iter()
                .map(StorePath::as_str)
                .collect::<Vec<_>>(),
            [A, B]
        );
        // The banner, the warning, a path outside the store, a malformed hash and a path
        // that climbs out of its store object.
        assert_eq!(closure.dropped, 5);
    }

    #[test]
    fn an_empty_answer_is_no_paths_and_nothing_dropped() {
        assert_eq!(parse_closure(""), ClosurePaths::default());
        assert_eq!(parse_closure("\n\n"), ClosurePaths::default());
    }

    #[test]
    fn control_characters_and_replacement_characters_are_dropped() {
        let escape = format!("{A}\u{1b}[31m");
        let bidi = format!("{A}\u{202e}");
        let lossy = format!("{A}\u{fffd}");
        let closure = parse_closure(&format!("{escape}\n{bidi}\n{lossy}\n{B}\n"));
        assert_eq!(closure.paths.len(), 1);
        assert_eq!(closure.dropped, 3);
    }

    /// The script is sent to every machine, so what it can do is part of the contract: one
    /// read-only query, nothing that writes, nothing from outside.
    #[test]
    fn the_script_is_the_one_read_only_query() {
        let commands: Vec<&str> = DEPLOYED_QUERY_SCRIPT
            .lines()
            .filter(|l| !l.starts_with("PATH=") && *l != "export PATH")
            .collect();
        assert_eq!(
            commands,
            ["exec nix-store --query --requisites /run/current-system"]
        );
        // No redirection, pipe, substitution or chaining; `$PATH` in the PATH extension is
        // the only expansion.
        assert!(!DEPLOYED_QUERY_SCRIPT.contains(['>', '<', '|', '`', ';', '&']));
        assert_eq!(DEPLOYED_QUERY_SCRIPT.matches('$').count(), 1);
    }
}
