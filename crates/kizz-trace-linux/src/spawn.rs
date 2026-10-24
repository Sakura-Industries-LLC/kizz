// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Starting a traced program.
//!
//! The tracer forks, and before the program runs the child connects its standard
//! input, output and error, enters the working directory, sets
//! `PR_SET_NO_NEW_PRIVS`, comes under `ptrace` with `PTRACE_TRACEME`, and stops
//! itself. The tracer then sets the options that follow forks, clones and `exec` and
//! kill the traced processes if the tracer dies, and resumes the child, which
//! installs the seccomp-bpf filter and runs the program with `execve`.
//!
//! Between `fork` and `execve` the child runs only async-signal-safe `libc` calls on
//! data prepared before the fork, because other threads of the tracer's process may
//! hold locks that the child would never see released. When a step fails, the child
//! writes the step and its `errno` to a pipe that `execve` closes, and exits.

use std::{
    ffi::{CString, NulError},
    fs::File,
    io::{self, Read},
    mem,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    ptr,
};

use kizz_trace::{Accesses, RunRequest, TraceError};
use libc::{c_char, c_int, pid_t, sock_filter, sock_fprog};

use crate::sys;

/// The options set on the first traced process, which every process and thread it
/// starts inherits: stop at seccomp `SECCOMP_RET_TRACE` actions, follow forks,
/// vforks, clones and `exec`, and kill every traced process if the tracer exits.
pub(crate) const TRACE_OPTIONS: c_int = libc::PTRACE_O_TRACESECCOMP
    | libc::PTRACE_O_TRACEFORK
    | libc::PTRACE_O_TRACEVFORK
    | libc::PTRACE_O_TRACECLONE
    | libc::PTRACE_O_TRACEEXEC
    | libc::PTRACE_O_EXITKILL;

/// The exit code of a child that could not start the program.
const START_FAILED_EXIT_CODE: c_int = 127;

/// The step at which the child failed to start the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Connecting standard input, output and error.
    Stdio,
    /// Resetting the signal mask and the `SIGPIPE` disposition.
    Signals,
    /// Entering the working directory.
    WorkingDirectory,
    /// Setting `PR_SET_NO_NEW_PRIVS`.
    NoNewPrivileges,
    /// Requesting tracing with `PTRACE_TRACEME`.
    TraceMe,
    /// Stopping for the tracer.
    Stop,
    /// Installing the seccomp-bpf filter.
    Filter,
    /// Running the program with `execve`.
    Exec,
}

impl Step {
    /// Every step, in the order of their codes.
    const ALL: [Self; 8] = [
        Self::Stdio,
        Self::Signals,
        Self::WorkingDirectory,
        Self::NoNewPrivileges,
        Self::TraceMe,
        Self::Stop,
        Self::Filter,
        Self::Exec,
    ];

    /// The code the child writes for this step.
    const fn code(self) -> c_int {
        match self {
            Self::Stdio => 1,
            Self::Signals => 2,
            Self::WorkingDirectory => 3,
            Self::NoNewPrivileges => 4,
            Self::TraceMe => 5,
            Self::Stop => 6,
            Self::Filter => 7,
            Self::Exec => 8,
        }
    }

    /// The step a code the child wrote stands for.
    fn from_code(code: c_int) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.code() == code)
    }

    /// What the child was doing at this step.
    const fn description(self) -> &'static str {
        match self {
            Self::Stdio => "connecting standard input, output and error",
            Self::Signals => "resetting the signal mask",
            Self::WorkingDirectory => "entering the working directory",
            Self::NoNewPrivileges => "setting PR_SET_NO_NEW_PRIVS",
            Self::TraceMe => "requesting tracing with PTRACE_TRACEME",
            Self::Stop => "stopping for the tracer",
            Self::Filter => "installing the seccomp-bpf filter",
            Self::Exec => "running the program",
        }
    }
}

/// A failure the child reported before the program ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChildFailure {
    /// The step that failed.
    pub step: Step,
    /// The `errno` of the failure.
    pub errno: c_int,
}

