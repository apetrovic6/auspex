//! Here only because the template's `cargo xtask check` requires it, and recorded as a
//! template gap in the PR.
//!
//! `plural_families_reach_a_shell` (`tools/xtask/src/invariants.rs`) refuses a Crux app none
//! of whose shells names every plural family in `crates/i18n/locales/common/` with a
//! `plural_key(Key::…)` call. The one family is the template's demonstrator, `apples.*`.
//! auspex is English-only and its shell draws no catalog sentence, so the smallest thing
//! that satisfies the check without putting apples on screen is this test, which also
//! proves the family still renders. When the template drops the demonstrator (or the check
//! learns that an app may opt out), delete this file and the `i18n` dev-dependency.

use i18n::{Key, Locale, number, plural_key, translate_with};

#[test]
fn the_template_demonstrator_plural_still_renders() {
    let en = Locale::FALLBACK;
    let one = translate_with!(plural_key(Key::Apples, 1, en), en, count = number(1, en));
    let two = translate_with!(plural_key(Key::Apples, 2, en), en, count = number(2, en));
    assert_eq!((one.as_str(), two.as_str()), ("1 apple", "2 apples"));
}
