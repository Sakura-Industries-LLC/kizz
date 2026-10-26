// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz tracer for Linux.
//!
//! [`LinuxTracer`] implements the tracer interface of `kizz_trace` with `ptrace` and
//! a seccomp-bpf filter that stops a traced process only for the system calls that
//! touch the file system, [`TRACKED_SYSTEM_CALLS`], those that make the trace
//! incomplete, [`UNTRACEABLE_SYSTEM_CALLS`], and the native calls outside the table
//! that it knows, [`KNOWN_SYSTEM_CALLS`], and lets every other call run at full
//! speed. Only the `kizz` binary wires it in.
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
//! alike: a failed call, including a lookup that found nothing, is a probe. An
//! `openat2` with `RESOLVE_IN_ROOT` or `RESOLVE_BENEATH` and without `O_NONBLOCK`
//! that fails with `EAGAIN` records nothing, though: Linux gives that error when a
//! rename or mount anywhere on the machine races with a `..` step, and abandons the
//! lookup, so the caller may repeat the call, and each repeated call is recorded
//! like any other.
//!
//! A process that makes one of [`UNTRACEABLE_SYSTEM_CALLS`], which can change what a
//! path means or use a path in a way that the tracer does not record, marks the run
//! as incompletely traced when it makes the call, whether or not the call succeeds,
//! with a reason that names the process and the call; `clone`, `clone3` and
//! `unshare` count only with `CLONE_NEWNS`, and `setns` only when it can enter a
//! mount namespace. A native system call outside [`KNOWN_SYSTEM_CALLS`] leaves the
//! trace complete: the tracer reports its number in the result's unknown system
//! calls. Either call then runs unchanged.
//!
//! The socket calls of [`TRACKED_SYSTEM_CALLS`] name socket addresses: a Unix socket
//! path is recorded like any path, a write for `bind` and a probe for the others,
//! and `connect`, and `sendto`, `sendmsg` and `sendmmsg` with a destination, also
//! report it in the result's connections: each IPv4 or IPv6 address and port
//! whatever the outcome of the call, and each Unix socket, by path or abstract name,
//! unless its path was not found. Other socket families are not recorded.
//!
//! At the exit of a successful call that opens a regular file for reading or runs
//! one, the tracer also notes the file's metadata as its first read, unless the
//! tree wrote that path earlier or the call created or truncated the file: through
//! the descriptor that an opening call returned, and through the path for the
//! program of an `exec` and each interpreter and loader that the kernel loaded.
//!
//! When the request asks for the process tree, the tracer also keeps, apart from
//! the combined accesses, which stay exactly as without it, each process of the
//! tree with the programs it ran and how it ended, and attributes what each call
//! touched to the program that its thread's process ran, each kind with the call's
//! outcome: `ok`, the name of the error, or `unknown` for a call whose result it
//! never saw. A successful `exec` belongs to the program that it starts, whose
//! arguments the tracer reads at the call's entry. Connections, unknown system
//! calls and every reason that the trace is incomplete belong to programs in the
//! same way, while the result keeps the first reason. A command that cannot start
//! gives an error whose tree holds its one process, without a status, with the
//! request's program and what the attempt touched. Without the request, the tracer
//! does none of this extra work.
//!
//! [`LinuxTracer`] also answers the platform queries of the tracer interface. It
//! reads the extended attributes of a path with `listxattr` and `getxattr`, or
//! `llistxattr` and `lgetxattr` without following a final symbolic link, and a file
//! system that keeps none (`EOPNOTSUPP`) has none. It reads the machine state that
//! a command it starts inherits from its own thread: the kernel with `uname`, every
//! resource limit with `getrlimit`, the CPUs with `sched_getaffinity`, the
//! `MemTotal` and `MemAvailable` lines of `/proc/meminfo`, and the file system of
//! the working directory with `statfs`, and its mount flags with `statvfs`. It
//! names an error number with Linux's name for it, such as `ENOENT`, the kernel's
//! names for its internal restart errors, such as `ERESTARTSYS`, and `errno` and the
//! number for any other.
//!
//! When a request is cancelled, the tracer interrupts the tree: it sends `SIGINT`
//! once to every live process. It follows the tree as before for
//! [`CANCEL_GRACE_PERIOD`], and returns as soon as the whole tree has exited; when
//! the grace period ends first, it kills whatever remains with `SIGKILL`. A
//! cancelled run may therefore end with an exit code that the program chose after
//! it handled the interrupt. The program of a build runs in a process group of its
//! own, with pipes or `/dev/null` as its standard streams, so an interrupt that a
//! terminal sends to the tracer's process group never reaches it directly: it
//! receives the interrupt exactly once, from the tracer.
//!
//! A request that keeps the terminal ([`RunRequest::terminal`]), as `kizz execute`
//! makes, connects nothing: the program uses the standard input, output and error
//! of the tracer's process directly, and stays in that process's process group and
//! session, so a terminal's input and signals, Ctrl-C included, reach it as they
//! would without kizz. Nothing is captured or echoed, no thread feeds or reads its
//! streams, and the result's output is empty, in the shape of the request's capture
//! mode. In both ways the program starts with an empty signal mask and `SIGPIPE`
//! at its default action, and inherits every other disposition, so a signal that
//! the tracer's process ignores stays ignored in it.
//!
//! The caller forwards a signal with [`Cancellation::forward`]; until the request
//! is cancelled, the watchdog delivers each one once, to the first process until
//! the tracer has collected its end, and afterwards to each live process of the
//! tree, signaling only the process IDs of the live set. The tracer knows the
//! first process by its identity, not by its process ID, which Linux may give to a
//! later process of the tree once the first is collected. A signal sent to the
//! whole process group of a program that keeps the terminal reaches it directly,
//! and once more when the caller forwards it, since neither can tell it from one
//! sent to the caller alone.
//!
//! The tracer resumes a process that a stop signal stopped, because with
//! `PTRACE_TRACEME` it cannot learn when `SIGCONT` would end the stop. A terminal's
//! Ctrl-Z therefore stops the tracer's process, which keeps the default action of
//! `SIGTSTP`, `SIGTTIN` and `SIGTTOU`, and each traced process waits at its next
//! stop for the tracer, while a stop signal sent to one traced process alone does
//! not keep it stopped.
//!
//! Each run starts its threads through `kizz_threads`, which waits and retries while
//! the system cannot start a thread for the moment, and never panics: the thread
//! that feeds `stdin`, `kizz-feeder`, and a thread that captures each output
//! stream, `kizz-stdout` and `kizz-stderr`, or `kizz-output` for merged output,
//! which start first, before anything of the request runs; then the tracing
//! thread, `kizz-tracer`; and the watchdog of cancellation, `kizz-watchdog`. When
//! one still cannot start, no process of the request is left, and the run returns
//! [`TraceError::Tracer`], whose message names the thread and the system's error.
//!
//! The cleanup of last resort is the end of the tracing thread: every process and
//! thread of the tree is its tracee, with `PTRACE_O_EXITKILL`, so when the thread
//! ends, for an error, a panic or the end of the tree, Linux kills whatever is left
//! of the tree, also processes that left its process group or session and threads
//! in stops that the tracer already collected. The run then collects its first
//! process through a process descriptor, and its output and input threads end with
//! the pipes. No step of that reads `/proc` or signals a process or thread ID.
//! Collecting by descriptor needs Linux 5.4 or later: the descriptor comes from
//! `pidfd_open`, and `waitid` with `P_PIDFD` collects the process.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the kizz tracer supports only Linux on x86_64");

