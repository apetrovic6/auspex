//! Freezing a machine (`f`): what the core decides, and the two things it asks the shell to do
//! with the state file (keep a freeze, read it again before an update).
//!
//! # What the core guarantees
//!
//! - **A frozen machine is never updated.** `u` refuses it ([`Freezes::refusal`], checked by
//!   `act::check`, so it holds again when the pre-update checks answer and at the `y`), and a
//!   later `U` (AUS-10) asks the view's `FreezeView::frozen` for the same answer. Build is not
//!   refused: it changes nothing on any machine.
//! - **Three sources, one answer.** `machines.<m>.frozen = true` in `config.toml` is
//!   *declared*: read at start-up, never written, and `f` cannot undo it (the file is a
//!   read-only store symlink under home-manager, and a key that edits a declaration would be
//!   undone by the next switch). A freeze made with `f` is *saved*, in the state file the shell
//!   keeps, and survives a restart. A freeze made with `f` whose save failed is *run-only*: the
//!   machine is frozen for this run, the file does not hold it. A machine is frozen if any of
//!   the three says so.
//! - **Freezing needs no act mode and no question** (it only prevents changes); **unfreezing
//!   asks** `y/N`, by name, because it is the direction that lets a deploy through.
//! - **Unknown is not empty.** When the shell could not read the state file (corrupt, written
//!   by a newer auspex, no usable state directory) it says so ([`SavedFreezes::Unknown`]), and
//!   the core then refuses `u` for *every* machine and refuses `f` too. Treating "could not
//!   tell" as "nothing is frozen" would update the machine the operator froze; refusing `f`
//!   keeps a corrupt file from being overwritten and a newer one from being downgraded.
//!   Declared freezes still apply, and a build is unaffected.
//! - **A freeze the operator was told holds, holds.** A freeze is applied at once (so `u` is
//!   refused at once) and saved beside it. If the save fails the machine is *run-only* frozen
//!   and the notice says it was not kept; it stays frozen, whatever a later save answers,
//!   until the operator unfreezes it (the next successful save also carries it into the
//!   file: [`SaveFreezeRequest::also_frozen`]). An unfreeze of a saved freeze is applied only
//!   once the save has succeeded: the file is what a restart will believe, so a failed unfreeze
//!   leaves the machine frozen, and says so. One save at a time: `f` while one is out says so.
//! - **The last look is at the file.** Another running auspex may have frozen a machine since
//!   this one read the file at start-up. So at the `y` of an update, and again in the shell
//!   right before the action starts, the file is read again ([`LoadFreezesRequest`]); a freeze
//!   found there is honoured, and an update whose file cannot be read is refused.
//!
//! The save is [`SaveFreezeRequest`]: one machine, frozen or not. The shell changes that entry
//! in the file under a lock and answers with the whole set as written, which the core adopts,
//! so a freeze made by another running auspex is also seen after the next `f` here.

use std::collections::BTreeSet;

use auspex_domain::fleet::is_plain_name;
use crux_core::Command;
use crux_core::render::render;

use super::{Event, Model};
use crate::config::{FreezeConfig, SavedFreezes};
use crate::effects::{
    Effect, LoadFreezesAnswer, LoadFreezesRequest, RequestId, SaveFreezeAnswer, SaveFreezeRequest,
};

/// A save that is out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Save {
    id: RequestId,
    machine: String,
    /// What was asked: freeze (`true`) or unfreeze.
    frozen: bool,
}

/// The re-read of the file that stands between the operator's `y` and the update.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Recheck {
    id: RequestId,
    machine: String,
    /// The pending system the operator confirmed: what the update would deploy.
    path: String,
}

/// Which machines are frozen, and from where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Freezes {
    declared: BTreeSet<String>,
    saved: SavedFreezes,
    /// Frozen with `f` in this run, not (yet) in the file: the save failed, or is still out.
    run_only: BTreeSet<String>,
    save: Option<Save>,
    recheck: Option<Recheck>,
}

impl Freezes {
    pub(super) fn new(config: &FreezeConfig) -> Self {
        Self {
            declared: config.declared.clone(),
            saved: config.saved.clone(),
            run_only: BTreeSet::new(),
            save: None,
            recheck: None,
        }
    }