impl ChildFailure {
    /// The error for a request whose child failed this way.
    pub(crate) fn into_error(
        self,
        request: &RunRequest,
    ) -> TraceError {
        let source = io::Error::from_raw_os_error(self.errno);
        let program = request.program.clone();
        let accesses = Accesses::new();
        match self.step {
            Step::Exec if matches!(self.errno, libc::ENOENT | libc::ENOTDIR) => {
                TraceError::ProgramNotFound { program, accesses }
            },
            Step::Exec if matches!(self.errno, libc::EACCES | libc::ENOEXEC | libc::EISDIR) => {
                TraceError::ProgramNotExecutable {
                    program,
                    source,
                    accesses,
                }
            },
            Step::Stdio | Step::Signals | Step::WorkingDirectory | Step::Exec => {
                TraceError::StartFailed {
                    program,
                    source,
                    accesses,
                }
            },
            Step::NoNewPrivileges | Step::TraceMe | Step::Stop | Step::Filter => {
                TraceError::Tracer {
                    message: format!(
                        "the child of {program} failed while {}",
                        self.step.description()
                    ),
                    source: Some(source),
                }
            },
        }
    }
}

/// The C strings of a request, prepared before the fork.
#[derive(Debug)]
pub(crate) struct Program {
    /// The path of the program.
    path: CString,
    /// The argument vector.
    args: Vec<CString>,
    /// The environment, as `NAME=value` strings.
    env: Vec<CString>,
    /// The working directory.
    cwd: CString,
}

impl Program {
    /// Prepare the C strings of `request`.
    ///
    /// # Errors
    ///
    /// Returns an error when a string contains a NUL byte.
    pub(crate) fn prepare(request: &RunRequest) -> Result<Self, NulError> {
        let args = request
            .args
            .iter()
            .map(|arg| CString::new(arg.as_bytes()))
            .collect::<Result<_, _>>()?;
        let env = request
            .env
            .iter()
            .map(|(name, value)| {
                let mut entry = name.as_bytes().to_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_bytes());
                CString::new(entry)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            path: CString::new(request.program.as_path().as_os_str().as_bytes())?,
            args,
            env,
            cwd: CString::new(request.cwd.as_path().as_os_str().as_bytes())?,
        })
    }
}

/// The descriptors the child moves onto its standard input, output and error.
#[derive(Debug)]
pub(crate) struct ChildStdio {
    /// The descriptor for standard input.
    pub stdin: OwnedFd,
    /// The descriptor for standard output.
    pub stdout: OwnedFd,
    /// The descriptor for standard error, or `None` to share standard output's.
    pub stderr: Option<OwnedFd>,
}

/// Everything the child uses, as raw values that need no allocation.
struct ChildPlan {
    /// The descriptor for standard input.
    stdin: c_int,
    /// The descriptor for standard output.
    stdout: c_int,
    /// The descriptor for standard error.
    stderr: c_int,
    /// The write end of the failure pipe.
    report: c_int,
    /// The working directory.
    cwd: *const c_char,
    /// The program.
    path: *const c_char,
    /// The null-terminated argument vector.
    argv: *const *const c_char,
    /// The null-terminated environment.
    envp: *const *const c_char,
    /// The filter program.
    filter: *const sock_fprog,
    /// An empty signal set.
    empty_mask: *const libc::sigset_t,
}

/// Report the failure of `step` with the current `errno` and exit.
///
/// # Safety
///
/// Must only run in the child between `fork` and `execve`.
unsafe fn fail(
    report: c_int,
    step: Step,
) -> ! {
    // SAFETY: `__errno_location` returns the calling thread's `errno`.
    let errno = unsafe { *libc::__errno_location() };
    let words: [c_int; 2] = [step.code(), errno];
    // SAFETY: `words` is readable for its whole size. A failed write leaves the
    // tracer to report the exit code instead.
    unsafe {
        libc::write(
            report,
            words.as_ptr().cast::<libc::c_void>(),
            mem::size_of_val(&words),
        );
        libc::_exit(START_FAILED_EXIT_CODE)
    }
}

