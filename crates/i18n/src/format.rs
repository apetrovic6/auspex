//! Locale-aware formatting, behind the `format` feature: numbers, plurals, instants.
//!
//! The catalog is dumb on purpose (ADR 0017): `count_is = "Count is: %{count}"` says
//! nothing about what kind of value fills the placeholder, and `translate_with!`
//! interpolates whatever `Display` produces — which for an `i64` is `1234567` in every
//! locale, including the ones that owe the reader `1.234.567`. Formatting is therefore
//! the shell's job, *before* the interpolation, and this is the Rust shells' one way to
//! do it. Kotlin has `NumberFormat`, Swift has `.formatted()`; both are the platform's
//! own CLDR, which is the right answer on a phone and a deliberate second opinion next
//! to this one (ADR 0017).

use chrono::{Local as ChronoLocal, TimeZone as _};
use icu_datetime::input::{UtcOffset, ZonedDateTime};
use icu_datetime::{DateTimeFormatterPreferences, FixedCalendarDateTimeFormatter, fieldsets};
use icu_decimal::DecimalFormatter;
use icu_decimal::input::Decimal;
use icu_decimal::options::DecimalFormatterOptions;
use icu_decimal::preferences::DecimalFormatterPreferences;
use icu_locale_core::Locale as IcuLocale;
use icu_plurals::{PluralCategory, PluralRules, PluralRulesPreferences};

use crate::{Key, Locale};

// The CLDR data this crate formats with, and the only way to get it: sliced to the locales
// `locales/` ships, in a crate of its own, by `try_new_unstable` rather than `try_new`
// (ADR 0021).
//
// The const is private on purpose. A provider argument on `number`, `datetime` and
// `plural_key` would put a databake artifact on the public surface of ADR 0017 and 0018
// for no reason a caller can act on: there is no second data set for anyone to choose
// between.
const CLDR: i18n_data::Cldr = i18n_data::CLDR;

/// `value` written the way `locale` writes numbers.
///
/// `1_234_567` is `1.234.567` in Croatian and `1,234,567` in English; the separators,
/// their positions and the digit shapes are CLDR's answer for the locale, not a rule
/// this repository states. Integers only — currency and percent remain out of scope for
/// the same reason plurals are (ADR 0017); dates and times are not, and got their own
/// function rather than a wider `number` (ADR 0020).
///
/// Hands the result to [`crate::translate_with!`] as a pre-formatted string:
///
/// ```
/// # use i18n::{number, translate_with, Locale};
/// assert_eq!(
///     translate_with!("Count is: %{count}", Locale::FALLBACK, count = number(1234, Locale::En)),
///     "Count is: 1,234"
/// );
/// ```
///
/// The formatter is built per call rather than cached: it is a handful of pointers into
/// statically compiled data, and a shell that renders in a loop holds its `Locale` for
/// the run, so there is nothing to invalidate. A cache would be a `static` map from two
/// locales today, and would still be wrong the moment a product adds a third.
///
/// # Panics
///
/// Only if the *root* locale's data is missing from the slice, which cannot happen quietly:
/// without `compiled_data` the constructors below are the only ones, and they do not compile
/// against a provider that lacks a marker. An unparsable or data-less locale falls back to
/// the root formatter, and that one has nowhere left to fall back to.
#[must_use]
pub fn number(value: i64, locale: Locale) -> String {
    // A code `locales/` happens to name but ICU cannot parse gets the root locale's
    // formatting rather than a panic: a shell calls this mid-frame, and ungrouped
    // digits on screen are a visible degradation where a dead renderer is not. The
    // generated `Locale` codes are directory names, which nothing checks are BCP 47.
    let preferences = locale
        .as_str()
        .parse::<IcuLocale>()
        .map_or_else(|_| DecimalFormatterPreferences::default(), IcuLocale::into);

    // Within the slice this fails only where the data genuinely lacks an entry — an exotic
    // `-u-nu-` numbering system, not a language — and the root locale is always present, so
    // the fallback chain terminates.
    let formatter =
        DecimalFormatter::try_new_unstable(&CLDR, preferences, DecimalFormatterOptions::default())
            .or_else(|_| {
                DecimalFormatter::try_new_unstable(
                    &CLDR,
                    DecimalFormatterPreferences::default(),
                    DecimalFormatterOptions::default(),
                )
            })
            .expect("the ICU data slice covers the root locale");

    formatter.format(&Decimal::from(value)).to_string()
}

