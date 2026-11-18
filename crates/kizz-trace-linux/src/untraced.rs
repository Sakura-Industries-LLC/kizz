// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Explicit untraced execution for commands that need to use ptrace themselves.
//!
//! The direct child determines the exit status. It leads a private process group;
//! cancellation and cleanup signal that group. Descendants must not escape it.
//! An unreaped child reserves the group ID until cleanup has finished, so signals
//! cannot target a recycled ID. Unlike tracing, this mode cannot supervise escaped
//! descendants or kill the whole tree when kizz itself is killed.

use std::{
    collections::BTreeSet,
    io, mem,
    os::unix::process::CommandExt as _,
    process::{Child, Command, Stdio},
    ptr, thread,
    time::Instant,
};

use kizz_trace::{
    Accesses, Cancellation, EchoSink, FirstReads, IncompleteReason, ProcessTree, RunRequest,
    RunResult, Termination, TraceError,
};

use crate::{
    CANCEL_GRACE_PERIOD, CANCEL_POLL_INTERVAL, capture, join, not_started, reader_thread,
    signal_number, sys, tracer_error,
};

/// A child whose ID remains reserved until its process group has been cleaned up.
struct ChildGroup {
    /// The direct child; no other code may reap it before this guard is dropped.
    child: Child,
}

impl ChildGroup {
    /// Signal the private process group while its leader's ID is still reserved.
    fn signal(
        &self,
        signal: libc::c_int,
    ) -> io::Result<()> {
        let pid = libc::pid_t::try_from(self.child.id()).map_err(io::Error::other)?;
        let group = pid
            .checked_neg()
            .ok_or_else(|| io::Error::other("invalid process group ID"))?;
        match sys::kill(group, signal) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            result => result,
        }
    }

    /// Observe the child's exit without reaping it and releasing its ID.
    fn termination(&self) -> io::Result<Option<Termination>> {
        // SAFETY: siginfo_t is plain data and zero initializes its unused fields.
        let mut info: libc::siginfo_t = unsafe { mem::zeroed() };
        // SAFETY: info is writable, and this unreaped child belongs to this process.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
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
        // SAFETY: waitid initialized the SIGCHLD fields of info.
        let status = unsafe { info.si_status() };
        Ok(match info.si_code {
            libc::CLD_EXITED => Some(Termination::Exited(status)),
            libc::CLD_KILLED | libc::CLD_DUMPED => Some(Termination::Signaled(status)),
            _ => None,
        })
    }

    /// Wait for the direct child, forwarding signals and escalating cancellation.
    fn wait(
        &self,
        cancel: &Cancellation,
    ) -> io::Result<Termination> {
        let mut interrupted = None;
        loop {
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
        // Keep the leader unreaped until the group has been signalled. Even when
        // it has exited, its zombie reserves the ID against reuse.
        drop(self.signal(libc::SIGKILL));
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

/// Reset inherited signal state and arrange for the direct child to die with kizz.
///
/// # Safety
///
/// Only call after fork, before exec. Uses only async-signal-safe operations.
unsafe fn prepare_child(parent: libc::pid_t) -> io::Result<()> {
    // SAFETY: plain data, initialized by sigemptyset before use.
    let mut mask: libc::sigset_t = unsafe { mem::zeroed() };
    // SAFETY: every pointer is valid and the calls have no other memory requirements.
    let no_new_privileges: libc::c_ulong = 1;
    let failed = unsafe {
        libc::sigemptyset(&raw mut mask) == -1
            || libc::sigprocmask(libc::SIG_SETMASK, &raw const mask, ptr::null_mut()) == -1
            || libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR
            || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, no_new_privileges, 0, 0, 0) == -1
            || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == -1
    };
    if failed {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getppid has no requirements. Check the race with setting PDEATHSIG.
    if unsafe { libc::getppid() } != parent {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

/// The start error, with no claim that any file access was observed.
fn start_error(
    request: &RunRequest,
    source: io::Error,
) -> TraceError {
    match source.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR) => {
            TraceError::ProgramNotFound {
                program: request.program.clone(),
                accesses: Accesses::new(),
                tree: None,
            }
        },
        Some(libc::EACCES | libc::ENOEXEC | libc::EISDIR) => {
            TraceError::ProgramNotExecutable {
                program: request.program.clone(),
                source,
                accesses: Accesses::new(),
                tree: None,
            }
        },
        _ => {
            TraceError::StartFailed {
                program: request.program.clone(),
                source,
                accesses: Accesses::new(),
                tree: None,
            }
        },
    }
}

/// Run without ptrace or a seccomp filter, preserving streams and exact environment.
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
    let (child_stdio, tracer_stdio) = capture::pipes(request)
        .map_err(|error| tracer_error("could not create the standard streams", error))?;
    let Some(child_stdio) = child_stdio else {
        return Err(TraceError::Tracer {
            message: "untraced execution has no child streams".to_owned(),
            source: None,
        });
    };
    let mut command = Command::new(request.program.as_path());
    if let Some(first) = request.args.first() {
        command.arg0(first);
    }
    command.args(request.args.iter().skip(1));
    command
        .env_clear()
        .envs(&request.env)
        .current_dir(request.cwd.as_path());
    command.process_group(0);
    command.stdin(Stdio::from(child_stdio.stdin));
    let stdout = std::fs::File::from(child_stdio.stdout);
    let stderr =
        match child_stdio.stderr {
            Some(stderr) => Stdio::from(stderr),
            None => {
                Stdio::from(stdout.try_clone().map_err(|error| {
                    tracer_error("could not duplicate the merged stream", error)
                })?)
            },
        };
    command.stdout(Stdio::from(stdout)).stderr(stderr);
    // SAFETY: getpid has no requirements and its result fits pid_t.
    let parent = unsafe { libc::getpid() };
    // SAFETY: the callback runs only async-signal-safe code on data prepared here.
    unsafe { command.pre_exec(move || prepare_child(parent)) };
    let (termination, streams) = thread::scope(move |scope| {
        let feeder = tracer_stdio
            .stdin
            .map(|stdin| {
                kizz_threads::spawn_scoped(
                    scope,
                    "kizz-feeder",
                    "the thread that feeds stdin",
                    move || capture::feed(stdin, request.stdin.as_deref().unwrap_or_default()),
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
                    capture::collect(pipe, stream, request.echo.then_some(echo))
                })
                .map(|reader| (stream, reader))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| not_started(&error))?;
        let spawned = command.spawn();
        // Closing our copies is essential, including when spawn failed.
        drop(command);
        let child = ChildGroup {
            child: spawned.map_err(|error| start_error(request, error))?,
        };
        let termination = child
            .wait(cancel)
            .map_err(|error| tracer_error("could not wait for the untraced command", error))?;
        // Kill any remaining members before waiting for the output pipes to end.
        drop(child);
        let streams = readers
            .into_iter()
            .map(|(stream, reader)| {
                join(reader, "that captures output").map(|bytes| (stream, bytes))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(feeder) = feeder {
            join(feeder, "that feeds stdin")?;
        }
        Ok::<_, TraceError>((termination, streams))
    })?;
    let mut result = RunResult {
        termination,
        output: capture::assemble(request.capture, streams),
        accesses: Accesses::new(),
        incomplete: Some(IncompleteReason::Other {
            detail: "dependency tracing explicitly disabled with untraced=True".to_owned(),
        }),
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
