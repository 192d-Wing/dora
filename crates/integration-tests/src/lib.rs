//! Out-of-process integration tests for the split dora services.
//!
//! This crate's `tests/` harness spawns the `dora-migrate`, `dora-v4`,
//! `dora-v6`, and `dora-api` binaries as child processes. The small helpers the
//! individual test binaries share live here (rather than duplicated per test
//! file) so a fix to binary resolution or the throwaway-runtime helper is made
//! in one place.

use std::env;

/// Absolute path to a workspace binary (e.g. `dora-v4`), resolved from the test
/// executable's own location. `env!("CARGO_BIN_EXE_...")` is unavailable here
/// because this test crate does not (and cannot) depend on the binary-only
/// service crates; instead the test exe lives at `target/<profile>/deps/<exe>`,
/// so the sibling service binaries are two directories up.
pub fn bin_path(name: &str) -> String {
    let mut path = env::current_exe().expect("failed to resolve current exe");
    path.pop(); // drop the test executable's file name
    if path.file_name().is_some_and(|n| n == "deps") {
        path.pop(); // drop `deps/`, leaving target/<profile>/
    }
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    path.push(file);
    path.to_string_lossy().into_owned()
}

/// Run a future to completion on a throwaway current-thread runtime (the harness
/// itself is synchronous; only the DB provisioning is async).
pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime")
        .block_on(fut)
}
