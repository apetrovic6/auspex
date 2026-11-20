//! One auspex updates a fleet at a time: an exclusive `flock` per fleet in the state directory.
//!
//! The in-process rules ("never two machines of a rolling group at once") hold inside one auspex.
//! Two auspex processes (two terminals, an ssh session someone forgot) each check the cluster and
//! each start the next member, so two machines of one group, or two etcd members, were out at
//! once. This lock is what makes the rule hold across them: it is taken for a whole `U` and for
//! each `u`, and a second auspex that cannot take it **refuses**, naming who holds it.
//!
//! The lock is `flock` on a file of its own (`update-<digest of the fleet>.lock`, 0600, in the
//! private state directory beside the freeze file), on a description opened afresh, so two takes
//! in one process exclude each other too. It is released when the guard is dropped, and by the
//! kernel when the process dies by any means: a lock cannot be left behind by a crash. The file
//! holds a line saying who took it, which is only for the refusal's message: the lock is the
//! `flock`, never the text.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use auspex_domain::text::sanitize_line;
use rustix::fs::{FlockOperation, flock};
use sha2::{Digest as _, Sha256};

/// The lock, held until this is dropped.
#[derive(Debug)]
pub struct FleetLock {
    _file: File,
}

/// Where the lock for `fleet_key` ([`crate::freeze::fleet_key`]) is, in `state_dir`.
#[must_use]
pub fn lock_path(state_dir: &Path, fleet_key: &str) -> PathBuf {
    let digest = Sha256::digest(fleet_key.as_bytes());
    let name = digest[..8].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    state_dir.join(format!("update-{name}.lock"))
}

/// Takes the update lock for the fleet, or says who has it.
///
/// `holder` is written into the file for the next refusal: who is updating and what
/// ("pid 123, tester@host: update all").
///
/// # Errors
///
/// Another auspex holds it (the message names the fleet and, when it could be read, the holder),
/// or the state directory or file cannot be used.
pub fn acquire(state_dir: &Path, fleet_key: &str, holder: &str) -> Result<FleetLock, String> {
    crate::ssh::prepare_private_dir(state_dir, "state directory")?;
    let path = lock_path(state_dir, fleet_key);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&path)
        .map_err(|e| format!("cannot open the update lock {}: {e}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => {
            let mut text = String::new();
            let _ = (&mut file).take(512).read_to_string(&mut text);
            let who = sanitize_line(text.trim());
            return Err(format!(
                "another auspex is updating this fleet ({fleet_key}){}: wait for it, or quit it",
                if who.is_empty() {
                    String::new()
                } else {
                    format!(": {who}")
                }
            ));
        }
        Err(e) => {
            return Err(format!(
                "cannot lock {}: {e}; not updating without it",
                path.display()
            ));
        }
    }
    // Who holds it, for the next refusal. A failure to write it does not unlock anything.
    let _ = file.set_len(0);
    let _ = file.write_all(holder.as_bytes());
    Ok(FleetLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("axl").tempdir().unwrap()
    }

    #[test]
    fn a_second_taker_is_refused_naming_the_holder_until_the_first_lets_go() {
        let d = dir();
        let state = d.path().join("state");
        let first = acquire(&state, "/fleet", "pid 1, me@host: update all").unwrap();
        let refused = acquire(&state, "/fleet", "pid 2, you@host: update luna").unwrap_err();
        assert!(
            refused.contains("another auspex is updating this fleet (/fleet)")
                && refused.contains("pid 1, me@host: update all"),
            "{refused}"
        );
        // Another fleet is another lock.
        let other = acquire(&state, "/other", "pid 3: update all").unwrap();
        drop(other);
        drop(first);
        let again = acquire(&state, "/fleet", "pid 2: update luna").unwrap();
        // The file now names the new holder.
        let text = std::fs::read_to_string(lock_path(&state, "/fleet")).unwrap();
        assert_eq!(text, "pid 2: update luna");
        drop(again);
    }

    #[test]
    fn the_lock_is_private_and_never_followed_through_a_symlink() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let d = dir();
        let state = d.path().join("state");
        let guard = acquire(&state, "/fleet", "x").unwrap();
        let mode = std::fs::metadata(lock_path(&state, "/fleet"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(guard);
        std::fs::remove_file(lock_path(&state, "/fleet")).unwrap();
        let target = d.path().join("elsewhere");
        std::fs::write(&target, "mine").unwrap();
        symlink(&target, lock_path(&state, "/fleet")).unwrap();
        assert!(
            acquire(&state, "/fleet", "x").is_err(),
            "a symlink is refused"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
    }
}
