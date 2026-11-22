//! What the unit tests in this crate wait for, and how long: the same bounds `tests/common`
//! gives the integration tests (that module is not visible from here, and these are
//! `#[cfg(test)]` only).
//!
//! Every bound in a test that depends on when the scheduler runs a process is a bet on the
//! machine: an idle laptop wins it, a CI box with the rest of a build beside it, or twelve busy
//! loops sharing the core at a better priority (AUS-45, AUS-47), does not, and the test then
//! fails with nothing wrong in the product. A wait that ends the moment its condition holds
//! costs nothing when the bound is long, so these are long.

use std::time::Duration;

/// How long a test waits for something that must happen, and the budget it gives a product
/// deadline whose length is not what the test is about: two minutes, not never.
pub const PATIENT: Duration = Duration::from_secs(120);

/// A sleeper that outlives [`PATIENT`], for a fake's descendant that must still be running when
/// the call returns: "the call waited for it" is then told from "the call was slow".
pub const OUTLIVES_PATIENT: &str = "150";

/// Runs `attempt` with `first`, then with four times that, and so on up to [`PATIENT`], until it
/// returns `Some`; panics if even [`PATIENT`] was not enough.
///
/// For a test whose subject is a short deadline that must expire only after a fake program has
/// got as far as the thing under test: no fixed short deadline can promise that on a machine
/// that does not run the fake for seconds. `attempt` builds everything it needs afresh, runs the
/// call under the deadline it is given, and returns `None` if the deadline won the race.
pub async fn grown<T>(first: Duration, mut attempt: impl AsyncFnMut(Duration) -> Option<T>) -> T {
    let mut deadline = first;
    loop {
        if let Some(seen) = attempt(deadline).await {
            return seen;
        }
        assert!(
            deadline < PATIENT,
            "the fake never got as far as the test needs, even with {deadline:?}"
        );
        deadline = (deadline * 4).min(PATIENT);
    }
}