/// The child's side of starting the program.
///
/// # Safety
///
/// Must only run in the child of `fork`, with pointers into data that was prepared
/// before the fork and is still alive. It makes only async-signal-safe calls.
unsafe fn child(plan: &ChildPlan) -> ! {
    // SAFETY: every call below is async-signal-safe and gets valid arguments from
    // `plan`.
    unsafe {
        if libc::dup2(plan.stdin, libc::STDIN_FILENO) == -1
            || libc::dup2(plan.stdout, libc::STDOUT_FILENO) == -1
            || libc::dup2(plan.stderr, libc::STDERR_FILENO) == -1
        {
            fail(plan.report, Step::Stdio);
        }
        if libc::sigprocmask(libc::SIG_SETMASK, plan.empty_mask, ptr::null_mut()) == -1
            || libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR
        {
            fail(plan.report, Step::Signals);
        }
        if libc::chdir(plan.cwd) == -1 {
            fail(plan.report, Step::WorkingDirectory);
        }
        let no_new_privs: libc::c_ulong = 1;
        let unused: libc::c_ulong = 0;
        if libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            no_new_privs,
            unused,
            unused,
            unused,
        ) == -1
        {
            fail(plan.report, Step::NoNewPrivileges);
        }
        if libc::ptrace(
            libc::PTRACE_TRACEME,
            0,
            ptr::null_mut::<libc::c_void>(),
            ptr::null_mut::<libc::c_void>(),
        ) == -1
        {
            fail(plan.report, Step::TraceMe);
        }
        if libc::raise(libc::SIGSTOP) != 0 {
            fail(plan.report, Step::Stop);
        }
        let flags: libc::c_ulong = 0;
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            flags,
            plan.filter,
        ) == -1
        {
            fail(plan.report, Step::Filter);
        }
        libc::execve(plan.path, plan.argv, plan.envp);
        fail(plan.report, Step::Exec)
    }
}

/// The null-terminated array of pointers to `strings`.
fn pointers(strings: &[CString]) -> Vec<*const c_char> {
    strings
        .iter()
        .map(|string| string.as_ptr())
        .chain([ptr::null()])
        .collect()
}

/// The error for a tracer failure while starting the program.
fn tracer_error(
    message: &str,
    source: io::Error,
) -> TraceError {
    TraceError::Tracer {
        message: message.to_owned(),
        source: Some(source),
    }
}

/// Kill a child that is stopped before its program ran, and collect its exit.
fn abandon(pid: pid_t) {
    // The child is stopped and traced, so `SIGKILL` ends it at once, and only this
    // thread can collect it. Failures leave nothing else to clean up.
    drop(sys::kill(pid, libc::SIGKILL));
    drop(sys::wait_for(pid));
}

/// Read the failure the child reported, if it reported one.
///
/// Call it after the child has run its program or exited, when every write end of
/// the pipe is closed.
///
/// # Errors
///
/// Returns the error of reading the pipe.
pub(crate) fn read_failure(report: OwnedFd) -> io::Result<Option<ChildFailure>> {
    let mut bytes = Vec::new();
    File::from(report).read_to_end(&mut bytes)?;
    let (words, _) = bytes.as_chunks::<{ mem::size_of::<c_int>() }>();
    let mut words = words.iter().map(|word| c_int::from_ne_bytes(*word));
    let (Some(code), Some(errno)) = (words.next(), words.next()) else {
        return Ok(None);
    };
    Ok(Step::from_code(code).map(|step| ChildFailure { step, errno }))
}

/// A started child: stopped under `ptrace` with the options set and then resumed,
/// or already exited with a failure to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Started {
    /// The child is traced and running towards its program.
    Running(pid_t),
    /// The child exited before it came under `ptrace`; the failure pipe says why.
    Exited,
}

