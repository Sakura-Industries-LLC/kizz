// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz tracer for Linux.
//!
//! [`LinuxTracer`] implements the tracer interface of `kizz_trace` with `ptrace` and
//! a seccomp-bpf filter that stops a traced process only for the system calls that
//! touch the file system, [`TRACKED_SYSTEM_CALLS`], and lets every other call run at
//! full speed. Only the `kizz` binary wires it in.
//!
//! The tracer traces only the processes it starts itself: the child requests
//! tracing with `PTRACE_TRACEME` before it runs the program, and the tracer follows
//! the whole tree of processes and threads it starts, with the option that kills
//! all of them if the tracer exits. Nothing in the traced program changes, except
//! that `io_uring_setup` fails with `ENOSYS`, so its file accesses stay observable.
//!
//! At each tracked call the tracer reads the call's path arguments, directory
//! descriptors and flags from the stopped thread before the call runs, and its
//! result after it returns, and reports every path the tree touched as an absolute
//! path with its combined access kinds. Successful and failed calls are recorded
//! alike: a failed call, including a lookup that found nothing, is a probe.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the kizz tracer supports only Linux on x86_64");

mod capture;
mod decode;
mod filter;
mod follow;
mod spawn;
mod sys;

use std::{
    collections::BTreeSet,
    io,
    sync::mpsc::{self, RecvTimeoutError},
    thread::{self, ScopedJoinHandle},
    time::Duration,
};

pub use filter::{TRACKED_SYSTEM_CALLS, TrackedSystemCall};
use kizz_trace::{
    Accesses, Cancellation, EchoSink, OutputStream, RunRequest, RunResult, TraceError, Tracer,
};

use crate::{
    capture::TracerStdio,
    follow::LiveProcesses,
    spawn::{Program, Started},
};

/// How often the cancellation watchdog checks the cancellation flag.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Runs commands under `ptrace` with a seccomp-bpf filter in front.
///
/// Each call to [`Tracer::run`] starts and traces its own process tree from a new
/// thread that has no other child, so tracers on several threads of one process run
/// independently, and no run waits for a child that its caller started.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxTracer;

