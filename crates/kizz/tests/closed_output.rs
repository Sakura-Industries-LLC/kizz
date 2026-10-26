// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of output that cannot be written ("Kizz never panics").
//!
//! Each test runs the built binary with standard output, or standard error, on the
//! write end of a pipe whose read end the test closed before it started `kizz`, so
//! every write to that stream fails, as it does for `kizz list | head -1` once
//! `head` has exited. `kizz` must go on with its work, exit with the status that
//! the same run gives with an open output, and never panic: no run exits with
//! status 101, and no stream that the test reads says "panicked".
//!
//! Each test writes a project into one temporary directory and gives every run of
//! `kizz` a cache in another. Every run gets an explicit environment that holds only
//! `KIZZ_CACHE_DIR`, set to that cache, and `PATH`, which the tasks inherit to find
//! their programs.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{
    error::Error,
    fs, io,
    path::PathBuf,
    process::{Command, Stdio},
};

use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The `PATH` every run of `kizz` gets, and its tasks inherit.
const PATH: &str = "/usr/bin:/bin";

/// The words of a Rust panic message, which no stream may hold.
const PANICKED: &str = "panicked";

/// The exit status of a Rust process that panicked.
const PANIC_STATUS: i32 = 101;

/// The task of every project: it prints with `print`, and runs a command whose
/// output kizz echoes live with `stream = True`, so a build writes a task
/// program's line, the echo of a command, and its own lines.
const KIZZ_MD: &str = concat!(
    "# Echo\n",
    "\n",
    "```starlark ci\n",
    "env.inherit(\"PATH\")\n",
    "print(\"from the program\")\n",
    "run(\"sh\", \"-c\", \"echo streamed; echo also >&2\", stream = True)\n",
    "```\n",
);

/// The summary of a build whose one step came from the cache.
const CACHED: &str = "build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached";

/// Which standard stream of `kizz` goes to a pipe that nobody reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closed {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// What one run of `kizz` produced on the stream that the test read.
#[derive(Debug)]
struct Run {
    /// The exit code, or `None` when a signal killed the process.
    status: Option<i32>,
    /// The stream that was not closed, with invalid UTF-8 replaced: standard
    /// error when standard output was closed, and the other way around; both,
    /// one after the other, for a run with no stream closed.
    read: String,
}

impl Run {
    /// Check that the run did not panic: no status 101 and no panic message on a
    /// stream that the test read.
    fn check_no_panic(&self) {
        assert_ne!(self.status, Some(PANIC_STATUS), "{self:?}");
        assert!(!self.read.contains(PANICKED), "{self:?}");
    }
}

/// A project with one task and an empty cache, each in a temporary directory.
struct Project {
    /// The project directory.
    _dir: TempDir,
    /// The project directory, with every symbolic link resolved.
    root: PathBuf,
    /// The cache directory.
    cache: TempDir,
}

impl Project {
    /// A project whose `kizz.md` holds [`KIZZ_MD`], and an empty cache.
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        fs::write(root.join("kizz.md"), KIZZ_MD)?;
        Ok(Self {
            _dir: dir,
            root,
            cache: tempfile::tempdir()?,
        })
    }

    /// The command that runs `kizz` with `args` in the project, with the project's
    /// cache, `PATH`, and no other environment.
    fn command(
        &self,
        args: &[&str],
    ) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kizz"));
        command
            .args(args)
            .env_clear()
            .env("KIZZ_CACHE_DIR", self.cache.path())
            .env("PATH", PATH)
            .current_dir(&self.root)
            .stdin(Stdio::null());
        command
    }

    /// Run `kizz` with `args` and both streams open, reading both.
    fn run_open(
        &self,
        args: &[&str],
    ) -> TestResult<Run> {
        let output = self.command(args).output()?;
        let mut read = String::from_utf8_lossy(&output.stdout).into_owned();
        read.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(Run {
            status: output.status.code(),
            read,
        })
    }

    /// Run `kizz` with `args` and the stream `closed` on the write end of a pipe
    /// whose read end is closed before `kizz` starts, reading the other stream.
    fn run_closed(
        &self,
        args: &[&str],
        closed: Closed,
    ) -> TestResult<Run> {
        let (reader, writer) = io::pipe()?;
        drop(reader);
        let mut command = self.command(args);
        match closed {
            Closed::Stdout => command.stdout(writer).stderr(Stdio::piped()),
            Closed::Stderr => command.stdout(Stdio::piped()).stderr(writer),
        };
        let output = command.output()?;
        let read = match closed {
            Closed::Stdout => output.stderr,
            Closed::Stderr => output.stdout,
        };
        Ok(Run {
            status: output.status.code(),
            read: String::from_utf8_lossy(&read).into_owned(),
        })
    }
}

