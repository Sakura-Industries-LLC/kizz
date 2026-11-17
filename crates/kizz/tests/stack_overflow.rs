// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `kizz build` and `kizz run` when a task program overflows kizz's stack ("Kizz
//! never panics"), end to end: kizz catches the overflow, prints an error that names
//! the task, deletes the lock file and exits with status 2, where Rust's runtime
//! would abort it, and the next build runs normally. With the test-only variable
//! `KIZZ_TEST_FAULT`, kizz faults outside every stack guard, and the handler hands
//! that fault on, so kizz dies of `SIGSEGV`.
//!
//! Each run starts `kizz` through `sh -c` with `ulimit -c 0` and `ulimit -s 2048`, so
//! the stack limit is the same wherever the tests run and no core file is written.
//! Each run gets an explicit environment that holds only `KIZZ_CACHE_DIR`, set to a
//! temporary cache, `PATH`, and the variable when a test sets it, and the project is
//! written into a temporary directory.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert"
)]

use std::{
    error::Error,
    fs,
    os::unix::process::ExitStatusExt as _,
    path::PathBuf,
    process::{Command, Stdio},
};

use kizz_store::LOCK_FILE_NAME;
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The `PATH` every run of `kizz` gets.
const PATH: &str = "/usr/bin:/bin";

/// The variable that makes kizz fault outside every stack guard.
const TEST_FAULT: &str = "KIZZ_TEST_FAULT";

/// The script that starts `kizz`, its first argument, with the rest of its
/// arguments, under a stack limit of 2,048 KiB and without core files.
const LIMITED: &str = "ulimit -c 0 && ulimit -s 2048 && exec \"$0\" \"$@\"";

/// How many lists the `deep` task nests: far more than four times the depth of
/// 1,385 at which a run of the built `kizz` first overflowed under the same limit,
/// as the step's implementation log records.
const DEPTH: u32 = 100_000;

/// The error that kizz prints when the program of the `deep` task, whose block
/// opens on line 3 under the heading "Deep", overflows the stack.
const MESSAGE: &str = "error: the program of task kizz.md:3 (Deep) overflowed the stack, so kizz stopped; its \
                       values are probably nested too deeply";

/// What Rust's runtime prints when it aborts after an overflow.
const ABORTS: [&str; 2] = ["fatal runtime error", "has overflowed its stack"];

/// What one run of `kizz` produced.
#[derive(Debug)]
struct Run {
    /// The exit code, or `None` when a signal killed the process.
    status: Option<i32>,
    /// The signal that killed the process, if one did.
    signal: Option<i32>,
    /// Standard output, with invalid UTF-8 replaced.
    stdout: String,
    /// Standard error, with invalid UTF-8 replaced.
    stderr: String,
}

/// A project with the tasks `deep` and `ok`, and a cache, each in a temporary
/// directory.
struct Project {
    /// The project directory.
    _dir: TempDir,
    /// The project directory, with every symbolic link resolved.
    root: PathBuf,
    /// The cache directory.
    cache: TempDir,
}

impl Project {
    /// Write the project: the task `deep`, of target and name `deep`, whose top-level
    /// loop wraps a list in a list [`DEPTH`] times and is followed by another
    /// top-level statement, so that Starlark's garbage collector copies the nesting;
    /// and the task `ok`, of target and name `ok`, whose program only prints.
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        fs::write(
            root.join("kizz.md"),
            format!(
                "# Deep\n\n```starlark deep name=deep\nnested = []\nfor _ in range({DEPTH}):\n    nested = \
                 [nested]\nprint(\"built\")\n```\n\n# Ok\n\n```starlark ok name=ok\nprint(\"ok\")\n```\n"
            ),
        )?;
        Ok(Self {
            _dir: dir,
            root,
            cache: tempfile::tempdir()?,
        })
    }

    /// Run `kizz --color never --no-banner` with `args` under the stack limit, with
    /// [`TEST_FAULT`] set when `fault` is.
    fn kizz(
        &self,
        args: &[&str],
        fault: bool,
    ) -> TestResult<Run> {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                LIMITED,
                env!("CARGO_BIN_EXE_kizz"),
                "--color",
                "never",
                "--no-banner",
            ])
            .args(args)
            .env_clear()
            .env("KIZZ_CACHE_DIR", self.cache.path())
            .env("PATH", PATH)
            .current_dir(&self.root)
            .stdin(Stdio::null());
        if fault {
            command.env(TEST_FAULT, "1");
        }
        let output = command.output()?;
        Ok(Run {
            status: output.status.code(),
            signal: output.status.signal(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// The cache's lock file.
    fn lock_file(&self) -> PathBuf {
        self.cache.path().join(LOCK_FILE_NAME)
    }
}

/// Require that `run` stopped cleanly after the overflow: status 2, not a signal,
/// the message that names the task, and nothing of an abort or a panic.
fn assert_stopped_cleanly(run: &Run) {
    assert_eq!(run.status, Some(2), "{run:?}");
    assert_eq!(run.signal, None, "{run:?}");
    assert!(
        run.stderr.lines().any(|line| line == MESSAGE),
        "the message is missing: {run:?}"
    );
    for abort in ABORTS {
        assert!(!run.stderr.contains(abort), "{abort:?}: {run:?}");
    }
    assert!(!run.stderr.contains("panicked"), "{run:?}");
}

/// Require that `kizz build ok` in `project` succeeds after an overflow, and that
/// its task printed.
fn assert_next_build_succeeds(project: &Project) -> TestResult {
    let next = project.kizz(&["build", "ok"], false)?;
    assert_eq!(next.status, Some(0), "{next:?}");
    assert!(next.stdout.lines().any(|line| line == "ok: ok"), "{next:?}");
    Ok(())
}

/// `kizz build deep` stops with status 2 and the message that names the task, does
/// not abort, leaves no lock file, and the next build succeeds.
#[test]
fn kizz_build_stops_cleanly_when_a_program_overflows_the_stack() -> TestResult {
    let project = Project::new()?;
    let run = project.kizz(&["build", "deep"], false)?;
    assert_stopped_cleanly(&run);
    assert!(
        !project.lock_file().exists(),
        "the lock file is left: {run:?}"
    );
    assert_next_build_succeeds(&project)
}

/// `kizz run deep` stops with status 2 and the message that names the task, does
/// not abort, leaves no lock file, and the next build succeeds.
#[test]
fn kizz_run_stops_cleanly_when_a_program_overflows_the_stack() -> TestResult {
    let project = Project::new()?;
    let run = project.kizz(&["run", "deep"], false)?;
    assert_stopped_cleanly(&run);
    assert!(
        !project.lock_file().exists(),
        "the lock file is left: {run:?}"
    );
    assert_next_build_succeeds(&project)
}

/// A fault outside every stack guard is handed on, never swallowed: with
/// [`TEST_FAULT`], kizz dies of `SIGSEGV` and prints neither its own message nor
/// that of Rust's runtime.
#[test]
fn a_fault_outside_every_stack_guard_is_handed_on() -> TestResult {
    let project = Project::new()?;
    let run = project.kizz(&["build", "ok"], true)?;
    assert_eq!(run.signal, Some(libc::SIGSEGV), "{run:?}");
    assert_eq!(run.status, None, "{run:?}");
    assert!(!run.stderr.contains("overflowed"), "{run:?}");
    for abort in ABORTS {
        assert!(!run.stderr.contains(abort), "{abort:?}: {run:?}");
    }
    assert!(!run.stderr.contains("panicked"), "{run:?}");
    Ok(())
}
