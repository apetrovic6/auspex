//! The two things auspex does to a machine: update it, and build its pending system here.
//!
//! `u` runs `clan machines update`, `b` a local `nix build`. Both stream their output through
//! the process supervisor ([`crate::clan`]'s runner: its own session, no terminal, stdin on
//! `/dev/null`, capped output; cancelling kills the whole group and reaps it).
//!
//! # The argv, exactly
//!
//! `u`: `clan machines update --host-key-check strict --flake <fleet> -- <machine>`.
//!
//! `b`: `nix build --no-link --print-out-paths <fleet>#nixosConfigurations.<m>.config.system.build.toplevel`
//! (darwin: `<fleet>#darwinConfigurations.<m>.system`).
//!
//! # Safety, enforced here and not only by the caller
//!
//! **`clan machines update` with no machine updates every machine in the fleet.** So the argv
//! is built by [`update_argv`], which refuses any name that is not a plain machine name
//! ([`auspex_domain::fleet::is_plain_name`]: 1 to 63 of `[A-Za-z0-9_-]`, starting with a
//! letter or digit) *before* anything is spawned, and always contains exactly that one name,
//! after a `--`. An empty name, a name that reads as an option (`-h`, `--flake`), one with a
//! space or a second name in it: refused. The core refuses a name the inventory does not
//! list; this layer cannot know the inventory, and refuses what no inventory may contain.
//!
//! **`--flake` comes before the `--`**, or clan would read it as a second machine. The
//! generic runner appends `--flake` last, so the machine goes in as its *tail*
//! ([`crate::clan::run_with_tail`]).
//!
//! **A build never builds another system's derivation.** [`build`] compares the machine's
//! `system` with this machine's before it starts nix, and refuses a mismatch (or an unknown
//! system) without running anything.
//!
//! Nothing here evaluates or builds a machine except through the `nix` it is given.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use auspex_domain::fleet::{MachineClass, is_plain_name};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::clan::{
    ClanError, ClanLine, ClanRun, ClanStep, RunParams, apply_run_environment, run_with_tail, stream,
};
use crate::nix::FlakeRef;
use crate::process::{OnExit, command, spawn, spawn_error};

/// What `clan` is run with before `--flake`: `machines update --host-key-check strict`.
///
/// `strict`: a host whose key is not already known fails the deploy instead of being trusted
/// on first use. A deploy is not the place to accept a key nobody has looked at.
pub const UPDATE_HEAD: [&str; 4] = ["machines", "update", "--host-key-check", "strict"];

/// Why an action was refused before anything ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

impl From<Refused> for ClanError {
    fn from(r: Refused) -> Self {
        Self::new(ClanStep::Run, format!("refused: {}", r.0))
    }
}

fn check_name(machine: &str) -> Result<(), Refused> {
    if machine.is_empty() {
        return Err(Refused(
            "no machine named: `clan machines update` with no machine would update every \
             machine in the fleet"
                .to_owned(),
        ));
    }
    if !is_plain_name(machine) {
        return Err(Refused(format!(
            "{machine:?} is not a plain machine name (1 to 63 of A-Z a-z 0-9 - _, starting \
             with a letter or digit)"
        )));
    }
    Ok(())
}

/// The arguments of `clan` for updating `machine`, split around `--flake <fleet>`: what comes
/// before it ([`UPDATE_HEAD`]) and what comes after (`-- <machine>`).
///
/// # Errors
///
/// [`Refused`] when `machine` is not a plain machine name; nothing is built from it.
pub fn update_args(machine: &str) -> Result<(Vec<OsString>, Vec<OsString>), Refused> {
    check_name(machine)?;
    Ok((
        UPDATE_HEAD.iter().map(OsString::from).collect(),
        vec![OsString::from("--"), OsString::from(machine)],
    ))
}

/// The whole argv (after the program) of an update: `machines update --host-key-check strict
/// --flake <fleet> -- <machine>`.
///
/// # Errors
///
/// [`Refused`] as [`update_args`].
pub fn update_argv(flake: &FlakeRef, machine: &str) -> Result<Vec<OsString>, Refused> {
    let (head, tail) = update_args(machine)?;
    Ok(crate::clan::run_argv_with_tail(flake, &head, &tail))
}

