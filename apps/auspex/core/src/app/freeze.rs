//! Freezing a machine (`f`): what the core decides, and the one thing it asks the shell to keep.
//!
//! # What the core guarantees
//!
//! - **A frozen machine is never updated.** `u` refuses it ([`Freezes::refusal`], checked by
//!   `act::check`, so it holds again at the `y`), and a later `U` (AUS-10) asks
//!   the view's `FreezeView::frozen` for the same answer. Build is not refused: it changes
//!   nothing on any machine.
//! - **Two sources, one answer.** `machines.<m>.frozen = true` in `config.toml` is
//!   *declared*: read at start-up, never written, and `f` cannot undo it (the file is a
//!   read-only store symlink under home-manager, and a key that edits a declaration would be
//!   undone by the next switch). A freeze made with `f` is *saved*, in the state file the shell
//!   keeps, and survives a restart. A machine is frozen if either says so.
//! - **Freezing needs no act mode and no question** (it only prevents changes); **unfreezing
//!   asks** `y/N`, by name, because it is the direction that lets a deploy through.
//! - **Unknown is not empty.** When the shell could not read the state file (corrupt, written
//!   by a newer auspex, no usable state directory) it says so ([`SavedFreezes::Unknown`]), and
//!   the core then refuses `u` for *every* machine and refuses `f` too. Treating "could not
//!   tell" as "nothing is frozen" would update the machine the operator froze; refusing `f`
//!   keeps a corrupt file from being overwritten and a newer one from being downgraded.
//!   Declared freezes still apply, and a build is unaffected.
//! - **The memory is the file's.** A freeze is applied at once (so `u` is refused at once) and
//!   saved beside it; if the save fails the machine stays frozen for this run and the notice
//!   says it was not kept. An unfreeze is applied only once the save has succeeded: the file
//!   is what a restart will believe, so a failed unfreeze leaves the machine frozen, and says
//!   so. One save at a time: `f` while one is out says so.
//!
//! The save is [`SaveFreezeRequest`]: one machine, frozen or not. The shell changes that one
//! entry in the file under a lock and answers with the whole set as written, which the core
//! adopts, so a freeze made by another running auspex is seen after the next `f` here.

use std::collections::BTreeSet;

use auspex_domain::fleet::is_plain_name;
use crux_core::Command;
use crux_core::render::render;

use super::{Event, Model};
use crate::config::{FreezeConfig, SavedFreezes};
use crate::effects::{Effect, RequestId, SaveFreezeAnswer, SaveFreezeRequest};

/// A save that is out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Save {
    id: RequestId,
    machine: String,
    /// What was asked: freeze (`true`) or unfreeze.
    frozen: bool,
}

/// Which machines are frozen, and from where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Freezes {
    declared: BTreeSet<String>,
    saved: SavedFreezes,
    save: Option<Save>,
}

impl Freezes {
    pub(super) fn new(config: &FreezeConfig) -> Self {
        Self {
            declared: config.declared.clone(),
            saved: config.saved.clone(),
            save: None,
        }
    }

    pub(super) fn is_declared(&self, machine: &str) -> bool {
        self.declared.contains(machine)
    }

    fn is_saved(&self, machine: &str) -> bool {
        matches!(&self.saved, SavedFreezes::Known(s) if s.contains(machine))
    }

    /// Whether `machine` is frozen, by either source. Unknown saved freezes are not counted
    /// here: [`Freezes::refusal`] is what refuses on them.
    pub(super) fn is_frozen(&self, machine: &str) -> bool {
        self.is_declared(machine) || self.is_saved(machine)
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
            SavedFreezes::Known(s) if s.contains(machine) => {
                Some(format!("{machine} is frozen; f to unfreeze"))
            }
            SavedFreezes::Known(_) => None,
        }
    }

    pub(super) const fn saving(&self) -> bool {
        self.save.is_some()
    }

    pub(super) const fn saved(&self) -> &SavedFreezes {
        &self.saved
    }

    pub(super) const fn declared(&self) -> &BTreeSet<String> {
        &self.declared
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
    if model.freeze.is_saved(&machine) {
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
    if model.freeze.is_declared(&machine) || !model.freeze.is_saved(&machine) {
        return model.notice(format!("{machine} is not frozen with f any more"));
    }
    request(model, machine, false)
}

/// Asks the shell to keep the change, and applies a freeze at once.
fn request(model: &mut Model, machine: String, frozen: bool) -> Command<Effect, Event> {
    let id = model.fresh_id();
    if frozen && let SavedFreezes::Known(s) = &mut model.freeze.saved {
        s.insert(machine.clone());
    }
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
            model.freeze.saved = SavedFreezes::Known(set);
            // What was asked is never forgotten, whatever the file said back: a freeze that
            // came back without its machine would let a deploy through.
            if save.frozen
                && let SavedFreezes::Known(s) = &mut model.freeze.saved
            {
                s.insert(save.machine.clone());
            }
            let now = if save.frozen { "frozen" } else { "unfrozen" };
            model.notice(format!("{} {now}", save.machine))
        }
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
