# The CLDR slice that the formatters in `i18n` read (ADR 0021): generating it, and keeping
# the committed copy honest.
# Import with: mod i18n '<relative-path-to-repo-root>/just/i18n.just'
#
# `check-data` belongs in `just check-repo`, beside `proto::check`, for the same reason
# `proto::check` is there: `crates/i18n-data/src/data` is committed so that a plain
# `cargo build` needs no generator, no CLDR download and no network — and nothing in
# `cargo build` or `cargo test` notices when the committed bytes stop being what the
# generator produces. `cargo test -p i18n --features format` proves the payload is *usable*;
# only this proves it is current.

repo := source_directory() / ".."

# (internal) generate the slice into DIR, which must not exist yet
#
# The invocation is the definition of the slice, so it lives in one recipe that both
# `generate-data` and `check-data` call: a check whose regeneration differs from the
# generation it checks is worse than no check, since it reports the payload as stale
# forever, or — if only one of the two lines drifted — as current.
[no-cd]
[script('bash')]
_generate-to dir:
    set -euo pipefail
    # A `[script]` recipe's parameter is interpolated into the script, not passed
    # positionally; one assignment here keeps the rest of the body reading a plain bash
    # variable.
    out='{{ dir }}'
    cd '{{ repo }}'
    if ! command -v icu4x-datagen >/dev/null 2>&1; then
        echo '  {{ style("error") }}✗ icu4x-datagen is not on PATH{{ NORMAL }} — it is a dev-shell package (nix/icu4x-datagen.nix), so this recipe runs inside `devenv shell`' >&2
        exit 1
    fi
    # What `crates/i18n/src/format.rs` asks the provider for, which is the whole of the
    # slice: date and time patterns and the glue between their halves, the month, day-period
    # and year names, decimal symbols and digits, and cardinal plural rules.
    #
    # A marker missing here cannot be missed at runtime, and does not need to be checked for:
    # `format.rs` builds its formatters with `try_new_unstable(&CLDR, …)`, so a marker the
    # provider does not implement is a compile error there. What the list below *is* checked
    # for is agreement with `impl_*!(Cldr, ITER)` in `crates/i18n-data/src/lib.rs`, which
    # implements the iterable half of each marker so the locale invariant can read it; a
    # marker generated and not implemented is dead weight with nothing comparing the two.
    markers=(
        DatetimePatternsDateGregorianV1
        DatetimePatternsTimeV1
        DatetimeNamesMonthGregorianV1
        DatetimeNamesDayperiodV1
        DatetimeNamesYearGregorianV1
        DatetimePatternsGlueV1
        DecimalSymbolsV1
        DecimalDigitsV1
        PluralsCardinalV1
    )
    flags=()
    for marker in "${markers[@]}"; do flags+=(--markers "$marker"); done

    # The locale list is derived, never typed: the file stems under `crates/i18n/locales` are
    # exactly what `i18n-keygen` turns into `Locale::ALL`, so this recipe and that build
    # script cannot be handed different lists. `und` is absent from it and present in the
    # output — datagen always cuts the root locale, because every fallback chain ends there.
    mapfile -t locales < <(find crates/i18n/locales -name '*.toml' -exec basename {} .toml \; | sort -u)
    if [ "${#locales[@]}" -eq 0 ]; then
        echo '  {{ style("error") }}✗ no locale files under crates/i18n/locales{{ NORMAL }} — refusing to generate an empty slice' >&2
        exit 1
    fi

    # CLDR does not float (see versions.toml). `--cldr-tag latest` would make `check-data`
    # depend on Unicode's release schedule: the payload would go stale between two runs of
    # the same commit, and the diff would name ninety files and no cause.
    #
    # `ICU4X_CLDR_ROOT` points datagen at an already-unpacked release instead of a tag to
    # download, and is what lets this recipe run inside a Nix build sandbox, which has no
    # network: nix/cldr.nix fetches the pinned zip once into the store and nix/ci.nix sets
    # the variable to it. Same release either way — the store path is built from this same
    # `tools.cldr-json` value — so the two spellings cannot disagree about which CLDR they
    # mean, and a laptop with the variable unset downloads exactly what CI fetched.
    source_args=()
    if [ -n "${ICU4X_CLDR_ROOT:-}" ]; then
        source_args+=(--cldr-root "$ICU4X_CLDR_ROOT")
    else
        cldr=$(just --justfile '{{ source_directory() }}/lib.just' _version tools.cldr-json)
        source_args+=(--cldr-tag "$cldr")
    fi

    echo '{{ style("command") }}i18n generate-data:{{ NORMAL }}'
    icu4x-datagen "${flags[@]}" --locales "${locales[@]}" "${source_args[@]}" \
        --format baked --use-separate-crates --out "$out"

    # The length comparison is the whole check, and it is a length because a set difference
    # between a Rust macro list and a bash one costs a parser each. It disagrees in exactly
    # one direction silently (a marker renamed in both lists at once), and that rename still
    # has to survive `cargo build`.
    impls=$(grep -c '^impl_[a-z_0-9]*!(Cldr, ITER);$' crates/i18n-data/src/lib.rs || true)
    if [ "$impls" != "${#markers[@]}" ]; then
        echo "  {{ style("error") }}✗ marker lists disagree{{ NORMAL }}: this recipe passes ${#markers[@]}; crates/i18n-data/src/lib.rs implements $impls." >&2
        echo '    the generated payload and the provider that reads it must name the same markers' >&2
        exit 1
    fi

    echo "  ✓ $(du -sh "$out" | cut -f1) of CLDR for ${#locales[@]} locales and ${#markers[@]} markers"

# regenerate `crates/i18n-data/src/data` — run this when a locale is added to the catalog,
# when a formatter needs a new marker, or when `icu4x-datagen` or the CLDR pin moves
#
# Writes through a temporary directory and swaps it in only after the generator succeeded,
# because datagen needs the network (it downloads CLDR, and has no offline switch that this
# repository uses) and a half-written `src/data` is a crate that does not compile — which
# reads like a broken editor session rather than like a failed download.
[no-cd]
[script('bash')]
generate-data:
    set -euo pipefail
    cd '{{ repo }}'
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    just --justfile '{{ source_file() }}' _generate-to "$tmp/data"
    rm -rf crates/i18n-data/src/data
    mkdir -p crates/i18n-data/src/data
    cp -R "$tmp/data/." crates/i18n-data/src/data/
    echo '  ✓ crates/i18n-data/src/data rewritten — commit it with whatever prompted it'

# fail if the committed slice is not what this recipe generates
#
# Compares a snapshot against a fresh generation rather than asking git, for the reason
# spelled out in `just proto::check`: `git diff` says nothing about untracked files, so a
# git-based check passes silently before the generated tree is first committed.
[no-cd]
[script('bash')]
check-data:
    set -euo pipefail
    cd '{{ repo }}'
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    cp -R crates/i18n-data/src/data "$tmp/committed"
    just --justfile '{{ source_file() }}' _generate-to "$tmp/generated"
    if ! diff -r "$tmp/committed" "$tmp/generated" >/dev/null 2>&1; then
        echo '  {{ style("error") }}✗ crates/i18n-data/src/data is not what the generator produces{{ NORMAL }}' >&2
        diff -rq "$tmp/committed" "$tmp/generated" >&2 || true
        echo '  run `just i18n::generate-data` and commit the result' >&2
        exit 1
    fi
    echo '  ✓ the CLDR slice matches its generator'
