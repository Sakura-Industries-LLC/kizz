// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz` command.
//!
//! In builds with `debug_assertions` only, which tests use and release builds lack,
//! the variable `KIZZ_TEST_FAIL_THREAD_STARTS`, a number N, makes the first N
//! attempts to start a thread in the process fail with `EAGAIN`, through
//! `kizz_threads::testing`, so the end-to-end tests can show that kizz retries, and
//! that it ends with an error instead of a panic when the retries run out. A release
//! build ignores the variable.

use std::process::ExitCode;

#[cfg(debug_assertions)]
use kizz::environment::FAIL_THREAD_STARTS;

/// Parse the command line, run the selected command, and exit with its status.
fn main() -> ExitCode {
    #[cfg(debug_assertions)]
    fail_thread_starts();
    kizz::cli::run()
}

/// Make the first attempts to start a thread fail, as many as
/// [`FAIL_THREAD_STARTS`] says, when it holds a number.
#[cfg(debug_assertions)]
fn fail_thread_starts() {
    let count = std::env::var_os(FAIL_THREAD_STARTS)
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u32>().ok()));
    if let Some(count) = count {
        kizz_threads::testing::fail_starts_in_process(count);
    }
}
