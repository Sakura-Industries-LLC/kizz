// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Explicit untraced execution for commands that need to use ptrace themselves.
//!
//! The first process determines the exit status. Its unreaped ID reserves the
//! original process group until cleanup. Pipes use nonblocking I/O, with bounded
//! draining after exit, so escaped descendants cannot keep a run or its cache lock
//! alive. Only the first process is set to die with kizz; other descendants may
//! survive kizz's abrupt death, even when they stayed in the original group.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, Read as _, Write as _},
    mem,
    os::fd::AsRawFd as _,
    thread,
    time::{Duration, Instant},
};

use kizz_trace::{
    Accesses, Cancellation, EchoSink, FirstReads, IncompleteReason, OutputStream, ProcessTree,
    RunRequest, RunResult, Termination, TraceError,
};

use crate::{
    CANCEL_GRACE_PERIOD, CANCEL_POLL_INTERVAL, capture, signal_number, spawn, sys, tracer_error,
};

/// Maximum time to drain output after the first process ends.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum bytes read from each stream in one cancellation polling round.
const CHUNK_SIZE: usize = 64 * 1024;

/// A child whose ID remains reserved until its original group has been cleaned up.
struct ChildGroup {
    /// The unreaped first process; no other code may release this ID.
    pid: libc::pid_t,
}

impl ChildGroup {
    /// Signal the original group and also the first process if it changed groups.
    fn signal(
        &self,
        signal: libc::c_int,
    ) -> io::Result<()> {
        let group = self
            .pid
            .checked_neg()
            .ok_or_else(|| io::Error::other("invalid group ID"))?;
        let sent = ignore_missing(sys::kill(group, signal));
        // SAFETY: getpgid has no memory requirements; the unreaped child reserves pid.
        let current = unsafe { libc::getpgid(self.pid) };
        if current != self.pid || signal == libc::SIGKILL {
            // Always kill the first process on escalation, even if its original group
            // vanished.
            let first = ignore_missing(sys::kill(self.pid, signal));
            sent.and(first)
        } else {
            sent
        }
    }

    /// Observe the first process's exit without reaping it and releasing its ID.
    fn termination(&self) -> io::Result<Option<Termination>> {
        // SAFETY: siginfo_t is plain data, zeroed before waitid fills its fields.
        let mut info: libc::siginfo_t = unsafe { mem::zeroed() };
        let id = libc::id_t::try_from(self.pid).map_err(io::Error::other)?;
        // SAFETY: info is writable and id names this process's unreaped child.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: info's SIGCHLD fields were initialized by waitid or zero
        // initialization.
        let status = unsafe { info.si_status() };
        Ok(match info.si_code {
            libc::CLD_EXITED => Some(Termination::Exited(status)),
            libc::CLD_KILLED | libc::CLD_DUMPED => Some(Termination::Signaled(status)),
            _ => None,
        })
    }

    /// Service streams while waiting, forwarding signals and escalating cancellation.
    fn wait(
        &self,
        streams: &mut Streams<'_>,
        echo: Option<&dyn EchoSink>,
        cancel: &Cancellation,
    ) -> io::Result<Termination> {
        let mut interrupted = None;
        loop {
            streams.pump(echo)?;
            if let Some(termination) = self.termination()? {
                return Ok(termination);
            }
            if cancel.is_cancelled() {
                if let Some(started) = interrupted {
                    if Instant::now().duration_since(started) >= CANCEL_GRACE_PERIOD {
                        self.signal(libc::SIGKILL)?;
                    }
                } else {
                    self.signal(libc::SIGINT)?;
                    interrupted = Some(Instant::now());
                }
            } else {
                for signal in cancel.take_forwarded() {
                    self.signal(signal_number(signal))?;
                }
            }
            thread::sleep(CANCEL_POLL_INTERVAL);
        }
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        // Signal both identities before collecting the child and releasing its ID.
        drop(self.signal(libc::SIGKILL));
        drop(sys::wait_for(self.pid));
    }
}

/// An already absent process or process group needs no further signal.
fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        other => other,
    }
}

/// One nonblocking output reader and the bytes it has captured.
struct Output {
    /// Which output stream this pipe represents.
    stream: OutputStream,
    /// The pipe, until EOF or the drain deadline closes it.
    pipe: Option<File>,
    /// Bytes captured before EOF or forced closure.
    bytes: Vec<u8>,
}

/// Nonblocking standard streams, owned by the supervising thread.
struct Streams<'a> {
    /// The input writer, closed when the input is sent or the first process ends.
    stdin: Option<File>,
    /// The input bytes supplied by the request.
    input: &'a [u8],
    /// How much input has been written.
    written: usize,
    /// Each output reader and its captured bytes.
    outputs: Vec<Output>,
    /// Reused read buffer, kept off the supervising thread's stack.
    buffer: Vec<u8>,
}