/// Run `args` in a fresh project, first after `before` with both streams open and
/// then, in another fresh project, after the same `before` with the stream
/// `closed`; check that the closed run exits with the open run's status, 0, and
/// that neither panicked.
fn same_status_when_closed(
    args: &[&str],
    before: &[&[&str]],
    closed: Closed,
) -> TestResult {
    let open = Project::new()?;
    for earlier in before {
        open.run_open(earlier)?.check_no_panic();
    }
    let with_open = open.run_open(args)?;
    with_open.check_no_panic();
    assert_eq!(with_open.status, Some(0), "{with_open:?}");

    let project = Project::new()?;
    for earlier in before {
        project.run_open(earlier)?.check_no_panic();
    }
    let with_closed = project.run_closed(args, closed)?;
    with_closed.check_no_panic();
    assert_eq!(with_closed.status, with_open.status, "{with_closed:?}");
    Ok(())
}

/// `kizz list` with standard output closed, as in `kizz list | head -1` after
/// `head` exited, lists nothing that anyone reads and exits with status 0, as it
/// does with an open output, with and without its banner.
#[test]
fn list_with_a_closed_standard_output_exits_as_with_an_open_one() -> TestResult {
    same_status_when_closed(&["--color", "never", "list"], &[], Closed::Stdout)?;
    same_status_when_closed(&["--no-banner", "list"], &[], Closed::Stdout)
}

/// `kizz list` with standard error closed exits with status 0.
#[test]
fn list_with_a_closed_standard_error_exits_as_with_an_open_one() -> TestResult {
    same_status_when_closed(&["--color", "never", "list"], &[], Closed::Stderr)
}

/// `kizz why` with standard output closed, after a build, exits with status 0.
#[test]
fn why_with_a_closed_standard_output_exits_as_with_an_open_one() -> TestResult {
    let build: &[&str] = &["--no-banner", "build", "ci"];
    same_status_when_closed(&["--color", "never", "why"], &[build], Closed::Stdout)
}

/// `kizz why` with standard error closed, after a build, exits with status 0.
#[test]
fn why_with_a_closed_standard_error_exits_as_with_an_open_one() -> TestResult {
    let build: &[&str] = &["--no-banner", "build", "ci"];
    same_status_when_closed(&["--color", "never", "why"], &[build], Closed::Stderr)
}

/// `kizz build` with standard output closed, whose task prints with `print` and
/// echoes a command's output live with `stream = True`, succeeds with status 0;
/// its step is cached as with an open output, so the next build, with an open
/// output, runs nothing and says that the step came from the cache.
#[test]
fn build_with_a_closed_standard_output_succeeds_and_caches_its_step() -> TestResult {
    same_status_when_closed(&["--color", "never", "build", "ci"], &[], Closed::Stdout)?;
    let project = Project::new()?;
    let closed = project.run_closed(&["--color", "never", "build", "ci"], Closed::Stdout)?;
    closed.check_no_panic();
    assert_eq!(closed.status, Some(0), "{closed:?}");
    let again = project.run_open(&["--color", "never", "--no-banner", "build", "ci"])?;
    again.check_no_panic();
    assert_eq!(again.status, Some(0), "{again:?}");
    assert!(
        again.read.lines().any(|line| line == CACHED),
        "the step was not cached: {again:?}"
    );
    Ok(())
}

/// `kizz build` with standard error closed, whose command also writes to standard
/// error, succeeds with status 0, and its step is cached.
#[test]
fn build_with_a_closed_standard_error_succeeds_and_caches_its_step() -> TestResult {
    same_status_when_closed(&["--color", "never", "build", "ci"], &[], Closed::Stderr)?;
    let project = Project::new()?;
    let closed = project.run_closed(&["--color", "never", "build", "ci"], Closed::Stderr)?;
    closed.check_no_panic();
    assert_eq!(closed.status, Some(0), "{closed:?}");
    let again = project.run_open(&["--color", "never", "--no-banner", "build", "ci"])?;
    again.check_no_panic();
    assert!(
        again.read.lines().any(|line| line == CACHED),
        "the step was not cached: {again:?}"
    );
    Ok(())
}