    pub(super) fn is_declared(&self, machine: &str) -> bool {
        self.declared.contains(machine)
    }

    fn in_file(&self, machine: &str) -> bool {
        matches!(&self.saved, SavedFreezes::Known(s) if s.contains(machine))
    }

    /// Frozen with `f`: in the file, or for this run.
    fn by_f(&self, machine: &str) -> bool {
        self.in_file(machine) || self.run_only.contains(machine)
    }

    /// Whether `machine` is frozen, by any source. Unknown saved freezes are not counted here:
    /// [`Freezes::refusal`] is what refuses on them.
    pub(super) fn is_frozen(&self, machine: &str) -> bool {
        self.is_declared(machine) || self.by_f(machine)
    }

    /// Why `u` must not touch `machine`, or `None`.
    pub(super) fn refusal(&self, machine: &str) -> Option<String> {
        if self.is_declared(machine) {
            return Some(format!(
                "{machine} is frozen in config.toml (machines.{machine}.frozen); f cannot \
                 unfreeze it"
            ));
        }
        match &self.saved {
            SavedFreezes::Unknown(why) => Some(format!(
                "which machines are frozen is unknown ({why}); not updating {machine}"
            )),
            SavedFreezes::Known(_) if self.by_f(machine) => {
                Some(format!("{machine} is frozen; f to unfreeze"))
            }
            SavedFreezes::Known(_) => None,
        }
    }

    pub(super) const fn saving(&self) -> bool {
        self.save.is_some()
    }

    /// An update is waiting for the file to be read again.
    pub(super) const fn rechecking(&self) -> bool {
        self.recheck.is_some()
    }

    pub(super) const fn saved(&self) -> &SavedFreezes {
        &self.saved
    }

    pub(super) const fn declared(&self) -> &BTreeSet<String> {
        &self.declared
    }

    pub(super) const fn run_only(&self) -> &BTreeSet<String> {
        &self.run_only
    }

    /// Takes in what the file says now: the set the file holds replaces the saved set, and a
    /// run-only freeze the file now holds is an ordinary one. A run-only freeze the file does
    /// not hold stays: only the operator lifts it.
    fn adopt(&mut self, set: BTreeSet<String>) {
        self.run_only.retain(|m| !set.contains(m));
        self.saved = SavedFreezes::Known(set);
    }
}

/// `f`: freezes the selected machine, or asks about unfreezing it.
pub(super) fn toggle(model: &mut Model) -> Command<Effect, Event> {
    model.view.notice = None;
    let names = model.names();
    let Some(machine) = model
        .view
        .index(&names)
        .and_then(|i| names.get(i))
        .map(|n| (*n).to_owned())
    else {
        return model.notice("no machine selected");
    };
    if !is_plain_name(&machine) || !model.rows.iter().any(|r| r.machine.name == machine) {
        return model.notice(format!("{machine:?} is not a machine of the inventory"));
    }
    if model.freeze.is_declared(&machine) {
        return model.notice(format!(
            "{machine} is frozen in config.toml (machines.{machine}.frozen); f cannot unfreeze \
             it: remove the setting there"
        ));
    }
    if let SavedFreezes::Unknown(why) = model.freeze.saved() {
        return model.notice(format!(
            "which machines are frozen is unknown ({why}); not changing it"
        ));
    }
    if model.freeze.saving() {
        return model.notice("the previous freeze is still being saved; try again");
    }
    if model.freeze.by_f(&machine) {
        model.ask(super::act::Confirm::Unfreeze { machine });
        return render();
    }
    request(model, machine, true)
}

/// `y` to "unfreeze luna?": re-checked, since the world may have moved while it was open.
pub(super) fn unfreeze_confirmed(model: &mut Model, machine: String) -> Command<Effect, Event> {
    if model.freeze.saving() {
        return model.notice("the previous freeze is still being saved; try again");
    }
    if model.freeze.is_declared(&machine) || !model.freeze.by_f(&machine) {
        return model.notice(format!("{machine} is not frozen with f any more"));
    }
    // Frozen for this run only: the file never held it, so there is nothing to keep. Lifting it
    // is the operator's word, and it needs no write that could fail.
    if !model.freeze.in_file(&machine) {
        model.freeze.run_only.remove(&machine);
        return model.notice(format!(
            "unfroze {machine} (it was frozen for this run only)"
        ));
    }
    request(model, machine, false)
}

