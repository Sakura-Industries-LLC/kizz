// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `kizz build` when threads cannot start ("Kizz never panics"), end to end, with
//! the test-only variable `KIZZ_TEST_FAIL_THREAD_STARTS`, which makes the first
//! attempts to start a thread in the `kizz` process fail with `EAGAIN`, and whose
//! retries record their waits instead of sleeping.
//!
//! Each run gets an explicit environment that holds only `KIZZ_CACHE_DIR`, set to a
//! temporary cache, `PATH`, which the tasks inherit, and the variable when a test
//! sets it.

use std::{
    error::Error,
    fs,
    path::Path,
    process::{Command, Stdio},
};

use kizz_store::LOCK_FILE_NAME;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The variable that makes thread starts fail on purpose.
const FAIL_THREAD_STARTS: &str = "KIZZ_TEST_FAIL_THREAD_STARTS";

/// The `PATH` every run of `kizz` gets, and its tasks inherit.
const PATH: &str = "/usr/bin:/bin";

/// What one run of `kizz` produced.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    /// The exit code, or `None` when a signal killed the process.
    status: Option<i32>,
    /// Standard output, with invalid UTF-8 replaced.
    stdout: String,
    /// Standard error, with invalid UTF-8 replaced.
    stderr: String,
}

/// Write a project into `root` with two tasks of target `ci` in the wrong order, so
/// its build runs commands in two passes.
fn write_project(root: &Path) -> TestResult {
    fs::write(
        root.join("kizz.md"),
        [
            "# Use\n\n```starlark ci",
            "env.inherit(\"PATH\")",
            "run(\"cat\", \"out.txt\")",
            "```\n\n# Make\n\n```starlark ci",
            "env.inherit(\"PATH\")",
            "run(\"sh\", \"-c\", \"echo made > out.txt\")",
            "```\n",
        ]
        .join("\n"),
    )?;
    Ok(())
}

/// Run `kizz --color never --no-banner build ci` in `project` with the cache in
/// `cache`, and with `failures` in [`FAIL_THREAD_STARTS`] when it is given.
fn build(
    project: &Path,
    cache: &Path,
    failures: Option<&str>,
) -> TestResult<Run> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kizz"));
    command
        .args(["--color", "never", "--no-banner", "build", "ci"])
        .env_clear()
        .env("KIZZ_CACHE_DIR", cache)
        .env("PATH", PATH)
        .current_dir(project)
        .stdin(Stdio::null());
    if let Some(failures) = failures {
        command.env(FAIL_THREAD_STARTS, failures);
    }
    let output = command.output()?;
    Ok(Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// A build whose first 3 attempts to start a thread fail succeeds with the output
/// and status that it has without them, and never exits with status 101.
#[test]
fn a_build_whose_first_thread_starts_fail_succeeds_as_without_them() -> TestResult {
    // Two copies of the project, each with a cache of its own, so both builds start
    // from nothing and run every command.
    let plain_project = tempfile::tempdir()?;
    let plain_root = plain_project.path().canonicalize()?;
    write_project(&plain_root)?;
    let retried_project = tempfile::tempdir()?;
    let retried_root = retried_project.path().canonicalize()?;
    write_project(&retried_root)?;
    let plain_cache = tempfile::tempdir()?;
    let retried_cache = tempfile::tempdir()?;

    let plain = build(&plain_root, plain_cache.path(), None)?;
    assert_eq!(plain.status, Some(0), "{plain:?}");
    let retried = build(&retried_root, retried_cache.path(), Some("3"))?;
    assert_eq!(retried, plain);
    assert_ne!(retried.status, Some(101));
    assert!(!retried.stderr.contains("panicked"), "{retried:?}");
    assert!(!retried_cache.path().join(LOCK_FILE_NAME).exists());
    Ok(())
}

/// A build whose every attempt to start a thread fails exits with status 2, prints
/// one error that names the thread and error 11 and no panic, and leaves no lock
/// file.
#[test]
fn a_build_whose_every_thread_start_fails_exits_with_status_2() -> TestResult {
    let project = tempfile::tempdir()?;
    let root = project.path().canonicalize()?;
    write_project(&root)?;
    let cache = tempfile::tempdir()?;

    let run = build(&root, cache.path(), Some(&u32::MAX.to_string()))?;
    assert_eq!(run.status, Some(2), "{run:?}");
    assert_ne!(run.status, Some(101));
    assert_eq!(run.stdout, "", "{run:?}");
    let lines: Vec<&str> = run.stderr.lines().collect();
    assert_eq!(lines.len(), 1, "{run:?}");
    let error = lines.first().copied().unwrap_or_default();
    assert!(
        error.contains(
            "could not start the thread that handles Ctrl-C after 11 attempts in 22.3 seconds: "
        ),
        "{run:?}"
    );
    assert!(error.ends_with("(os error 11)"), "{run:?}");
    assert!(!run.stderr.contains("panicked"), "{run:?}");
    assert!(!cache.path().join(LOCK_FILE_NAME).exists());
    Ok(())
}