mod capture;
mod decode;
mod errors;
mod filter;
mod follow;
mod processes;
mod spawn;
mod sys;

use std::{
    collections::BTreeSet,
    io,
    os::fd::OwnedFd,
    path::Path,
    sync::{
        OnceLock,
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

pub use filter::{
    KNOWN_SYSTEM_CALLS, TRACKED_SYSTEM_CALLS, TrackedSystemCall, UNTRACEABLE_SYSTEM_CALLS,
};
use kizz_threads::ScopedJoinHandle;
use kizz_trace::{
    Accesses, Cancellation, EchoSink, ExtendedAttributes, FirstReads, MachineState, OutputStream,
    RunRequest, RunResult, Signal, TraceError, Tracer,
};
use libc::{c_int, pid_t, sock_filter};

use crate::{
    capture::TracerStdio,
    follow::{Escalation, LiveProcesses},
    spawn::{ChildStdio, Program, Started},
};

/// How often the cancellation watchdog checks the cancellation flag and, until the
/// request is cancelled, takes and delivers the signals that the caller forwarded.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long a cancelled tree may run after its interrupt, the `SIGINT` that every
/// live process receives once, before the tracer kills whatever remains of it.
pub const CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(2);

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

/// The tracer error for a thread of the run that could not be started, whose
/// message names the thread and the system's error.
fn not_started(error: &kizz_threads::StartError) -> TraceError {
    TraceError::Tracer {
        message: error.to_string(),
        source: None,
    }
}

/// The name and the description of the thread that captures `stream`.
const fn reader_thread(stream: OutputStream) -> (&'static str, &'static str) {
    match stream {
        OutputStream::Stdout => ("kizz-stdout", "the thread that captures standard output"),
        OutputStream::Stderr => ("kizz-stderr", "the thread that captures standard error"),
        OutputStream::Merged => ("kizz-output", "the thread that captures the merged output"),
    }
}

/// The signal number of a forwarded [`Signal`].
const fn signal_number(signal: Signal) -> c_int {
    match signal {
        Signal::Hangup => libc::SIGHUP,
        Signal::Terminate => libc::SIGTERM,
    }
}

/// Deliver every signal forwarded through `cancel` since the last call to the tree,
/// under the rules of [`follow::signal_first`]: to its first process until the loop
/// has collected that process's end, which the live set knows by identity, and
/// afterwards to every live process, waiting at most `patience` for the live set.
///
/// # Errors
///
/// The error of a live set that stays locked, when the signal is not sent.
fn forward(
    live: &LiveProcesses,
    cancel: &Cancellation,
    patience: Duration,
) -> io::Result<()> {
    for signal in cancel.take_forwarded() {
        follow::signal_first(live, signal_number(signal), patience)?;
    }
    Ok(())
}

/// Forward the signals that the caller forwards through `cancel` to the tree whose
/// live processes `live` holds until the request is cancelled; then interrupt the
/// tree, and kill whatever remains of it when the grace period ends, until `done`
/// is dropped.
///
/// Every [`CANCEL_POLL_INTERVAL`] until cancellation, the watchdog takes the
/// forwarded signals and delivers each once, under the rules of
/// [`follow::signal_first`]: to the first process until the loop has collected its
/// end, and afterwards to every live process, also to a later process that Linux
/// gave the first process's ID. Once the request is cancelled, it forwards nothing
/// more.
///
/// On cancellation the watchdog sends `SIGINT` once to every live process. During
/// [`CANCEL_GRACE_PERIOD`] the event loop follows the tree as it does without
/// cancellation, and a tree that exits in that time drops `done`, which ends the
/// watchdog at once. When the grace period ends first, the watchdog sets
/// `escalation`, so the event loop kills every thread it sees from then on, and
/// then sends `SIGKILL` to every live process; it also covers a tree whose threads
/// are all blocked and report nothing. It signals only the process IDs in the live
/// set, never the ID of a thread that does not lead its process, which that thread
/// releases when it runs `exec`.
///
/// When the live set stays locked for `patience`, so the watchdog cannot signal its
/// processes, it asks the tracing thread to end the run, with [`Escalation::end`],
/// which wakes the thread from its wait, and asks again every
/// [`CANCEL_POLL_INTERVAL`] until the loop has ended and dropped `done`. The
/// tracing thread's end then kills the whole tree, escaped and stopped processes
/// included, with no live set, no process group and no ID.
///
/// # Errors
///
/// The error of a live set that stays locked; the run then fails with it.
fn watch(
    live: &LiveProcesses,
    cancel: &Cancellation,
    escalation: &Escalation,
    done: &mpsc::Receiver<()>,
    patience: Duration,
) -> io::Result<()> {
    // End the run without the live set, for a set that stays locked.
    let end = |error: io::Error| -> io::Result<()> {
        loop {
            // A wake that fails leaves the request noted, and the loop reads it
            // before its next wait; the next round wakes it again.
            drop(escalation.end());
            match done.recv_timeout(CANCEL_POLL_INTERVAL) {
                Err(RecvTimeoutError::Timeout) => {},
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return Err(error),
            }
        }
    };
    while !cancel.is_cancelled() {
        if let Err(error) = forward(live, cancel, patience) {
            return end(error);
        }
        match done.recv_timeout(CANCEL_POLL_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {},
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
    if let Err(error) = follow::signal_all(live, libc::SIGINT, patience) {
        return end(error);
    }
    match done.recv_timeout(CANCEL_GRACE_PERIOD) {
        Err(RecvTimeoutError::Timeout) => {},
        Ok(()) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
    }
    escalation.escalate();
    follow::signal_all(live, libc::SIGKILL, patience).or_else(end)
}

/// What the event loop and the helper threads of one run produced.
struct Supervised {
    /// The process ID of the child that the tracer forked.
    pid: pid_t,
    /// The outcome of the event loop, or `None` when the child exited before it
    /// came under `ptrace`.
    followed: io::Result<Option<follow::Outcome>>,
    /// The captured bytes of each output stream.
    streams: Result<Vec<(OutputStream, Vec<u8>)>, TraceError>,
    /// The result of feeding `stdin`.
    fed: Result<(), TraceError>,
}

/// What the tracing thread needs to start the traced program.
struct Launch<'a> {
    /// The request.
    request: &'a RunRequest,
    /// The program, its arguments and its environment, prepared for the child.
    program: &'a Program,
    /// The seccomp-bpf filter that the child installs.
    filter: &'a [sock_filter],
    /// The child's ends of its standard streams, which kizz closes once the child
    /// has them, or when the program never starts; `None` for a command that keeps
    /// the terminal.
    stdio: Option<ChildStdio>,
    /// The write end of the failure pipe.
    report: OwnedFd,
}

/// What the tracing thread produced: the process ID of the child that it forked,
/// and the outcome of the event loop, or `None` when the child exited before it
/// came under `ptrace`.
type Traced = (pid_t, io::Result<Option<follow::Outcome>>);

/// The loop's result `followed` together with the watchdog's `watched`: when the
/// watchdog asked the tracing thread to end the run, the loop failed only for that,
/// and the watchdog's error, the live set that stayed locked, is the run's;
/// otherwise the loop's error comes first, then the watchdog's.
fn settle(
    followed: io::Result<follow::Outcome>,
    watched: io::Result<()>,
    escalation: &Escalation,
) -> io::Result<Option<follow::Outcome>> {
    match (followed, watched) {
        (Ok(outcome), Ok(())) => Ok(Some(outcome)),
        (Err(_), Err(error)) if escalation.is_ended() => Err(error),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Start the program of `launch` on the calling thread, the tracing thread, and
/// follow its tree to the end while the watchdog, whose live-set locks wait at most
/// `patience`, watches `cancel`.
///
/// The thread's only child is the program's first process, whose descriptor goes
/// into `root` at once, before the program runs, so the caller can collect the
/// process once this thread has ended. When anything fails after the start, the
/// error is returned at once and nothing here cleans up: this thread then ends, and
/// its end kills the whole tree ("The cleanup of last resort" of the `follow`
/// module). That holds for a thread that cannot be prepared to be woken, a first
/// process that cannot enter the live set, a watchdog that cannot start, and a
/// loop that fails, the loop that the watchdog ends included.
///
/// # Errors
///
/// The [`TraceError`] of the start, of preparing this thread to be woken, of the
/// live set, or of the watchdog's start; the error of the loop is the second value
/// of the result.
fn trace(
    launch: Launch<'_>,
    cancel: &Cancellation,
    live: &LiveProcesses,
    patience: Duration,
    root: &OnceLock<OwnedFd>,
) -> Result<Traced, TraceError> {
    let request = launch.request;
    let started = spawn::start(
        request,
        launch.program,
        launch.filter,
        launch.stdio.as_ref(),
        launch.report,
    )?;
    drop(launch.stdio);
    let (pid, handle) = match started {
        Started::Running(pid, handle) => (pid, handle),
        Started::Exited(pid) => return Ok((pid, Ok(None))),
    };
    // Only this call sets the cell, once per run, so it cannot be set already.
    drop(root.set(handle));
    let escalation = Escalation::new()
        .map_err(|error| tracer_error("could not prepare the tracing thread to be woken", error))?;
    // The first process is live before the watchdog starts, so an interrupt for a
    // request that is already cancelled reaches it.
    follow::track_root(live, pid)
        .map_err(|error| tracer_error("could not follow the traced program", error))?;
    let (done_sender, done) = mpsc::channel::<()>();
    let escalation = &escalation;
    thread::scope(|scope| {
        let watchdog = kizz_threads::spawn_scoped(
            scope,
            "kizz-watchdog",
            "the thread that watches for cancellation",
            move || watch(live, cancel, escalation, &done, patience),
        )
        .map_err(|error| not_started(&error))?;
        let followed = follow::follow(pid, live, escalation, request.tree.then_some(request));
        drop(done_sender);
        // The watchdog only signals processes and wakes this thread; a live set
        // that it could not lock is the run's error.
        let watched = watchdog.join().unwrap_or(Ok(()));
        Ok((pid, settle(followed, watched, escalation)))
    })
}

/// Run the program of `launch` on a new thread, the tracing thread, while helper
/// threads of the calling thread feed its input and capture its output, and collect
/// its first process once the tracing thread has ended.
///
/// The feeder and the readers start first, before anything of the request runs.
/// When one cannot start, the run returns [`TraceError::Tracer`], whose message
/// names the thread and the system's error, and those that started end at once,
/// because the child's ends of the pipes close with `launch`.
///
/// The tracing thread starts next, through `kizz_threads`; when it cannot start,
/// nothing of the request has run, and the error names it. A new thread has no
/// child until it starts the traced program, so no wait of a run collects, signals
/// or adopts a child that the calling thread, or any other thread, started. It is
/// the tracer of the whole tree, and when it ends, for an error, a panic or the end
/// of the tree, Linux kills whatever is left of the tree (`PTRACE_O_EXITKILL`); the
/// readers then see their pipes end, and the feeder its pipe close. The first
/// process, a child of this process whose thread has ended, is then collected
/// through its descriptor, unless the loop already collected it. No step of that
/// reads `/proc`, needs the live set, or signals a process or thread ID.
///
/// The watchdog waits at most `patience` for `live`, the live set of the tree.
fn supervise(
    launch: Launch<'_>,
    tracer_stdio: TracerStdio,
    echo: Option<&dyn EchoSink>,
    cancel: &Cancellation,
    live: &LiveProcesses,
    patience: Duration,
) -> Result<Supervised, TraceError> {
    let request = launch.request;
    let root = OnceLock::new();
    let root = &root;
    thread::scope(|scope| {
        let feeder = tracer_stdio
            .stdin
            .map(|stdin| {
                let content = request.stdin.as_deref().unwrap_or_default();
                kizz_threads::spawn_scoped(
                    scope,
                    "kizz-feeder",
                    "the thread that feeds stdin",
                    move || capture::feed(stdin, content),
                )
            })
            .transpose()
            .map_err(|error| not_started(&error))?;
        let readers = tracer_stdio
            .outputs
            .into_iter()
            .map(|(stream, pipe)| {
                let (name, what) = reader_thread(stream);
                kizz_threads::spawn_scoped(scope, name, what, move || {
                    capture::collect(pipe, stream, echo)
                })
                .map(|reader| (stream, reader))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| not_started(&error))?;
        let tracing_thread =
            kizz_threads::spawn_scoped(scope, "kizz-tracer", "the tracing thread", move || {
                trace(launch, cancel, live, patience, root)
            })
            .map_err(|error| not_started(&error))?;
        let traced = tracing_thread.join().unwrap_or_else(|_| {
            Err(TraceError::Tracer {
                message: "the tracing thread panicked".to_owned(),
                source: None,
            })
        });
        // The tracing thread has ended, and with it whatever was left of the tree.
        let reaped = root.get().map_or(Ok(()), sys::reap).map_err(|error| {
            tracer_error(
                "could not collect the first process of the traced program",
                error,
            )
        });
        let streams = readers
            .into_iter()
            .map(|(stream, reader)| {
                join(reader, "that captures output").map(|bytes| (stream, bytes))
            })
            .collect();
        let fed = feeder.map_or(Ok(()), |feeder| join(feeder, "that feeds stdin"));
        let (pid, followed) = traced?;
        reaped?;
        Ok(Supervised {
            pid,
            followed,
            streams,
            fed,
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
                tree: request
                    .tree
                    .then(|| processes::failed_start(None, request, None)),
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
        let live = LiveProcesses::default();
        let launch = Launch {
            request,
            program: &program,
            filter: &filter,
            stdio: child_stdio,
            report: report_write,
        };
        let supervised = supervise(
            launch,
            tracer_stdio,
            request.echo.then_some(echo),
            cancel,
            &live,
            kizz_threads::LOCK_PATIENCE,
        )?;
        let pid = supervised.pid;
        let outcome = supervised
            .followed
            .map_err(|error| tracer_error("could not follow the traced processes", error))?
            .unwrap_or_else(|| {
                follow::Outcome {
                    termination: None,
                    accesses: Accesses::new(),
                    incomplete: None,
                    first_reads: FirstReads::new(),
                    unknown_system_calls: BTreeSet::new(),
                    connections: BTreeSet::new(),
                    tree: None,
                }
            });
        let streams = supervised.streams?;
        supervised.fed?;
        if let Some(failure) = spawn::read_failure(report_read)
            .map_err(|error| tracer_error("could not read the failure pipe", error))?
        {
            let tree = request
                .tree
                .then(|| processes::failed_start(outcome.tree, request, Some(pid)));
            return Err(failure.into_error(request, outcome.accesses, tree));
        }
        let Some(termination) = outcome.termination else {
            return Err(TraceError::Tracer {
                message: "the traced program ended without an exit status".to_owned(),
                source: None,
            });
        };
        Ok(RunResult {
            termination,
            output: capture::assemble(request.capture, streams),
            accesses: outcome.accesses,
            incomplete: outcome.incomplete,
            first_reads: outcome.first_reads,
            unknown_system_calls: outcome.unknown_system_calls,
            connections: outcome.connections,
            tree: outcome.tree,
        })
    }

    fn extended_attributes(
        &self,
        path: &Path,
        follow: bool,
    ) -> io::Result<ExtendedAttributes> {
        sys::extended_attributes(path, follow)
    }

    fn machine_state(
        &self,
        cwd: &Path,
    ) -> MachineState {
        sys::machine_state(cwd)
    }

    fn error_name(
        &self,
        errno: i32,
    ) -> String {
        errors::name(i64::from(errno))
    }
}
