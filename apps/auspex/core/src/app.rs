//! What this app does, independent of how it is shown.
//!
//! The core is pure: it never performs an effect, it asks for one. A shell fulfils the
//! request and hands the answer back. That is what lets the same logic drive a terminal, a
//! browser, a desktop window and a phone without any of them disagreeing.

use crux_core::macros::effect;
use crux_core::render::{RenderOperation, render};
use crux_core::{App, Command};
use facet::Facet;
use serde::{Deserialize, Serialize};

/// Everything a shell can tell the core.
///
/// `Facet` and `Serialize` are what the typegen reads to emit the Kotlin and Swift
/// equivalents, so a variant added here appears on every platform.
#[derive(Facet, Serialize, Deserialize, Clone, Debug)]
#[repr(C)]
pub enum Event {
    Increment,
    Decrement,
    Reset,
}

/// Everything the core can ask a shell to do.
///
/// One variant so far. When this app needs the outside world — an API call, the clock,
/// storage — add a variant here whose operation is in the app's own vocabulary rather than
/// in HTTP's: see `apps/counter` for a worked example of a shell-fulfilled API effect.
#[effect(facet_typegen)]
#[derive(Debug)]
pub enum Effect {
    Render(RenderOperation),
}

/// The app's whole state. Private: a shell sees [`ViewModel`], never this.
#[derive(Default)]
pub struct Model {
    count: i64,
}

/// What a shell draws: **values**, so that each shell formats them in its own language.
///
/// This used to be a `String` built here, which meant the sentence was whichever language
/// the core happened to be written in (ADR 0016).
///
/// There is no `status` field, because there is nothing to report: this core has no effects
/// beyond a redraw, so no request is ever outstanding and nothing ever fails. The version
/// with an API effect, a `pending` flag and a `Status` whose `Failed` carries a `Failure` —
/// kinds and a code, never a sentence — is `apps/counter/core`. Copy that shape when this core
/// grows an effect: the shells here have no status row yet either, and the sentences a shell
/// looks up by kind and code are in `crates/i18n/locales/common/`.
///
/// There is no `updated_at` field either, for a sharper reason: nothing in this core happens
/// *at* a moment. It has no clock and no effect to stamp, and an instant invented in a pure
/// reducer is worse than none — while an always-zero one would pass every shell that formats
/// it, silently, forever (ADR 0020). The shape with an instant lives in `apps/counter/core`:
/// the moment is recorded where the write happens, crosses the `ViewModel` as epoch
/// milliseconds, and each shell renders it in its own locale's date. The sentence is already
/// here — `updated_at` in `crates/i18n/locales/common/` waits, unnamed by any template call
/// site, exactly as the failure sentences above do.
#[derive(Facet, Serialize, Deserialize, Clone, Default)]
pub struct ViewModel {
    pub count: i64,
}

#[derive(Default)]
pub struct Auspex;

impl App for Auspex {
    type Event = Event;
    type Model = Model;
    type ViewModel = ViewModel;
    type Effect = Effect;

    fn update(&self, event: Event, model: &mut Model) -> Command<Effect, Event> {
        match event {
            // `saturating_*` rather than `+`: a wrapping counter is a surprise, and a
            // panicking one in a release build is a silently wrong number.
            Event::Increment => model.count = model.count.saturating_add(1),
            Event::Decrement => model.count = model.count.saturating_sub(1),
            Event::Reset => model.count = 0,
        }
        render()
    }

    /// The number, not the sentence around it — `count_is` is looked up by each shell, from
    /// `locales/common/`, because every app scaffolded from this skeleton says it.
    fn view(&self, model: &Model) -> ViewModel {
        ViewModel { count: model.count }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Run one event and report whether the core asked for a redraw.
    ///
    /// `App::update` is called directly rather than through a test harness: the harness in
    /// `crux_core::testing` is deprecated, and this is the same surface a shell drives.
    fn update(app: &Auspex, event: Event, model: &mut Model) -> bool {
        let mut command = app.update(event, model);
        command
            .effects()
            .any(|effect| matches!(effect, Effect::Render(_)))
    }

    #[test]
    fn counts_up_and_down() {
        let app = Auspex;
        let mut model = Model::default();

        update(&app, Event::Increment, &mut model);
        update(&app, Event::Increment, &mut model);
        update(&app, Event::Decrement, &mut model);

        assert_eq!(app.view(&model).count, 1);
    }

    #[test]
    fn reset_returns_to_zero() {
        let app = Auspex;
        let mut model = Model::default();

        update(&app, Event::Increment, &mut model);
        update(&app, Event::Reset, &mut model);

        assert_eq!(app.view(&model).count, 0);
    }

    #[test]
    fn every_event_asks_for_a_render() {
        let app = Auspex;

        // A shell redraws when the core says to. An update that changes the model without
        // asking would leave the screen showing the previous state.
        for event in [Event::Increment, Event::Decrement, Event::Reset] {
            let mut model = Model::default();
            assert!(
                update(&app, event.clone(), &mut model),
                "{event:?} changed the model without asking for a render"
            );
        }
    }
}
