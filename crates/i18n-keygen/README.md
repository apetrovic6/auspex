# i18n-keygen

Build-time generator: reads a rust-i18n locale tree (`locales/**/*.toml`) and emits
`Key`, `Locale`, the `path` helpers and the `translate` wrappers as source into the
calling crate's `OUT_DIR`.

Ported from `fishing`'s `i18n-keygen`, unchanged except the manifest. Its tests' fixture
strings still speak of lakes and jezera — they are fixtures, not this repository's copy.

## What belongs here

Anything that is a *rule about the locale tree*: namespace checks, drift policy, what the
emitted code looks like, and which keys one app's native resources may carry
(`resources::for_app` — an app's namespace and the bare common keys, taken from the key
itself, so an app name is an argument here and never a list of them). The rules live here so they can be unit-tested against fixture
trees — the real `crates/i18n/locales` cannot express a namespace violation without
breaking the build that ships it.

What does **not** belong here: the translations (those are `crates/i18n/locales/`), any
UI-framework integration (that is `crates/i18n`'s `leptos` feature), and any knowledge of
this repository's apps. The only dependency is `toml`.