/// Fork the child for `request`, bring it under `ptrace`, set the options, and
/// resume it.
///
/// Must be called from the thread that will make every later `ptrace` request and
/// wait for the traced processes. `report` is the write end of the failure pipe;
/// it is closed in the tracer's process once the child has it.
///
/// # Errors
///
/// Returns a [`TraceError`] when the fork fails or the tracer cannot set its
/// options.
pub(crate) fn start(
    request: &RunRequest,
    program: &Program,
    filter: &[sock_filter],
    stdio: &ChildStdio,
    report: OwnedFd,
) -> Result<Started, TraceError> {
    let argv = pointers(&program.args);
    let envp = pointers(&program.env);
    let filter_length = u16::try_from(filter.len()).map_err(|error| {
        tracer_error(
            "the seccomp-bpf filter is too long",
            io::Error::new(io::ErrorKind::InvalidInput, error),
        )
    })?;
    let fprog = sock_fprog {
        len: filter_length,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: `sigset_t` is plain data, and `sigemptyset` initializes it.
    let mut empty_mask: libc::sigset_t = unsafe { mem::zeroed() };
    // SAFETY: `empty_mask` is a valid `sigset_t`.
    unsafe { libc::sigemptyset(&raw mut empty_mask) };
    let stdout = stdio.stdout.as_raw_fd();
    let plan = ChildPlan {
        stdin: stdio.stdin.as_raw_fd(),
        stdout,
        stderr: stdio.stderr.as_ref().map_or(stdout, AsRawFd::as_raw_fd),
        report: report.as_raw_fd(),
        cwd: program.cwd.as_ptr(),
        path: program.path.as_ptr(),
        argv: argv.as_ptr(),
        envp: envp.as_ptr(),
        filter: &raw const fprog,
        empty_mask: &raw const empty_mask,
    };
    // SAFETY: the child branch only runs `child`, which makes async-signal-safe
    // calls on `plan`, whose pointers stay valid in the child's copy of memory.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: this is the child of `fork`.
        unsafe { child(&plan) }
    }
    let fork_error = (pid == -1).then(io::Error::last_os_error);
    drop(report);
    if let Some(source) = fork_error {
        return Err(TraceError::StartFailed {
            program: request.program.clone(),
            source,
            accesses: Accesses::new(),
        });
    }
    let status = sys::wait_for(pid)
        .map_err(|error| tracer_error("could not wait for the child to stop", error))?;
    if !libc::WIFSTOPPED(status) {
        return Ok(Started::Exited);
    }
    if libc::WSTOPSIG(status) != libc::SIGSTOP {
        abandon(pid);
        return Err(TraceError::Tracer {
            message: format!(
                "the child stopped with signal {} instead of SIGSTOP",
                libc::WSTOPSIG(status)
            ),
            source: None,
        });
    }
    if let Err(error) = sys::set_options(pid, TRACE_OPTIONS) {
        abandon(pid);
        return Err(tracer_error("could not set the ptrace options", error));
    }
    if let Err(error) = sys::resume(pid, 0) {
        abandon(pid);
        return Err(tracer_error("could not resume the child", error));
    }
    Ok(Started::Running(pid))
}

/// Unit tests for the child's failure reports.
#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString};

    use kizz_trace::{AbsolutePath, CaptureMode};

    use super::*;

    /// A request for `program`.
    fn request(program: &str) -> RunRequest {
        RunRequest {
            program: AbsolutePath::new(program).unwrap(),
            args: vec![OsString::from(program)],
            env: BTreeMap::new(),
            cwd: AbsolutePath::new("/").unwrap(),
            stdin: None,
            capture: CaptureMode::Split,
            echo: false,
        }
    }

    #[test]
    fn step_codes_round_trip() {
        for step in Step::ALL {
            assert_eq!(Step::from_code(step.code()), Some(step));
        }
        assert_eq!(Step::from_code(0), None);
    }

    #[test]
    fn failures_map_to_the_interface_errors() {
        let request = request("/missing");
        let failure = |step, errno| ChildFailure { step, errno }.into_error(&request);
        assert!(matches!(
            failure(Step::Exec, libc::ENOENT),
            TraceError::ProgramNotFound { .. }
        ));
        assert!(matches!(
            failure(Step::Exec, libc::EACCES),
            TraceError::ProgramNotExecutable { .. }
        ));
        assert!(matches!(
            failure(Step::Exec, libc::E2BIG),
            TraceError::StartFailed { .. }
        ));
        assert!(matches!(
            failure(Step::WorkingDirectory, libc::ENOENT),
            TraceError::StartFailed { .. }
        ));
        assert!(failure(Step::TraceMe, libc::EPERM).is_tracer_failure());
        assert!(failure(Step::Filter, libc::EACCES).is_tracer_failure());
    }

    #[test]
    fn programs_with_nul_bytes_are_rejected() {
        let mut request = request("/bin/sh");
        request.args.push(OsString::from("a\0b"));
        assert!(Program::prepare(&request).is_err());
    }
}
