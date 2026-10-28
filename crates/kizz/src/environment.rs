// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The environment variables that kizz reads from its own environment.
//!
//! [`VARIABLES`] lists every variable that changes what the `kizz` command does or
//! prints: those that kizz's own code reads, through the constants that the code
//! uses, and those that its terminal library, `console`, reads to decide whether to
//! style output under `--color=auto`. [`DEVELOPER_VARIABLES`] lists the variables
//! that only the tests of kizz and its debug builds read. The reference documents
//! every variable of both lists, and its coverage test checks that the code reads
//! no variable that the lists lack.

use kizz_store::location::{HOME, KIZZ_CACHE_DIR, XDG_CACHE_HOME};

/// The variable that makes thread starts fail on purpose; only debug builds, which
/// tests use, read it.
pub const FAIL_THREAD_STARTS: &str = "KIZZ_TEST_FAIL_THREAD_STARTS";

/// The variable that makes kizz fault outside every stack guard right after it
/// installs its handler of stack overflows; only debug builds, which tests use,
/// read it.
pub const TEST_FAULT: &str = "KIZZ_TEST_FAULT";

/// The variable that lets the tracer's tests skip the checks of system calls that
/// the tracer never saw; only those tests read it, never the `kizz` command.
pub const SKIP_BLOCKED_SYSCALLS: &str = "KIZZ_TEST_SKIP_BLOCKED_SYSCALLS";

/// Every variable that the `kizz` command reads from its own environment: the three
/// that locate the cache, `HOME` also for `~` in output and in path patterns; the
/// `PATH` in which `kizz execute` looks up its program; and `NO_COLOR`, `CLICOLOR`,
/// `CLICOLOR_FORCE` and `TERM`, which `console` reads to decide whether to style
/// standard output and standard error under `--color=auto`.
pub const VARIABLES: [&str; 8] = [
    KIZZ_CACHE_DIR,
    XDG_CACHE_HOME,
    HOME,
    kizz_core::PATH_VARIABLE,
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "TERM",
];

/// The variables that only the tests of kizz and its debug builds read, which the
/// reference documents for developers.
pub const DEVELOPER_VARIABLES: [&str; 3] = [SKIP_BLOCKED_SYSCALLS, FAIL_THREAD_STARTS, TEST_FAULT];