/// The reader's own UTC offset, in seconds, at `timestamp_ms` — the shell's half of
/// [`datetime`]'s second argument.
///
/// Asked at the instant, not at `now`, because the reader's offset is not constant:
/// a summer instant and a winter instant in the same shell can be an hour apart, and
/// a stamp resolved against today's offset renders an hour out in March and October.
/// Where the machine gives no answer (a zone the platform cannot name) the instant
/// gets UTC and stays a usable date.
///
/// This is the same shape ADR 0015 gave locale detection: the shell owns the decision,
/// and the crate holds the one platform call behind a feature, so the wasm case (`Local`
/// is UTC without `wasmbind`; the browser's zone with it) is solved once here rather
/// than in every shell that renders an instant.
#[must_use]
pub fn local_utc_offset_seconds(timestamp_ms: i64) -> i32 {
    ChronoLocal
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map_or(0, |instant| instant.offset().local_minus_utc())
}

/// An instant — epoch milliseconds — written the way `locale` writes dates and times,
/// at the reader's `utc_offset_seconds`.
///
/// The instant arrives from the wire as `i64` milliseconds and stays that way to this
/// function (ADR 0020); what happens here is civil rendering, and it needs a second
/// input the catalog and the wire cannot supply: where the reader is. An epoch
/// millisecond is not a date until an offset says which one — the same instant is
/// tonight in Zagreb and tomorrow in Tokyo — so the offset is a parameter and the
/// shell, which knows where it is running, supplies it (ADR 0020).
///
/// The formatter is the fixed-calendar one with a static field set: `YMDT` at medium
/// length — `Oct 3, 2025, 8:00:00 AM` in English (the narrow no-break space before `AM`
/// is CLDR's doing, pinned in the tests because it is invisible everywhere else),
/// `3. lis 2025. 08:00:00` in Croatian — both from CLDR, neither stated here.
/// `DateTimeFormatter`'s runtime calendar and dynamic field sets link data this
/// repository would never use, and the field set is the dial the size is measured on
/// (ADR 0020).
///
/// # Panics
///
/// As [`number`]. A `utc_offset_seconds` outside UTC±24h is not a panic: the reader
/// cannot be there, and the instant gets Greenwich's date rather than a dead frame.
#[must_use]
pub fn datetime(timestamp_ms: i64, utc_offset_seconds: i32, locale: Locale) -> String {
    let preferences = locale
        .as_str()
        .parse::<IcuLocale>()
        .map_or_else(|_| DateTimeFormatterPreferences::default(), IcuLocale::into);

    // The epoch constructor builds an `Iso` civil date; CLDR's names, and therefore
    // this formatter's data, are keyed on `Gregorian`. Same days, different name.
    let formatter = FixedCalendarDateTimeFormatter::<icu_calendar::Gregorian, _>::try_new_unstable(
        &CLDR,
        preferences,
        fieldsets::YMDT::medium(),
    )
    .or_else(|_| {
        FixedCalendarDateTimeFormatter::<icu_calendar::Gregorian, _>::try_new_unstable(
            &CLDR,
            DateTimeFormatterPreferences::default(),
            fieldsets::YMDT::medium(),
        )
    })
    .expect("the ICU data slice covers the root locale");

    let offset = UtcOffset::try_from_seconds(utc_offset_seconds).unwrap_or(UtcOffset::zero());
    let when = ZonedDateTime::from_epoch_milliseconds_and_utc_offset(timestamp_ms, offset);
    formatter
        .format(&ZonedDateTime {
            date: when.date.to_calendar(icu_calendar::Gregorian),
            time: when.time,
            zone: when.zone,
        })
        .to_string()
}

