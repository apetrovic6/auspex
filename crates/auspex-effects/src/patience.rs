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

/// How many times slower than an idle machine this one starts a process right now, from 1 to
/// 10: the worst of three spawns of `sh -c :`, per 100 ms. The same measure `tests/common` makes.
fn load_factor() -> u32 {
    let worst = (0..3)
        .map(|_| {
            let began = std::time::Instant::now();
            let _ = std::process::Command::new("sh").args(["-c", ":"]).status();
            began.elapsed()
        })
        .max()
        .unwrap_or_default();
    u32::try_from(worst.as_millis() / 100)
        .unwrap_or(10)
        .clamp(1, 10)
}

/// The slack an upper bound on a call that should return promptly gets: 2 s, stretched by how
/// loaded the machine is ([`load_factor`]).
pub fn slack() -> Duration {
    Duration::from_secs(2) * load_factor()
}

/// Whether the machine is too busy for an upper bound on how long a call took to mean anything:
/// more runnable threads than two per core (`/proc/loadavg`, Linux only), or a process start that
/// takes over 400 ms. Never inside a Nix build, where a skip would be a false green.
fn too_busy_to_read_a_clock(what: &str) -> bool {
    if std::env::var_os("NIX_BUILD_TOP").is_some()
        || std::env::var_os("AUSPEX_TEST_WINDOWS_ALWAYS").is_some()
    {
        return false;
    }
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let queued = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());
    #[expect(clippy::cast_precision_loss, reason = "a core count")]
    let crowded = queued.is_some_and(|q| q > 2.0 * cores as f64);
    let busy = crowded || load_factor() >= 4;
    if busy {
        eprintln!("SKIPPED: {what} measures a wall clock bound, and this machine is too busy");
    }
    busy
}

/// Asserts that a call that took `took` took less than `bound`, unless the machine is too busy
/// for that to mean anything (never inside a Nix build). `tests/common`'s `assert_within`: the
/// upper bound that `took < PATIENT` does not make, since that says only that the product did
/// not wait for what the fake would do if the deadline were ignored.
pub fn assert_within(took: Duration, bound: Duration, what: &str) {
    assert!(
        took < bound || too_busy_to_read_a_clock(what),
        "{what}: took {took:?}, which is not under {bound:?}"
    );
}
