//! The Leptos adapter, behind the `leptos` feature.
//!
//! Kept apart from the rest of the crate so a client on another toolkit — gpui,
//! say — links the translations without pulling a web framework in. Nothing
//! else in this crate references Leptos.

use leptos::prelude::*;

use crate::Locale;

/// A reactive translation.
///
/// [`crate::translate`] reads whatever locale it is handed, and rust-i18n's own
/// global one is invisible to Leptos — a string looked up through it would
/// render once and never change. This reads the locale signal from context
/// instead, so every call site is a closure subscribed to it, and returns that
/// closure: `<h1>{tr(Key::Lakes)}</h1>` renders and re-renders on its own.
///
/// The key is `impl AsRef<str>` rather than [`crate::Key`] so a plain string
/// still works for a one-off, though `Key` is what makes a typo a compile
/// error.
///
/// # Panics
///
/// If no `RwSignal<Locale>` has been provided as context.
pub fn tr(key: impl AsRef<str> + Copy + 'static) -> impl Fn() -> String + Copy {
    let locale = expect_context::<RwSignal<Locale>>();

    move || crate::translate(key, locale.get())
}
