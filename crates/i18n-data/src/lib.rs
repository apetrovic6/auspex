//! The CLDR data `i18n` formats with, sliced to the locales this repository ships.
//!
//! Nothing in `src/data/` is written by hand: `just i18n::generate-data` produces it and
//! `just i18n::check-data` fails if the committed copy has drifted from the invocation next
//! to it. The output is committed so a plain `cargo build` needs no datagen, no CLDR
//! download and no network — the rule from `crates/proto/src/gen`, applied to data instead
//! of to a wire contract.
//!
//! What is here is `en`, `hr` and the root `und`, because `crates/i18n/locales` ships `en`
//! and `hr` and a fallback chain needs its ancestors. It is *not* every locale Unicode
//! publishes, which is what `icu_*`'s default `compiled_data` feature links into every
//! binary that touches a formatter. ADR 0021 has the measurement and the alternatives that
//! were rejected; ADR 0020's table is the number it is judged against.
//!
//! If you add a locale to `crates/i18n/locales`, this crate goes stale and the invariant
//! over [`locale_codes`] in `i18n` says so — rather than quietly formatting the new locale
//! out of the root's data and passing.

// The generated payload: nine markers, each a `databake`-serialised static that holds `en`,
// `hr` and `und`, plus the macro that implements `DataProvider<Marker>` for a type of our
// choosing.
include!("data/mod.rs");

/// The provider, named so consumers take a reference to [`CLDR`] instead of choosing a type
/// of their own.
///
/// A unit struct: the payload lives in the associated `static`s the generated macros emit,
/// so this value carries nothing and `&CLDR` is a pointer into the binary's data segment.
pub struct Cldr;

// The one-line version of this block would be `impl_data_provider!(Cldr)`, and it is not
// enough: the aggregate macro implements the `DataProvider` halves only, and the invariant
// in `i18n` needs each marker's `IterableDataProvider` half — the identifiers the tries
// actually contain. So the nine generated per-marker macros are called directly, with `ITER`.
//
// This list and the one in [`locale_codes`] name the same nine markers. They cannot drift in
// the direction that would matter: each line of `locale_codes` names a marker type, and it
// compiles only while the matching impl from below exists.
make_provider!(Cldr);
impl_datetime_patterns_date_gregorian_v1!(Cldr, ITER);
impl_datetime_patterns_time_v1!(Cldr, ITER);
impl_datetime_names_month_gregorian_v1!(Cldr, ITER);
impl_datetime_names_dayperiod_v1!(Cldr, ITER);
impl_datetime_names_year_gregorian_v1!(Cldr, ITER);
impl_datetime_patterns_glue_v1!(Cldr, ITER);
impl_decimal_symbols_v1!(Cldr, ITER);
impl_decimal_digits_v1!(Cldr, ITER);
impl_plurals_cardinal_v1!(Cldr, ITER);

/// The sliced data. Pass `&CLDR` to ICU4X's `try_new_unstable` constructors.
///
/// A `const` and not a `static` because there is nothing to take address-stably of: the
/// generated impls read associated `static`s, and this value is zero-sized.
pub const CLDR: Cldr = Cldr;

/// Every locale identifier the slice holds, including `und` — the root, which every fallback
/// chain needs and no catalog ships.
///
/// Read out of the provider's own keys rather than from a list someone typed, because the
/// only use for this function is `i18n`'s invariant that data and catalog cover the same
/// locales, and both sides of that comparison have to come from somewhere different or the
/// check asserts that a string equals itself.
///
/// A marker whose identifiers cannot be listed fails the call rather than contributing a
/// smaller set: a set that is wrong by omission is precisely what that invariant looks for.
pub fn locale_codes() -> std::collections::BTreeSet<String> {
    use icu_provider::IterableDataProvider;

    let mut codes = std::collections::BTreeSet::new();
    for ids in
        [
            <Cldr as IterableDataProvider<
                icu_datetime::provider::semantic_skeletons::DatetimePatternsDateGregorianV1,
            >>::iter_ids(&CLDR),
            <Cldr as IterableDataProvider<
                icu_datetime::provider::semantic_skeletons::DatetimePatternsTimeV1,
            >>::iter_ids(&CLDR),
            <Cldr as IterableDataProvider<
                icu_datetime::provider::names::DatetimeNamesMonthGregorianV1,
            >>::iter_ids(&CLDR),
            <Cldr as IterableDataProvider<
                icu_datetime::provider::names::DatetimeNamesDayperiodV1,
            >>::iter_ids(&CLDR),
            <Cldr as IterableDataProvider<
                icu_datetime::provider::names::DatetimeNamesYearGregorianV1,
            >>::iter_ids(&CLDR),
            <Cldr as IterableDataProvider<
                icu_datetime::provider::semantic_skeletons::DatetimePatternsGlueV1,
            >>::iter_ids(&CLDR),
            <Cldr as IterableDataProvider<icu_decimal::provider::DecimalSymbolsV1>>::iter_ids(
                &CLDR,
            ),
            <Cldr as IterableDataProvider<icu_decimal::provider::DecimalDigitsV1>>::iter_ids(&CLDR),
            <Cldr as IterableDataProvider<icu_plurals::provider::PluralsCardinalV1>>::iter_ids(
                &CLDR,
            ),
        ]
    {
        codes.extend(
            ids.expect("the slice can list its own identifiers")
                .into_iter()
                .map(|id| id.locale.to_string()),
        );
    }
    codes
}

/// The identifiers a request for `code` resolves to, in the order they are tried, ending at
/// `und`.
///
/// This is the same fallback the sliced provider performs internally — [`CLDR`] carries its
/// own fallback data — written down so `i18n` can assert that everything in the slice
/// belongs to a locale the catalog ships. It matters because a sliced provider does not hold
/// only the locales it was asked for: `en` arrives with `en-001` and its regional children,
/// because CLDR splits English by region, and `hi-Latn` turns up in the slice because its
/// chain runs `hi-Latn → en-IN → en-001 → en`. Those are our locales seen through CLDR's
/// own geography, not someone else's language, and this function is how the invariant tells
/// that apart from data that has no business being here.
pub fn fallback_chain(code: &str) -> Vec<String> {
    use icu_provider::DataLocale;

    let fallbacker = icu_locale_fallback::LocaleFallbacker::new().for_config(Default::default());
    let locale: DataLocale = code
        .parse()
        .unwrap_or_else(|_| panic!("{code} is not a locale identifier"));
    let mut chain = Vec::new();
    let mut step = fallbacker.fallback_for(locale);
    while !step.get().is_unknown() {
        chain.push(step.get().to_string());
        step.step();
    }
    chain.push("und".to_owned());
    chain
}
