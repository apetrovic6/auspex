//! The core as a shell sees it: events in, effects out.
//!
//! Deliberately drives the public surface only — this is what an Android or Apple shell
//! does across the FFI boundary, minus the serialisation. A unit test inside the crate can
//! reach private state; this cannot, which is the point.

use auspex_core::{Auspex, Core, Effect, Event};

#[test]
fn a_shell_sees_the_count_change() {
    let core = Core::<Auspex>::new();

    let effects = core.process_event(Event::Increment);

    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Render(_))),
        "the shell is told to redraw"
    );
    // A number, not a sentence: what the shell sees is what it can translate (ADR 0016).
    assert_eq!(core.view().count, 1);
}
