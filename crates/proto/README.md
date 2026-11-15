# `proto` — the Rust binding for the wire contract

Generated from `proto/*.proto`. Do not edit anything under `src/gen/`.

    just proto::generate    # regenerate from the .proto files
    just proto::check       # fail if the committed output has drifted
    just proto::lint        # buf lint
    just proto::breaking    # compare against the default branch

The generated output is committed so a plain `cargo build` needs no buf, no protoc and no
network. `proto::check` runs in CI, so the committed copy cannot silently drift.

## Other languages

The same contract is generated for the shells into `proto/gen/kotlin` and
`proto/gen/swift` — protobuf types plus a Connect client, from `buf.build` remote plugins.
Those are the clients the Android and Apple shells call the API with, so no shell
hand-writes Connect and none can drift from the contract.

`just proto::check` compares every generated tree, not just this one. The shell languages
are the ones most likely to rot: nothing in `cargo build` touches them, so a `.proto`
change nobody regenerated would otherwise surface only at an Android or Apple build.