/// Which of a language's number forms a count takes.
///
/// The names are CLDR's, and so is the set: English uses `One` and `Other`, Croatian
/// `One`, `Few` and `Other`, Polish adds `Many`, Japanese has `Other` alone. That is why
/// a plural is not a variant per category in Rust (ADR 0018) — a `Key::ApplesFew` would
/// be a variant that exists or not depending on which locales are compiled in, and
/// `Key` is one type for all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Zero,
    One,
    Two,
    Few,
    Many,
    Other,
}

impl Category {
    /// CLDR's name for the category, which is what a locale file spells a plural member
    /// with: `apples.few`, `jabuke`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::One => "one",
            Self::Two => "two",
            Self::Few => "few",
            Self::Many => "many",
            Self::Other => "other",
        }
    }

    const fn from_icu(category: PluralCategory) -> Self {
        match category {
            PluralCategory::Zero => Self::Zero,
            PluralCategory::One => Self::One,
            PluralCategory::Two => Self::Two,
            PluralCategory::Few => Self::Few,
            PluralCategory::Many => Self::Many,
            PluralCategory::Other => Self::Other,
        }
    }
}

/// The cardinal plural `count` takes in `locale`.
///
/// `2` is `Other` in English and `Few` in Croatian; `21` is `Other` in English and `One`
/// in Croatian, because Croatian asks about the last digit and English asks about the
/// number. Nothing in this repository states those rules, which is the entire point: a
/// hand-written version is wrong for the languages nobody in the room speaks, and the
/// rules are a few thousand lines of `n%100` ranges that stop being true when CLDR
/// revises them (ADR 0018).
///
/// This answers the category, not the string. Use it with [`plural_key`] to name a
/// translation, and with [`number`] to write the digits — a count in a translated
/// sentence is *two* uses of one value, and only the selection is a plural question.
///
/// ```
/// # use i18n::{Category, Locale, plural_category};
/// assert_eq!(plural_category(2, Locale::En), Category::Other);
/// assert_eq!(plural_category(2, Locale::Hr), Category::Few);
/// assert_eq!(plural_category(21, Locale::Hr), Category::One);
/// ```
#[must_use]
pub fn plural_category(count: i64, locale: Locale) -> Category {
    Category::from_icu(rules(locale).category_for(count))
}

/// The plural member of `key` that `count` takes in `locale` — `"apples.few"`.
///
/// The answer is a key string, not a translated string, and it is meant to be handed
/// straight to [`crate::translate_with!`] or [`crate::translate`], which take any
/// `AsRef<str>` and so accept it exactly as they accept a `Key`:
///
/// ```
/// # use i18n::{Key, Locale, number, plural_key, translate_with};
/// assert_eq!(plural_key(Key::Apples, 2, Locale::Hr), "apples.few");
///
/// assert_eq!(
///     translate_with!(
///         plural_key(Key::Apples, 5, Locale::Hr),
///         Locale::Hr,
///         count = number(5, Locale::Hr),
///     ),
///     "5 jabuka"
/// );
/// ```
///
/// Note what that call passes as the count: [`number(5, Locale::Hr)`](number), not `5`.
/// The category and the digits are two questions about one value, and only the first is
/// answered in Croatian by CLDR; the second is answered by `number`, because the catalog
/// stays dumb about what fills a placeholder (ADR 0017) and because the alternative is a
/// shell printing `1.234` in one sentence and `1,234` in the next.
///
/// # Panics
///
/// As [`plural_category`].
#[must_use]
pub fn plural_key(key: Key, count: i64, locale: Locale) -> String {
    format!(
        "{}.{}",
        key.as_str(),
        plural_category(count, locale).as_str()
    )
}

/// The cardinal categories `locale` has, in CLDR's order.
///
/// English has two, Croatian three, Japanese one. A coverage test uses this to ask
/// whether a catalog owes its readers an `apples.few` without knowing which of the
/// shipped languages happens to have a `few` — the question the emitters ask too, on the
/// other side of the build (ADR 0018).
///
/// ```
/// # use i18n::{Category, Locale, plural_categories};
/// assert_eq!(
///     plural_categories(Locale::Hr),
///     [Category::One, Category::Few, Category::Other]
/// );
/// ```
#[must_use]
pub fn plural_categories(locale: Locale) -> Vec<Category> {
    rules(locale).categories().map(Category::from_icu).collect()
}

