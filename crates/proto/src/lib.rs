//! The wire contract, generated from `proto/`.
//!
//! Nothing in [`r#gen`] is written by hand — `just proto::generate` produces it and
//! `just proto::check` fails if the committed copy has drifted from the `.proto` files.
//! The output is committed so that a plain `cargo build` needs no buf, no protoc and no
//! network.
//!
//! The module is `r#gen` rather than `gen` because `gen` is a reserved keyword in edition
//! 2024; the code generator escapes it the same way.

// The workspace lint set is tuned for code people write and maintain. Applied to
// generated output it produces findings nobody can act on -- a manual Debug impl that
// omits a field, a missing `#[must_use]` -- because the fix would have to be made in the
// generator and would be overwritten on the next run. The generator emits its own
// targeted allows; this covers the rest.
//
// `unsafe_code` is deliberately NOT relaxed: the workspace forbids it, `forbid` cannot be
// downgraded by an `allow`, and it is the property actually worth enforcing over code
// that is not read line by line.
#![allow(clippy::pedantic, clippy::nursery)]

/// Mount point for the Connect services.
///
/// Lives here rather than in the server because both halves need it: the server nests its
/// router at this prefix, and every generated client derives its request URLs from it. A
/// constant in one of them would be a literal in the other.
pub const API_PREFIX: &str = "/api";

pub mod r#gen;