impl LinuxTracer {
    /// A Linux tracer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// The error for a failure of the tracer itself.
fn tracer_error(
    message: &str,
    source: io::Error,
) -> TraceError {
    TraceError::Tracer {
        message: message.to_owned(),
        source: Some(source),
    }
}

/// Wait for a helper thread of a run and return its result.
fn join<T>(
    handle: ScopedJoinHandle<'_, io::Result<T>>,
    what: &str,
) -> Result<T, TraceError> {
    match handle.join() {
        Ok(result) => result.map_err(|error| tracer_error(what, error)),
        Err(_) => {
            Err(TraceError::Tracer {
                message: format!("the thread {what} panicked"),
                source: None,
            })
        },
    }
}

/// Kill the tree when the request is cancelled, until `done` is dropped.
///
/// The event loop also kills every thread it sees after cancellation; the watchdog
/// covers a tree whose threads are all blocked and report nothing. It signals only
/// the process IDs in the live set, never the ID of a thread that does not lead its
/// process, which that thread releases when it runs `exec`.
fn watch(
    live: &LiveProcesses,
    cancel: &Cancellation,
    done: &mpsc::Receiver<()>,
) {
    loop {
        if cancel.is_cancelled() {
            follow::kill_all(live);
            return;
        }
        match done.recv_timeout(CANCEL_POLL_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {},
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// What the event loop and the helper threads of one run produced.
struct Supervised {
    /// The outcome of the event loop, or `None` when the child exited before it
    /// came under `ptrace`.
    followed: io::Result<Option<follow::Outcome>>,
    /// The captured bytes of each output stream.
    streams: Result<Vec<(OutputStream, Vec<u8>)>, TraceError>,
    /// The result of feeding `stdin`.
    fed: Result<(), TraceError>,
}

/// Follow a started child to the end of its tree while helper threads feed its
/// input, capture its output, and watch for cancellation.
///
/// The event loop runs on the calling thread, the tracer of the tree.
fn supervise(
    request: &RunRequest,
    started: Started,
    tracer_stdio: TracerStdio,
    echo: Option<&dyn EchoSink>,
    cancel: &Cancellation,
) -> Supervised {
    let live = LiveProcesses::new(BTreeSet::new());
    thread::scope(|scope| {
        let feeder = tracer_stdio.stdin.map(|stdin| {
            let content = request.stdin.as_deref().unwrap_or_default();
            scope.spawn(move || capture::feed(stdin, content))
        });
        let readers: Vec<_> = tracer_stdio
            .outputs
            .into_iter()
            .map(|(stream, pipe)| {
                (
                    stream,
                    scope.spawn(move || capture::collect(pipe, stream, echo)),
                )
            })
            .collect();
        let followed = match started {
            Started::Running(root) => {
                let (done_sender, done) = mpsc::channel::<()>();
                let live = &live;
                let watchdog = scope.spawn(move || watch(live, cancel, &done));
                let followed = follow::follow(root, live, cancel);
                if followed.is_err() {
                    follow::abandon(live);
                }
                drop(done_sender);
                // The watchdog only signals processes; it has no result.
                drop(watchdog.join());
                followed.map(Some)
            },
            Started::Exited => Ok(None),
        };
        let streams = readers
            .into_iter()
            .map(|(stream, reader)| {
                join(reader, "that captures output").map(|bytes| (stream, bytes))
            })
            .collect();
        let fed = feeder.map_or(Ok(()), |feeder| join(feeder, "that feeds stdin"));
        Supervised {
            followed,
            streams,
            fed,
        }
    })
}

/// Run `work`, which starts and follows a traced tree, on a new thread that becomes
/// the tracer of the tree, and return its result.
///
/// The event loop's waits report every child of the waiting thread. A new thread has
/// no child until it starts the traced program, so no wait of a run collects,
/// signals or adopts a child that the calling thread, or any other thread, started.
fn on_tracer_thread<T: Send>(
    work: impl FnOnce() -> Result<T, TraceError> + Send
) -> Result<T, TraceError> {
    thread::scope(|scope| {
        let tracer = thread::Builder::new()
            .name("kizz-tracer".to_owned())
            .spawn_scoped(scope, work)
            .map_err(|error| tracer_error("could not start the tracing thread", error))?;
        tracer.join().unwrap_or_else(|_| {
            Err(TraceError::Tracer {
                message: "the tracing thread panicked".to_owned(),
                source: None,
            })
        })
    })
}

impl Tracer for LinuxTracer {
    fn run(
        &self,
        request: &RunRequest,
        echo: &dyn EchoSink,
        cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        let program = Program::prepare(request).map_err(|error| {
            TraceError::StartFailed {
                program: request.program.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, error),
                accesses: Accesses::new(),
            }
        })?;
        let filter = filter::program().map_err(|error| {
            tracer_error(
                "could not build the seccomp-bpf filter",
                io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?;
        let (child_stdio, tracer_stdio) = capture::pipes(request)
            .map_err(|error| tracer_error("could not create the standard streams", error))?;
        let (report_read, report_write) = sys::pipe()
            .map_err(|error| tracer_error("could not create the failure pipe", error))?;
        let supervised = on_tracer_thread(move || {
            let started = spawn::start(request, &program, &filter, &child_stdio, report_write)?;
            drop(child_stdio);
            Ok(supervise(
                request,
                started,
                tracer_stdio,
                request.echo.then_some(echo),
                cancel,
            ))
        })?;
        let outcome = supervised
            .followed
            .map_err(|error| tracer_error("could not follow the traced processes", error))?;
        let streams = supervised.streams?;
        supervised.fed?;
        let (termination, accesses, incomplete) = outcome.map_or_else(
            || (None, Accesses::new(), None),
            |outcome| (outcome.termination, outcome.accesses, outcome.incomplete),
        );
        if let Some(failure) = spawn::read_failure(report_read)
            .map_err(|error| tracer_error("could not read the failure pipe", error))?
        {
            return Err(failure.into_error(request, accesses));
        }
        let Some(termination) = termination else {
            return Err(TraceError::Tracer {
                message: "the traced program ended without an exit status".to_owned(),
                source: None,
            });
        };
        Ok(RunResult {
            termination,
            output: capture::assemble(request.capture, streams),
            accesses,
            incomplete,
        })
    }
}