impl<'a> Streams<'a> {
    /// Prepare the parent ends without changing the blocking child ends.
    fn new(
        stdio: capture::TracerStdio,
        input: &'a [u8],
    ) -> io::Result<Self> {
        if let Some(pipe) = &stdio.stdin {
            nonblocking(pipe)?;
        }
        let outputs = stdio
            .outputs
            .into_iter()
            .map(|(stream, pipe)| {
                nonblocking(&pipe)?;
                Ok(Output {
                    stream,
                    pipe: Some(pipe),
                    bytes: Vec::new(),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            stdin: stdio.stdin,
            input,
            written: 0,
            outputs,
            buffer: vec![0; CHUNK_SIZE],
        })
    }

    /// Make bounded progress on input and every output, without waiting on a pipe.
    fn pump(
        &mut self,
        echo: Option<&dyn EchoSink>,
    ) -> io::Result<()> {
        if let Some(pipe) = &mut self.stdin {
            let remaining = self.input.get(self.written..).unwrap_or_default();
            if remaining.is_empty() {
                self.stdin = None;
            } else {
                match pipe.write(remaining) {
                    Ok(count) => self.written = self.written.saturating_add(count),
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => self.stdin = None,
                    Err(error) if retry(&error) => {},
                    Err(error) => return Err(error),
                }
            }
        }
        for output in &mut self.outputs {
            let Some(pipe) = &mut output.pipe else {
                continue;
            };
            match pipe.read(&mut self.buffer) {
                Ok(0) => output.pipe = None,
                Ok(count) => {
                    let chunk = self.buffer.get(..count).unwrap_or_default();
                    output.bytes.extend_from_slice(chunk);
                    if let Some(echo) = echo {
                        drop(echo.echo(output.stream, chunk));
                    }
                },
                Err(error) if retry(&error) => {},
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Close stdin and drain output until EOF, cancellation or a bounded deadline.
    /// Returns the reason open output pipes were closed before EOF, if any.
    fn drain(
        &mut self,
        echo: Option<&dyn EchoSink>,
        cancel: &Cancellation,
    ) -> io::Result<Option<&'static str>> {
        self.stdin = None;
        let started = Instant::now();
        loop {
            self.pump(echo)?;
            if self.outputs.iter().all(|output| output.pipe.is_none()) {
                return Ok(None);
            }
            let stop = if cancel.is_cancelled() {
                Some("cancellation")
            } else if started.elapsed() >= DRAIN_TIMEOUT {
                Some("drain deadline")
            } else {
                None
            };
            if let Some(reason) = stop {
                for output in &mut self.outputs {
                    output.pipe = None;
                }
                return Ok(Some(reason));
            }
            thread::sleep(CANCEL_POLL_INTERVAL);
        }
    }
}

/// Transient nonblocking I/O failures are retried in the next cancellation round.
fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// Set nonblocking mode on a parent pipe; the child's opposite end is unaffected.
fn nonblocking(pipe: &File) -> io::Result<()> {
    // SAFETY: fcntl operates on this live descriptor and changes no memory.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor and F_SETFL flag argument are valid.
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Run without tracing, using the traced runner's exact child setup and execve.
pub(crate) fn run(
    request: &RunRequest,
    echo: &dyn EchoSink,
    cancel: &Cancellation,
) -> Result<RunResult, TraceError> {
    if request.terminal {
        return Err(TraceError::Tracer {
            message: "untraced execution requires captured standard streams".to_owned(),
            source: None,
        });
    }
    let program = spawn::Program::prepare(request).map_err(|error| {
        TraceError::StartFailed {
            program: request.program.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, error),
            accesses: Accesses::new(),
            tree: None,
        }
    })?;
    let (child_stdio, parent_stdio) = capture::pipes(request)
        .map_err(|error| tracer_error("could not create the standard streams", error))?;
    let Some(child_stdio) = child_stdio else {
        return Err(TraceError::Tracer {
            message: "untraced execution has no child streams".to_owned(),
            source: None,
        });
    };
    let mut streams = Streams::new(parent_stdio, request.stdin.as_deref().unwrap_or_default())
        .map_err(|error| tracer_error("could not prepare nonblocking streams", error))?;
    let (report_read, report_write) =
        sys::pipe().map_err(|error| tracer_error("could not create the failure pipe", error))?;
    let child = ChildGroup {
        pid: spawn::start_untraced(request, &program, &child_stdio, report_write)?,
    };
    drop(child_stdio);
    let echo = request.echo.then_some(echo);
    let termination = child
        .wait(&mut streams, echo, cancel)
        .map_err(|error| tracer_error("could not wait for the untraced command", error))?;
    // The first process remains unreaped while signalling, so no ID can be reused.
    drop(child);
    let cut_short = streams
        .drain(echo, cancel)
        .map_err(|error| tracer_error("could not drain untraced output", error))?;
    if let Some(failure) = spawn::read_failure(report_read)
        .map_err(|error| tracer_error("could not read the failure pipe", error))?
    {
        return Err(failure.into_error(request, Accesses::new(), None));
    }
    let mut detail = "dependency tracing explicitly disabled with untraced=True".to_owned();
    if let Some(reason) = cut_short {
        detail.push_str("; output possibly cut short after ");
        detail.push_str(reason);
    }
    let output = streams
        .outputs
        .into_iter()
        .map(|output| (output.stream, output.bytes))
        .collect();
    let mut result = RunResult {
        termination,
        output: capture::assemble(request.capture, output),
        accesses: Accesses::new(),
        incomplete: Some(IncompleteReason::Other { detail }),
        first_reads: FirstReads::new(),
        unknown_system_calls: BTreeSet::new(),
        connections: BTreeSet::new(),
        tree: None,
    };
    if request.tree {
        result.tree = Some(ProcessTree::of_run(request, &result));
    }
    Ok(result)
}
