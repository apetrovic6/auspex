# 0020. An instant crosses the wire as milliseconds, and the shell tells you what day it is

- **Status:** Accepted
- **Date:** 2026-10-04

## Context

The counter has had a second fact since the first migration: `updated_at`, a
`timestamp with time zone`, `NOT NULL DEFAULT CURRENT_TIMESTAMP`, set explicitly by every
write statement (ADR 0003 put the schema here, and the schema kept the moment before this
repository had anywhere to put it). It never crossed the wire. `CounterState` carried
`count`, every shell drew a number, and the screen answered *what* and never *when*.

ADR 0016, 0017 and 0018 drew the arc this walks along: meaning crosses the boundary and
the shell decides how it reads; an `i64` crosses as an `i64` and every locale writes its
own digits; a plural family crosses as one key and CLDR picks its member. A date is the
same shape of problem and a worse specimen of it. `count` was safe to carry raw because
every shell at least renders *a* number; `1_759_478_400_123` interpolates just as
silently through the same `{}` and renders something nobody can read as anything. Worse,
it is not a date in any calendar until two more facts are supplied that it does not
carry: which zone splits it into a day, and which language spells that day. The server
owns the instant and knows neither of the other two; the shell owns both of the other
two and did not have the first. That is the whole reason `updated_at` could sit in the
database for its whole life without ever reaching a screen — every single place that had
part of the answer could have printed a wrong one alone.

The wire had three obvious shapes available and they are on the rejection list below, so
it is worth naming the one taken first: **`int64 updated_at_unix_ms` in `CounterState`,
`0` meaning never** — epoch milliseconds, no zone, no calendar, no format, no sentinel
string. The service converts once, where the row arrives: `RETURNING count, updated_at`
and `timestamp_millis()`. The core stores it and passes it through, `Model` to
`ViewModel`, and never interprets it. `crates/i18n` gains one formatting function behind
the `format` feature:

```rust
pub fn datetime(timestamp_ms: i64, utc_offset_seconds: i32, locale: Locale) -> String
```

