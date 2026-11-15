//! One test binary for the whole suite.
//!
//! Submodules rather than separate `tests/*.rs` files, because each top-level file in
//! `tests/` is compiled and linked as its own binary — a real cost once there are a dozen
//! of them.

mod auth;
mod counter;
mod health;
mod helpers;
mod observability;
mod reflection;
mod web;