/// `locale`'s cardinal rules, falling back to CLDR's root locale the way [`number`] does.
///
/// A code `locales/` names that ICU cannot parse is not a runtime condition worth
/// killing a frame over; the root rules answer `one` for 1 and `other` for everything
/// else, which is the least wrong sentence available.
///
/// # Panics
///
/// Only if the root locale's data is missing from the slice.
fn rules(locale: Locale) -> PluralRules {
    let preferences = locale.as_str().parse::<IcuLocale>().map_or_else(
        |_| PluralRulesPreferences::default(),
        PluralRulesPreferences::from,
    );

    PluralRules::try_new_cardinal_unstable(&CLDR, preferences)
        // The line above covers an unparsable code; this covers a parsed one whose data is
        // somehow absent, and lands on the root rules either way.
        .or_else(|_| {
            PluralRules::try_new_cardinal_unstable(&CLDR, PluralRulesPreferences::default())
        })
        .expect("the ICU data slice covers the root locale")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The proof this change exists for: the same `i64`, the two locales the catalog
    /// ships, and the separators CLDR says each of them writes. Croatian groups with a
    /// full stop, English with a comma; before this, both shells printed `1234567`.
    #[test]
    fn groups_thousands_in_the_locale_s_own_style() {
        assert_eq!(number(1_234_567, Locale::Hr), "1.234.567");
        assert_eq!(number(1_234_567, Locale::En), "1,234,567");
    }

    /// A counter spends most of its life under a thousand, and CLDR's grouping must not
    /// decorate what needs no decoration — in either direction, including the sign.
    #[test]
    fn leaves_small_numbers_alone() {
        assert_eq!(number(0, Locale::En), "0");
        assert_eq!(number(42, Locale::Hr), "42");
        assert_eq!(number(999, Locale::En), "999");
        assert_eq!(number(-42, Locale::En), "-42");
    }

    /// The categories CLDR gives the two locales the catalog ships, and the counts that
    /// show they are different questions. Croatian splits on the last digit: 21 is `one`
    /// and 22 is `few`, where English asks about the number and gets `other` twice.
    #[test]
    fn selects_the_categories_the_locale_has() {
        assert_eq!(
            plural_categories(Locale::En),
            [Category::One, Category::Other]
        );
        assert_eq!(
            plural_categories(Locale::Hr),
            [Category::One, Category::Few, Category::Other]
        );

        let english = |count| plural_category(count, Locale::En);
        assert_eq!(english(1), Category::One);
        assert_eq!(english(-1), Category::One);
        assert_eq!(english(0), Category::Other);
        assert_eq!(english(2), Category::Other);
        assert_eq!(english(21), Category::Other);

        let croatian = |count| plural_category(count, Locale::Hr);
        assert_eq!(croatian(1), Category::One);
        assert_eq!(croatian(2), Category::Few);
        assert_eq!(croatian(4), Category::Few);
        assert_eq!(croatian(5), Category::Other);
        assert_eq!(croatian(11), Category::Other);
        assert_eq!(croatian(21), Category::One);
        assert_eq!(croatian(22), Category::Few);
        assert_eq!(croatian(101), Category::One);
    }

    /// A counter goes below zero, and so does its noun.
    ///
    /// These were measured against the library rather than reasoned out of it, because the
    /// interesting ones are not the ones a rule of thumb gets right. CLDR defines its
    /// operands on the *absolute* value, so Croatian reads `-21` as if it were `21` and lands
    /// on `one`, exactly where `21` lands; `-2` is `few` there and `-5` is `other`. `0` is
    /// pinned with them because it is the count a reader is likeliest to meet and the one an
    /// intuition about "one means one" gets wrong — and `other` for zero is CLDR's answer in
    /// both shipped languages, not a fallback.
    ///
    /// Nothing here computes a remainder to check that against, which is the point of ADR
    /// 0018; the assertions are the record that the library was asked.
    #[test]
    fn a_count_below_zero_still_takes_a_category() {
        let english = |count| plural_category(count, Locale::En);
        assert_eq!(english(-1), Category::One);
        assert_eq!(english(-2), Category::Other);
        assert_eq!(english(-21), Category::Other);
        assert_eq!(english(0), Category::Other);

        let croatian = |count| plural_category(count, Locale::Hr);
        assert_eq!(croatian(-1), Category::One);
        assert_eq!(croatian(-2), Category::Few);
        assert_eq!(croatian(-5), Category::Other);
        // The same last two digits as 21, and the same category: the sign is not an operand.
        assert_eq!(croatian(-21), Category::One);
        assert_eq!(croatian(0), Category::Other);
    }

    /// What a negative count looks like once it is on the screen, in both languages.
    ///
    /// The Croatian expectations spell their minus sign `\u{2212}` — U+2212 MINUS SIGN, not
    /// the ASCII hyphen a `format!("{count}")` would print. That is CLDR's doing and not
    /// `icu_decimal`'s invention: the minus sign is locale data, so Croatian writes `-1` as
    /// `−1` while English writes it as `-1`, and the two sentences disagree about a character
    /// in a way no reader would call a bug. It is asserted here because it is invisible
    /// everywhere else: no shell writes a minus sign, and a change to `number` that made
    /// Croatian print an ASCII hyphen would read as a typo in this test rather than as a
    /// regression in what the reader sees.
    #[test]
    fn a_negative_count_arrives_with_the_locale_s_own_minus_sign() {
        assert_eq!(number(-1, Locale::Hr), "\u{2212}1");
        assert_eq!(number(-1_234, Locale::Hr), "\u{2212}1.234");
        assert_eq!(number(-1, Locale::En), "-1");

        assert_eq!(
            translate_with!(
                plural_key(Key::Apples, -1, Locale::Hr),
                Locale::Hr,
                count = number(-1, Locale::Hr),
            ),
            "\u{2212}1 jabuka",
            "`one`, because the sign is not an operand — the noun agrees with 1"
        );
        assert_eq!(
            translate_with!(
                plural_key(Key::Apples, -2, Locale::Hr),
                Locale::Hr,
                count = number(-2, Locale::Hr),
            ),
            "\u{2212}2 jabuke"
        );
        assert_eq!(
            translate_with!(
                plural_key(Key::Apples, 0, Locale::Hr),
                Locale::Hr,
                count = number(0, Locale::Hr),
            ),
            "0 jabuka"
        );
    }

    /// A plural in a sentence, end to end: a `Key` naming a family, the count that picks
    /// the category, and the same count written the way that locale writes it.
    ///
    /// The `1.234`/`1,234` pair is the point of the test — the family is chosen by CLDR and
    /// the digits are printed by `number`, and a shell that forgot the second half would
    /// pass every assertion here except this one.
    #[test]
    fn a_plural_renders_in_the_language_being_written() {
        let croatian = |count| {
            translate_with!(
                plural_key(Key::Apples, count, Locale::Hr),
                Locale::Hr,
                count = number(count, Locale::Hr),
            )
        };

        assert_eq!(croatian(1), "1 jabuka");
        assert_eq!(croatian(2), "2 jabuke");
        assert_eq!(croatian(5), "5 jabuka");
        assert_eq!(croatian(21), "21 jabuka");
        // 1234 is `few`: Croatian looks at the last two digits (12–14 aside), so it lands
        // with the twos while English puts it with the millions. Grouping and category from
        // the same value, in the same call.
        assert_eq!(croatian(1_234), "1.234 jabuke");
        assert_eq!(croatian(1_231), "1.231 jabuka");
        assert_eq!(croatian(1_235), "1.235 jabuka");

        let english = |count| {
            translate_with!(
                plural_key(Key::Apples, count, Locale::En),
                Locale::En,
                count = number(count, Locale::En),
            )
        };

        assert_eq!(english(1), "1 apple");
        assert_eq!(english(2), "2 apples");
        assert_eq!(english(1_234), "1,234 apples");
    }

    /// The proof this change exists for, same shape as [`groups_thousands_in_the_locale_s_own_style`]:
    /// one instant, the two locales the catalog ships, and what each of them writes for a date
    /// and a time. `1_759_478_400_123` is 2025-10-03T08:00:00.123Z — a Friday morning — and the
    /// two answers disagree about everything CLDR was asked: the order of the parts, the month
    /// name (`lis`, the abbreviation Croatian uses for October), the hour clock, the separators.
    ///
    /// The English string carries `\u{202f}` — U+202F NARROW NO-BREAK SPACE — between the time
    /// and `AM`. That is CLDR's separator, current data, and it is pinned with an escape rather
    /// than typed as a plain space because a plain space would make this test fail on the
    /// library rather than reveal what the reader is owed; nothing else in the repository can
    /// see the difference, and a change that made English print a regular space there would
    /// read as a typo rather than as what it is.
    #[test]
    fn writes_the_instant_the_way_the_locale_writes_a_date() {
        let instant = 1_759_478_400_123;
        assert_eq!(
            datetime(instant, 0, Locale::En),
            "Oct 3, 2025, 8:00:00\u{202f}AM"
        );
        assert_eq!(datetime(instant, 0, Locale::Hr), "3. lis 2025. 08:00:00");
    }

    /// The offset is not decoration: it is *which date* the instant is.
    ///
    /// The same milliseconds, three offsets. One hour moves only the hour; twelve hours
    /// west of Greenwich moves the day itself — `3. lis` becomes `2. lis` — which is the
    /// whole reason the offset is a parameter of [`datetime`] and not a constant (ADR 0020).
    /// Nothing here reads the machine it runs on, so these hold in Zagreb and in Tokyo.
    #[test]
    fn the_offset_says_which_day_the_instant_is() {
        let instant = 1_759_478_400_123;
        assert_eq!(
            datetime(instant, 3600, Locale::En),
            "Oct 3, 2025, 9:00:00\u{202f}AM"
        );
        assert_eq!(datetime(instant, 7200, Locale::Hr), "3. lis 2025. 10:00:00");
        assert_eq!(
            datetime(instant, -43200, Locale::Hr),
            "2. lis 2025. 20:00:00"
        );
    }

    /// A caller that hands over an impossible offset gets Greenwich rather than a panic or a
    /// garbage date: `UtcOffset::try_from_seconds` refuses anything outside ±24h, and the
    /// fallback is the one no reader can call wrong about a fellow reader.
    #[test]
    fn an_impossible_offset_lands_on_greenwich() {
        let instant = 1_759_478_400_123;
        assert_eq!(
            datetime(instant, 1_000_000, Locale::En),
            datetime(instant, 0, Locale::En)
        );
    }

    /// The sentence, end to end: the catalog key, the instant from the wire, the shell's
    /// zone, and the formatted date inside the translated frame.
    ///
    /// `Updated …` in English and `Ažurirano …` in Croatian are the two things this test
    /// pins together: that the key exists in `locales/common/` in both languages, and that
    /// `%{when}` is filled by something that is already a date. The digits-only variant of
    /// this assertion — `when = count.to_string()` — is what the xtask invariant
    /// `instants_are_formatted_at_the_edge` is built to refuse (ADR 0020).
    #[test]
    fn an_instant_arrives_in_the_sentence_already_read_as_one() {
        let instant = 1_759_478_400_123;
        assert_eq!(
            translate_with!(
                Key::UpdatedAt,
                Locale::En,
                when = datetime(instant, 0, Locale::En),
            ),
            "Updated Oct 3, 2025, 8:00:00\u{202f}AM"
        );
        assert_eq!(
            translate_with!(
                Key::UpdatedAt,
                Locale::Hr,
                when = datetime(instant, 7200, Locale::Hr),
            ),
            "Ažurirano 3. lis 2025. 10:00:00"
        );
    }
}