/// Asks the shell to keep the change, and applies a freeze at once.
fn request(model: &mut Model, machine: String, frozen: bool) -> Command<Effect, Event> {
    let id = model.fresh_id();
    // Applied now, as run-only: it is in the file only once the shell says so.
    if frozen {
        model.freeze.run_only.insert(machine.clone());
    }
    let also_frozen = model
        .freeze
        .run_only
        .iter()
        .filter(|m| **m != machine)
        .cloned()
        .collect();
    model.freeze.save = Some(Save {
        id,
        machine: machine.clone(),
        frozen,
    });
    model.view.notice = Some(if frozen {
        format!("froze {machine}")
    } else {
        format!("unfreezing {machine}…")
    });
    Command::all([
        Command::request_from_shell(SaveFreezeRequest {
            id,
            machine,
            frozen,
            also_frozen,
        })
        .then_send(move |answer| Event::FreezeSaved { id, answer }),
        render(),
    ])
}

/// The shell's answer: the set as it was written, or why it was not.
pub(super) fn saved(
    model: &mut Model,
    id: RequestId,
    answer: SaveFreezeAnswer,
) -> Command<Effect, Event> {
    let Some(save) = model.freeze.save.take_if(|s| s.id == id) else {
        return Command::done();
    };
    match answer {
        Ok(set) => {
            if !save.frozen {
                model.freeze.run_only.remove(&save.machine);
            }
            model.freeze.adopt(set);
            // A freeze that came back without its machine stays: `request` made it run-only
            // and `adopt` keeps what the file does not hold, so a deploy is not let through.
            let now = if save.frozen { "frozen" } else { "unfrozen" };
            model.notice(format!("{} {now}", save.machine))
        }
        // A failed freeze is already applied, as run-only, and stays so until lifted.
        Err(why) if save.frozen => model.notice(format!(
            "{} is frozen for this run only: the freeze was not saved: {why}",
            save.machine
        )),
        Err(why) => model.notice(format!(
            "{} is still frozen: the unfreeze was not saved: {why}",
            save.machine
        )),
    }
}

/// The operator said yes to the update of `machine` (whose pending system was `path`) and every
/// check passed: the last look at the file before the update is requested.
pub(super) fn recheck(model: &mut Model, machine: String, path: String) -> Command<Effect, Event> {
    let id = model.fresh_id();
    model.freeze.recheck = Some(Recheck { id, machine, path });
    Command::all([
        Command::request_from_shell(LoadFreezesRequest { id })
            .then_send(move |answer| Event::FreezesLoaded { id, answer }),
        render(),
    ])
}

/// The shell's answer to [`recheck`]: what the file says now. The freeze is honoured, an
/// unreadable file refuses, and only then is the update requested (after every check is made
/// again: the world may have moved while the file was being read).
pub(super) fn loaded(
    model: &mut Model,
    id: RequestId,
    answer: LoadFreezesAnswer,
) -> Command<Effect, Event> {
    let Some(waiting) = model.freeze.recheck.take_if(|r| r.id == id) else {
        return Command::done();
    };
    // Quit while the file was being read: nothing is started on the way out.
    if model.view.quit {
        return Command::done();
    }
    let Recheck { machine, path, .. } = waiting;
    match answer {
        Err(why) => model.notice(format!(
            "could not read which machines are frozen ({why}); not updating {machine}"
        )),
        Ok(set) => {
            // Only while the file was understood at start-up: an unknown state stays unknown
            // (and refuses in `check`), it is not repaired by one good read.
            if matches!(model.freeze.saved(), SavedFreezes::Known(_)) {
                model.freeze.adopt(set);
            }
            super::act::launch_after_recheck(model, machine, &path)
        }
    }
}
