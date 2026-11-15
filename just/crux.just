# Crux code generation: turning a core crate into foreign-language bindings.
# Import with: mod crux '<relative-path-to-repo-root>/just/crux.just'
#
# There are two distinct generators and they used to disagree about the working
# directory — `boltffi pack` ran from the core crate while `cargo run --bin codegen` ran
# from the shell directory, so the same "generated" path meant two different places.
# Both recipes now take ABSOLUTE paths (callers build them from `justfile_directory()`),
# which makes them correct from any cwd and removes the ambiguity entirely.
#
#   typegen -> serde/bincode type bindings (Event, ViewModel, Effect) via crux_core
#   pack    -> the FFI surface + prebuilt native libraries via boltffi

# Generate serde type bindings for one language into an absolute output directory.
[no-cd]
typegen core_pkg language out_dir:
    @echo '{{ style("command") }}typegen ({{ language }}):{{ NORMAL }} {{ out_dir }}'
    RUST_LOG=${RUST_LOG:-info} cargo run --package {{ core_pkg }} --bin codegen \
        --features codegen,facet_typegen -- \
        --language {{ language }} --output-dir '{{ out_dir }}'

# Package native artefacts (xcframework/SPM, jniLibs) for one target.
# Runs from the core crate directory, because that is where boltffi.toml lives and every
# path inside it is relative to that file.
[no-cd]
[script('bash')]
pack core_dir target:
    set -euo pipefail
    echo '{{ style("command") }}pack ({{ target }}):{{ NORMAL }} {{ core_dir }}'
    cd '{{ core_dir }}'
    boltffi pack {{ target }}

# Remove everything both generators produce for an app.
[no-cd]
[script('bash')]
clean-generated *dirs:
    set -euo pipefail
    for d in {{ dirs }}; do
        echo "  removing $d"
        rm -rf "$d"
    done
