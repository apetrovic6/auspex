//! Stitches the two generated trees together.
//!
//! Messages and service stubs are generated into separate directories because both
//! plugins emit a per-package `<pkg>.mod.rs` and would otherwise collide. This file is
//! hand-written and deliberately lives outside both `out` directories, so
//! `clean: true` in `proto/buf.gen.yaml` does not remove it.

/// Protobuf message types.
pub mod buffa;

/// Connect RPC service stubs — client and server halves.
pub mod connect;
