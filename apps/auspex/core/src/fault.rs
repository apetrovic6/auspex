//! A panic inside the core's `update`, for the shell's tests (feature `fault-injection`).
//!
//! crux's `process` holds the model's write lock while `update` runs, so a panic in it poisons the
//! lock and every later call into the core panics too. That is what a bug in the core, reached
//! from a task that delivers an adapter's answer, does to a run; nothing else can reproduce it.
//! The core has no panic site of its own, hence this switch.
//!
//! The switch is process-wide, and tests run in parallel, so it names **which core** it is for:
//! the one whose configured flake is the given string.

use std::sync::Mutex;
use std::thread::ThreadId;

/// The flake of the core to break, and the thread that is spared.
static ARMED: Mutex<Option<(String, ThreadId)>> = Mutex::new(None);

/// Arms the fault: the next `update` of the core whose flake is `flake`, on a thread **other than
/// this one**, panics, once. The thread that arms it is the shell's loop in the test, whose own
/// updates must not be the ones.
pub fn arm_except_this_thread(flake: &str) {
    *ARMED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((flake.to_owned(), std::thread::current().id()));
}

/// Called at the top of `update`, with the flake the core was started with.
pub(crate) fn check(flake: Option<&str>) {
    let mut armed = ARMED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if armed.as_ref().is_some_and(|(f, spared)| {
        Some(f.as_str()) == flake && *spared != std::thread::current().id()
    }) {
        *armed = None;
        drop(armed);
        panic!("fault injected by a test: the core's update panics");
    }
}
