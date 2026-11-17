// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Linux tracer when its threads cannot start ("Kizz never panics"), with the
//! failures of `kizz_threads::testing`, which retry without sleeping.
//!
//! When the tracing thread, the feeder, an output reader or the watchdog cannot start
//! even after its retries, the run returns `TraceError::Tracer` with a message that
//! names the thread and error 11, and no process of the request is left; with fewer
//! failures than that, the run gives the result that it gives without them.
//!
//! This is a test executable of its own, with one test, so that no other test's
//! processes are children of this process while it checks that none is left.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: a plain std thread bounds how long a run may take, and the tests \
              assert, lock and print"
)]

use std::{
    collections::BTreeMap, error::Error, ffi::OsString, fs, io, path::Path, sync::mpsc, thread,
    time::Duration,
};

use kizz_threads::{
    WAITS,
    testing::{Failure, fail_starts_named},
};
use kizz_trace::{
    AbsolutePath, Cancellation, CaptureMode, EchoSink, OutputStream, RunRequest, RunResult,
    TraceError, Tracer,
};
use kizz_trace_linux::LinuxTracer;

/// The result of a test helper.
type Outcome<T> = Result<T, Box<dyn Error>>;

/// How long a run may take: far less than the retries' waits, which are recorded
/// instead of slept.
const PATIENCE: Duration = Duration::from_secs(60);

/// Each thread of a run with split capture: its name and what its error calls it.
const SPLIT_THREADS: [(&str, &str); 5] = [
    ("kizz-tracer", "the tracing thread"),
    ("kizz-feeder", "the thread that feeds stdin"),
    ("kizz-stdout", "the thread that captures standard output"),
    ("kizz-stderr", "the thread that captures standard error"),
    ("kizz-watchdog", "the thread that watches for cancellation"),
];

/// The output reader of a run with merged capture: its name and what its error
/// calls it.
const MERGED_READER: (&str, &str) = ("kizz-output", "the thread that captures the merged output");

/// An echo sink that drops everything.
struct NoEcho;

impl EchoSink for NoEcho {
    fn echo(
        &self,
        _stream: OutputStream,
        _bytes: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }
}

/// A request to run a shell that reads its `stdin`, writes to both output streams
/// and exits with 3, with `capture`.
fn request(capture: CaptureMode) -> Outcome<RunRequest> {
    Ok(RunRequest {
        program: AbsolutePath::new(Path::new("/bin/sh"))?,
        args: ["sh", "-c", "cat >/dev/null; echo out; echo err >&2; exit 3"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        env: BTreeMap::from([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))]),
        cwd: AbsolutePath::new("/")?,
        stdin: Some(b"input\n".to_vec()),
        capture,
        echo: false,
        tree: false,
        terminal: false,
    })
}

/// Run `request` on a new thread whose next `count` attempts to start a thread
/// named `name` fail with `EAGAIN`, and return its result and the waits the retries
/// recorded, failing when the run does not end in time.
fn run_failing(
    request: &RunRequest,
    name: &str,
    count: u32,
) -> Outcome<(Result<RunResult, TraceError>, Vec<Duration>)> {
    let request = request.clone();
    let name = name.to_owned();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let failures = fail_starts_named(count, Failure::Busy, &name);
        let result = LinuxTracer::new().run(&request, &NoEcho, &Cancellation::new());
        sender.send((result, failures.waits())).ok();
    });
    Ok(receiver.recv_timeout(PATIENCE)?)
}

/// The process IDs of the processes whose parent is this process, zombies included.
fn children() -> Outcome<Vec<u32>> {
    let me = std::process::id();
    Ok(fs::read_dir("/proc")?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let (_, rest) = stat.rsplit_once(')')?;
                    rest.split_whitespace().nth(1)?.parse::<u32>().ok()
                })
                == Some(me)
        })
        .collect())
}

/// For each thread of a run in turn, a start that fails 11 times ends the run with a
/// tracer error that names the thread and error 11, after every wait, with no
/// process of the request left; a start that fails 10 times gives the run's result
/// as without failures, after 10 waits.
#[test]
fn threads_that_cannot_start_end_the_run_with_an_error() {
    let cases = SPLIT_THREADS
        .iter()
        .map(|&thread| (CaptureMode::Split, thread))
        .chain([(CaptureMode::Merged, MERGED_READER)]);
    for (capture, (name, what)) in cases {
        let request = request(capture).unwrap();
        let expected = LinuxTracer::new()
            .run(&request, &NoEcho, &Cancellation::new())
            .unwrap();
        assert_eq!(expected.termination, kizz_trace::Termination::Exited(3));
        assert_eq!(children().unwrap(), Vec::<u32>::new());

        let (result, waits) = run_failing(&request, name, 11).unwrap();
        let Err(TraceError::Tracer { message, .. }) = result else {
            panic!("{name}: expected a tracer error, got {result:?}");
        };
        assert!(
            message.starts_with(&format!(
                "could not start {what} after 11 attempts in 22.3 seconds: "
            )),
            "{name}: {message}"
        );
        assert!(message.ends_with("(os error 11)"), "{name}: {message}");
        assert_eq!(waits, WAITS, "{name}");
        assert_eq!(
            children().unwrap(),
            Vec::<u32>::new(),
            "{name}: a process was left"
        );

        let (result, waits) = run_failing(&request, name, 10).unwrap();
        let result = result.unwrap();
        assert_eq!(result.termination, expected.termination, "{name}");
        assert_eq!(result.output, expected.output, "{name}");
        assert_eq!(waits, WAITS[..10], "{name}");
        assert_eq!(
            children().unwrap(),
            Vec::<u32>::new(),
            "{name}: a process was left"
        );
    }
}