and one small function beside it, `local_utc_offset_seconds(timestamp_ms) -> i32`,
because the offset is the second clock and the wire cannot carry it. It is asked *of the
reader's zone at that instant*, via chrono's `Local` (with `wasmbind`, so the wasm shell
gets the browser's zone and not UTC), which keeps daylight saving honest: an instant
stored before a DST boundary and read after it is still rendered on the day it was in.
An offset that cannot exist lands on Greenwich rather than failing — a wrong-by-hours
date is the failure mode, but a blank screen is the alternative, and nobody's clock zone
has ever been 12:45.

Inside, the formatter is a `FixedCalendarDateTimeFormatter<Gregorian, _>` with
`fieldsets::YMDT::medium()` — the *fixed* calendar type, because the epoch constructor
produces an `Iso` civil date and CLDR's names are keyed on `Gregorian`, which fixed-
calendar typing makes a compile-time fact instead of a runtime `None` for a calendar
nobody shipped. The sentence is one common key, `updated_at = "Updated %{when}"`, and
`%{when}` is filled by the formatted string and never by anything else.

Six call sites, because that is how many shells this app has. Each holds the same
`updated_text` shape: `0` renders the empty string and the row keeps its place in the
layout — the screen does not jump when the first reading lands, exactly as with the
status row; anything else renders the catalog sentence around `datetime(..)`. The web
shell gets its offset from the browser through chrono's `wasmbind`; Android calls
`java.time` (`Instant.ofEpochMilli`, `ZoneId.systemDefault()`,
`ofLocalizedDateTime(MEDIUM, MEDIUM)` against the configuration locale) and Apple calls
Foundation (`Date(timeIntervalSince1970:)`, `.formatted(date:time:)`) — each platform's
own CLDR, the same split ADR 0017 accepted for numbers and ADR 0018 restated. The Apple
line is UNVERIFIED, like every line in that file: nothing on Linux compiles Swift.

## Measured first, decided second

The plan for this change carried a measurement gate, because ADR 0017 had already
established what the ICU stack costs and a date formatter is the same CLDR asked a much
harder question. Numbers first (before = `d2c48f3`, x86_64-linux, the same three
artifacts as ADR 0017 and 0018):

| artifact | before | after | delta |
|---|---|---|---|
| `counter-tui` release | 5,232,992 B | 5,761,880 B | +528,888 (+10.1%) |
| `counter-tui` release, stripped | 3,657,304 B | 4,130,408 B | +473,104 (+12.9%) |
| `counter-web` `.wasm` — what a browser downloads | 874,940 B | 1,189,219 B | +314,279 (+35.9%) |

**Thirty-six percent on the browser artifact — roughly four and a half times what ADR
0017 put there (+7.9%), and it was argued about before it was merged rather than
after.** The
gate offered three ways to pay less and the repository took none of them: narrowing the
ICU datetime surface (a smaller field set, or `icu_datetime` without the data the field
sets share — measured before choosing to accept the full `medium` date-and-time), swapping
the web shell alone to `Intl.DateTimeFormat` (free in a browser, and rejected below), or
not shipping dates on web at all (a shell that shows *when* only on some platforms is
two products). The cost is recorded in this table because it is permanent: every future
locale added to `locales/` drags a little more of this blob with it, and the honest number
for "what does the next formatted thing cost" starts here, not at 0017's.

**ADR 0021 re-measured this table and recovered 230,944 of the +314,279** by slicing the ICU
data to the locales `locales/` actually ships instead of carrying every locale Unicode
publishes. It changed no formatting decision, and none of the three options above — all of
which were about *what gets formatted* — is it. The numbers in this table stay as measured at
this commit, which is the before-picture; read 0021 for the after one.

`Cargo.lock` moves **1,163 → 1,172 packages (+9)** — `icu_datetime` and what it pulls,
behind the `format` feature like everything else ICU. No second version of anything, the
same rule ADR 0017 held.

## Consequences

**`instants_are_formatted_at_the_edge` is a stricter invariant than ADR 0017's, and the
difference is the failure it prevents.** The numbers rule says a `%{count}` argument must
exist; a raw number through it still *renders*, just in the wrong locale's costume. Here
the rule is about the argument's expression, not its presence: every catalog key whose
placeholders include `when` must be filled at every call site by an expression containing
a `datetime` call. A raw `1_759_478_400_123` in the sentence renders as milliseconds,
which is not a date in any costume — it is a number wearing a date's clothes, and no
`Display` impl is at fault. The check reuses the same `translate_with!` scanner, prose
guard and catalog pairing as every other invariant, and asks one question of the `when`
segment. It is deliberately not a type: a helper that hands a pre-formatted string to a
sentence is exactly how an instant travels unformatted into one, so the rule binds the
argument position, where the call can be guaranteed. Its own fixtures are built with
`format!` rather than written out, because unlike the plural fixtures this check walks
*every* `.rs` file under the root — which live is this repository — so a literal raw call
staged as an example would fail `cargo xtask check` exactly as the failure it tests fails
it. What it cannot see is Kotlin and Swift: the platform shells fill their resources'
placeholders through `java.time` and `Date.formatted`, and no Rust scanner reads them —
the same limit as every check here, stated rather than worked around.

**The service cannot mock its own database, and that is a sea-orm fact worth recording.**
The natural test — drive the handler against `MockDatabase`, assert the response carries
the row's instant — does not compile in this crate: sea-orm derives `Clone` for
`DatabaseConnection` under `cfg_attr(not(feature = "mock"), …)`, and `startup.rs` clones
the connection into every service, so enabling the feature fails to build the crate that
wanted it. What is asserted instead closes the chain link by link around the two links
that are not this repository's code: `every_statement_names_the_instant_it_asks_back`
pins the named statement constants to still `RETURNING`/`SELECT` the column the code then
`try_get`s (a `RETURNING` edited down to `count` compiles, passes CI — which has no
database to run it against — and fails in production on every write), and
`the_moment_a_response_carries_is_the_moment_it_was_handed` pins `state()` to carry a
real instant rather than re-zero it. What remains uncovered is Postgres evaluating
`now()` and sea-orm extracting a `timestamp with time zone` — database and library
behaviour, and an always-zero `updated_at` passing every other gate in the repository is
precisely the failure this section exists to make loud.

**Android grew a mapping, and it is tested.** The hand-written `CounterApiClient` that
speaks Connect for the core in Kotlin now copies the whole `CounterState` into
`CounterApiResult.Ok`, where it used to pick `count` out — the same wholesale-copy rule
the Rust client states, and for the same reason: a selector that reaches past the state
to grab one field is where a field the contract added goes missing quietly. Its fake
service now stamps a non-zero instant, and its nine assertions carry the stamp, so a
mapping that drops `updatedAtUnixMs` fails a test rather than rendering an empty date
line forever. `:app:lintDebug` runs clean over the change (no new warnings; the seven
that report are the template's unused colors and version nagging), and the shared
module's unit tests pass against the generated `updatedAtUnixMs` field — which is also
the proof, the kind that outlives this record, that the typegen lowercases the wire's
snake_case into Kotlin's camelCase rather than anything a comment has to assert.

**`0` is a shared word with one meaning, and only one code path can produce it.** The
contract, the core, and all six shells agree that `updated_at_unix_ms == 0` means *never*
— the shells blank the row. Only `read()` on an empty table synthesises the pair; the
schema itself never produces it (`NOT NULL DEFAULT CURRENT_TIMESTAMP`), so the sentinel
is not hiding a real 1970 reading. `i64` milliseconds overflow in year 292 million, so
no clock this contract describes wraps into it.

**The template ships the sentence and not the call, and says so.** `updated_at` is in
`locales/common/`, so every scaffolded product's resources carry it; the skeleton's core
is pure — no clock, no effect, no honest instant to stamp — and an always-zero field in
a skeleton would pass every shell silently forever. The absence is documented where the
field would be (`templates/crux-app`'s `ViewModel` docs, beside the `status` paragraph
that has carried the same sentence since ADR 0016), and `apps/counter/core` holds the
shape to copy when a skeleton core grows a moment.

**The en/medium formatter prints seconds, and the tests pin the space before `AM` with
an escape.** `crates/i18n`'s tests assert `"Oct 3, 2025, 8:00:00\u{202f}AM"` — the
narrow no-break space ICU's data declares, typed as an escape because a plain space in
the test source would pass for either and lose the difference forever. Croatian renders
the same instant as `3. lis 2025. 08:00:00`: 24-hour, abbreviated month, and a
`-12h` offset moves it to `2. lis`, which is the offset argument proving what it is for.
An impossible offset lands on Greenwich — pinned, because the fallback is a decision.

**Checked counts.** `cargo xtask check` moves from **277 invariants at `d2c48f3`** to
**294 here**; `versions` stays at 41. The seventeen are, itemised:
`instants_are_formatted_at_the_edge` counts **6** — its own six `%{when}` call sites
(three shells, two in `crates/i18n`'s sentence tests, and one fixture sentence inside
xtask, which the walk reads whether or not it was assembled for the purpose); the
all-call-sites check `translate_with_arguments_match_the_catalog` counts the same six
again, **+6**, because those are its units; `app_paths_outside_apps_are_reachable` grew
**+1** because this record names `apps/counter/core` and every such pointer outside
`apps/` is checked for reachability; `decision_records_are_indexed` grew
**+2** for this record and its index link; and the Swift-identifier check grew **+2**
for the names the new field generates there. No number in that list is a rounding
estimate — the totals were attributed by running the registered checks, not by adding
them up after the fact.

## What was rejected, and why

**`google.protobuf.Timestamp`.** The contract's own answer for an instant, and wrong
here three times over. It would put a *message* where a scalar belongs — every reader
becomes `Option`-like (`MessageField`) for a value that is never absent-but-unset, and
`just proto::generate`'s codegen maps it to a seconds/nanos pair that each of six shells
would reassemble by hand, which is six chances to disagree about the nanos it then throws
away (nothing in this contract has sub-millisecond meaning: the schema stores
microseconds and every consumer stops at milliseconds). proto3 JSON additionally renders
it as an RFC3339 string while binary renders the pair, so the wire would have two
spellings of the same field depending on codec, and the repository pins protobuf as its
codec without wanting the contract to care. Epoch `i64` is the one spelling that arrives
identically through every codec and lands directly in `Date(timeIntervalSince1970:)`,
`Instant.ofEpochMilli`, `Date.now` and chrono — the four constructors this change's own
consumers are.

**ISO-8601 strings on the wire.** A date-shaped string is a date-shaped *string*: nothing
parses it, so every shell formats over it (or worse, shows it), every locale's date is
`2025-10-03`, and the type system is fully complicit. It is ADR 0016's exact rejected
object with a nicer pedigree.

**Formatting server-side, in the core, or anywhere before the shell.** The reader's zone
lives on the phone and the browser; the server formatting means shipping one reader's day
to all readers — a Stockholm server telling a Zagreb phone that a 01:30 write happened
today when it happened yesterday. The core formatting is the same sin one hop earlier,
and ADR 0016 already tried the general case.

**`Intl.DateTimeFormat` in the web shell.** Free, because the browser carries the CLDR.
Rejected for the third consecutive time, for the third consecutive identical reason
(ADR 0017 numbers, ADR 0018 plurals): two implementations of one locale's calendar
agreeing by coincidence, and no test this repository runs drives a browser, so the
coincidence breaking is invisible here. It was raised again at the gate *because* 35.9%
is a real number, and it lost again for the same reason: the bytes are a cost, the
silent locale disagreement is a defect.

**A typed placeholder — `%{when:datetime}` in the catalog.** Same rejection as ADR 0017's
typed markers, restated because this is where typed markers look strongest: the invariant
above gets the enforcement without new template vocabulary, and translators keep one
placeholder syntax they can see they must not fill with anything numeric.

**Dynamic construction, or `icu_datetime`'s non-fixed calendar surface.** `datetime()`
could build a `DateTimeFormatter` generically over calendars and answer `None` (or a
runtime `Iso`-named surprise) at runtime; it builds the fixed-calendar `Gregorian`
formatter instead, so the calendar the epoch milliseconds land in is a type. Per-call
construction is the cost paid there, and it is measured, not assumed away — it is inside
the +35.9% above; a cached-formatter design was not worth the lifetime knot in a function
with this call rate (one per render per shell).

**Nothing at all.** `updated_at` has been in the schema the whole time, which makes doing
nothing look free. It is the one option that leaves the repository with a fact it stores,
a contract that hides, and six shells that could have shown it — and the next reader of
`CounterState` will not know which of those was a decision.