/// Updates one machine with `clan` (`program`: the fleet's pinned binary), streaming its
/// output to `tx`.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired or `tx`'s receiver was dropped: the whole process group
/// has been killed and reaped. A refused name or a program that cannot be started is the
/// inner [`ClanError`].
pub async fn update(
    program: &Path,
    flake: &FlakeRef,
    machine: &str,
    params: &RunParams,
    cancel: &CancellationToken,
    tx: UnboundedSender<ClanLine>,
) -> Result<Result<ClanRun, ClanError>, Cancelled> {
    let (head, tail) = match update_args(machine) {
        Ok(a) => a,
        Err(r) => return Ok(Err(r.into())),
    };
    run_with_tail(program, flake, &head, &tail, params, cancel, tx).await
}

/// The flake output `b` builds: `<fleet>#nixosConfigurations.<m>.config.system.build.toplevel`,
/// or for darwin `<fleet>#darwinConfigurations.<m>.system`.
///
/// # Errors
///
/// [`Refused`] when `machine` is not a plain machine name.
pub fn build_installable(
    flake: &FlakeRef,
    machine: &str,
    class: MachineClass,
) -> Result<String, Refused> {
    check_name(machine)?;
    let attr = match class {
        MachineClass::Nixos => format!(
            "{}.{machine}.config.system.build.toplevel",
            class.flake_output()
        ),
        MachineClass::Darwin => format!("{}.{machine}.system", class.flake_output()),
    };
    Ok(format!("{}#{attr}", flake.as_str()))
}

/// The whole argv (after the program) of a build.
///
/// # Errors
///
/// [`Refused`] as [`build_installable`].
pub fn build_argv(
    flake: &FlakeRef,
    machine: &str,
    class: MachineClass,
) -> Result<Vec<OsString>, Refused> {
    Ok(vec![
        OsString::from("build"),
        OsString::from("--no-link"),
        OsString::from("--print-out-paths"),
        OsString::from(build_installable(flake, machine, class)?),
    ])
}

/// Builds `machine`'s pending system with `nix` on this machine, streaming nix's output.
///
/// `system` is the nix system the machine's toplevel is for, `local_system` this machine's
/// ([`crate::clan::current_system`]). They must be equal: a different one is refused before
/// nix runs, because building it here would either fail after a long wait or, with a remote
/// builder configured, quietly start a build somewhere the operator did not choose.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired or `tx`'s receiver was dropped (the whole group has been
/// killed and reaped). A refusal, or a nix that cannot be started, is the inner [`ClanError`].
#[expect(
    clippy::too_many_arguments,
    reason = "each is a distinct input of one action; a struct would only rename them"
)]
pub async fn build(
    nix: &OsStr,
    flake: &FlakeRef,
    machine: &str,
    class: MachineClass,
    system: Option<&str>,
    local_system: &str,
    params: &RunParams,
    cancel: &CancellationToken,
    tx: UnboundedSender<ClanLine>,
) -> Result<Result<ClanRun, ClanError>, Cancelled> {
    let argv = match build_argv(flake, machine, class) {
        Ok(a) => a,
        Err(r) => return Ok(Err(r.into())),
    };
    match system {
        Some(s) if s == local_system => {}
        Some(s) => {
            return Ok(Err(Refused(format!(
                "{machine} is {s} and this machine is {local_system}: auspex does not build \
                 another system's derivations"
            ))
            .into()));
        }
        None => {
            return Ok(Err(Refused(format!(
                "the system of {machine} is not known, so it cannot be checked against this \
                 machine ({local_system})"
            ))
            .into()));
        }
    }
    let cannot =
        |e: &std::io::Error| ClanError::new(ClanStep::Run, spawn_error("cannot run nix", nix, e));
    let mut cmd = match command(nix) {
        Ok(c) => c,
        Err(e) => return Ok(Err(cannot(&e))),
    };
    cmd.args(argv);
    apply_run_environment(&mut cmd);
    if let Some(dir) = flake.local_dir() {
        cmd.current_dir(dir);
    }
    let sp = match spawn(cmd, false, OnExit::KillGroup, None) {
        Ok(s) => s,
        Err(e) => return Ok(Err(cannot(&e))),
    };
    stream(sp, params, cancel, tx).await
}
