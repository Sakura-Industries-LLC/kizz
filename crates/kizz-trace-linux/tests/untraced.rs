// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Regression tests for explicit untraced execution, stream deadlines and cleanup.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: assertions, helper processes and bounded worker threads"
)]

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsString,
    fs, io,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        unix::{fs::PermissionsExt as _, process::CommandExt as _},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use kizz_trace::{
    AbsolutePath, Cancellation, CaptureMode, EchoSink, OutputStream, RunRequest, Signal,
    Termination, TraceError, Tracer,
};
use kizz_trace_linux::LinuxTracer;

/// Result of a regression test or helper.
type Outcome<T = ()> = Result<T, Box<dyn Error>>;
/// Helper mode in a child invocation of this executable.
const MODE: &str = "KIZZ_UNTRACED_HELPER";
/// File in which a helper reports a process ID.
const READY: &str = "KIZZ_UNTRACED_READY";
/// Maximum time to wait before cancelling a test's worker.
const TIMEOUT: Duration = Duration::from_secs(5);

/// An echo sink that discards bytes.
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

/// An exact request, without inheriting the test runner's environment.
fn request(
    program: &Path,
    args: &[&str],
) -> Outcome<RunRequest> {
    Ok(RunRequest {
        program: AbsolutePath::new(program)?,
        args: args.iter().map(OsString::from).collect(),
        env: BTreeMap::from([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))]),
        cwd: AbsolutePath::new("/")?,
        stdin: None,
        capture: CaptureMode::Split,
        echo: false,
        tree: true,
        terminal: false,
    })
}

/// A shell with explicit positional arguments.
fn shell(
    script: &str,
    args: &[&str],
) -> Outcome<RunRequest> {
    let mut command = vec!["sh", "-c", script, "sh"];
    command.extend_from_slice(args);
    request(Path::new("/bin/sh"), &command)
}

/// Run this executable's helper test in a separate process.
fn helper(
    mode: &str,
    ready: &Path,
) -> Outcome<RunRequest> {
    let mut request = request(&std::env::current_exe()?, &[
        "untraced",
        "--exact",
        "helper_process",
        "--test-threads=1",
    ])?;
    request
        .env
        .insert(OsString::from(MODE), OsString::from(mode));
    request
        .env
        .insert(OsString::from(READY), ready.as_os_str().to_owned());
    Ok(request)
}

/// Read a complete process ID, bounded so a failed start cannot hang a test.
fn read_pid(path: &Path) -> Outcome<i32> {
    let started = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return Ok(text.trim().parse()?);
        }
        if started.elapsed() >= TIMEOUT {
            return Err(format!("{} was not written", path.display()).into());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Whether a process is absent or only a zombie waiting to be collected.
fn exited(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| matches!(rest.trim_start().chars().next(), Some('Z' | 'X')))
}

/// Wait for a process to finish without relying on scheduler timing.
fn wait_exited(pid: i32) -> Outcome {
    let started = Instant::now();
    while !exited(pid) {
        if started.elapsed() >= TIMEOUT {
            return Err(format!("process {pid} is still alive").into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// A stable process descriptor for cleaning up an escaped descendant without ID reuse.
struct Escaped(OwnedFd);
impl Escaped {
    /// Open the descriptor while the helper is known to be alive.
    fn new(pid: i32) -> Outcome<Self> {
        // SAFETY: pidfd_open has no memory-safety requirements.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if descriptor == -1 {
            return Err(io::Error::last_os_error().into());
        }
        let descriptor = libc::c_int::try_from(descriptor)?;
        // SAFETY: pidfd_open returned a new, owned descriptor.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(descriptor) }))
    }
}
impl Drop for Escaped {
    fn drop(&mut self) {
        // SAFETY: the owned pidfd keeps identifying only this helper; no siginfo is
        // supplied.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

/// Only runs helper code when the request explicitly supplied its mode.
#[test]
fn helper_process() -> Outcome {
    let Some(mode) = std::env::var_os(MODE) else {
        return Ok(());
    };
    let ready = PathBuf::from(std::env::var_os(READY).ok_or("no ready path")?);
    match mode.to_str().ok_or("invalid mode")? {
        "escape-streams" => {
            fs::write(
                ready.with_extension("root"),
                format!("{}\n", std::process::id()),
            )?;
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "echo $$ > \"$1\"; exec /bin/sleep 30", "sh"])
                .arg(&ready)
                .env_clear();
            // SAFETY: setsid is async-signal-safe and uses no Rust state after fork.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            let _child = command.spawn()?;
            read_pid(&ready)?;
        },
        "leave-group" => {
            let group: libc::pid_t = std::env::var("KIZZ_UNTRACED_GROUP")?.parse()?;
            // SAFETY: these calls have no memory-safety requirements.
            unsafe {
                if libc::setpgid(0, group) == -1 {
                    return Err(io::Error::last_os_error().into());
                }
                libc::signal(libc::SIGINT, libc::SIG_IGN);
            }
            fs::write(ready, format!("{}\n", std::process::id()))?;
            thread::sleep(Duration::from_secs(30));
        },
        _ => return Err("unknown helper mode".into()),
    }
    Ok(())
}

/// A descendant outside the original group must not hold output or stdin indefinitely.
fn check_escaped_streams(cancel_drain: bool) -> Outcome {
    let directory = tempfile::tempdir()?;
    let ready = directory.path().join("ready");
    let mut request = helper("escape-streams", &ready)?;
    request.stdin = Some(vec![b'x'; 2_000_000]);
    let cancel = Cancellation::new();
    thread::scope(|scope| {
        let (send, receive) = mpsc::channel();
        let request = &request;
        let cancellation = &cancel;
        let worker = scope.spawn(move || {
            let result = LinuxTracer::new().run_untraced(request, &NoEcho, cancellation);
            let _sent = send.send(result);
        });
        let escaped = Escaped::new(read_pid(&ready)?)?;
        let root = read_pid(&ready.with_extension("root"))?;
        wait_exited(root)?;
        if cancel_drain {
            cancel.cancel();
        }
        let received = receive.recv_timeout(TIMEOUT);
        // Close the inherited pipes even on a regression, before joining the worker.
        drop(escaped);
        cancel.cancel();
        worker.join().map_err(|_| "worker panicked")?;
        let result = received??;
        assert_eq!(result.termination, Termination::Exited(0));
        assert!(
            result
                .incomplete
                .as_ref()
                .is_some_and(|why| why.to_string().contains("output possibly cut short"))
        );
        let reason = if cancel_drain {
            "after cancellation"
        } else {
            "after drain deadline"
        };
        assert!(
            result
                .incomplete
                .as_ref()
                .is_some_and(|why| why.to_string().contains(reason))
        );
        Ok(())
    })
}

#[test]
fn escaped_streams_have_a_bounded_drain_deadline() -> Outcome {
    check_escaped_streams(false)
}
#[test]
fn cancellation_closes_escaped_streams_during_drain() -> Outcome {
    check_escaped_streams(true)
}

/// Escalation must kill the first process even when its original group is empty.
#[test]
fn cancellation_kills_a_first_process_that_joined_another_group() -> Outcome {
    let directory = tempfile::tempdir()?;
    let ready = directory.path().join("ready");
    let mut request = helper("leave-group", &ready)?;
    // SAFETY: getpgrp has no requirements.
    request.env.insert(
        OsString::from("KIZZ_UNTRACED_GROUP"),
        OsString::from(unsafe { libc::getpgrp() }.to_string()),
    );
    let cancel = Cancellation::new();
    thread::scope(|scope| {
        let worker = scope.spawn(|| LinuxTracer::new().run_untraced(&request, &NoEcho, &cancel));
        let ready = read_pid(&ready);
        cancel.cancel();
        let result = worker.join().map_err(|_| "worker panicked")??;
        ready?;
        assert_eq!(result.termination, Termination::Signaled(libc::SIGKILL));
        Ok(())
    })
}

/// Only exec errors classify as missing/non-executable programs; cwd errors do not.
#[test]
fn start_failures_keep_exec_and_working_directory_errors_distinct() -> Outcome {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing");
    let mut request = request(&missing, &["missing"])?;
    let run = |request: &RunRequest| {
        LinuxTracer::new().run_untraced(request, &NoEcho, &Cancellation::new())
    };
    assert!(matches!(
        run(&request),
        Err(TraceError::ProgramNotFound { .. })
    ));
    request.program = AbsolutePath::new("/bin/true")?;
    request.cwd = AbsolutePath::new(&missing)?;
    assert!(matches!(run(&request), Err(TraceError::StartFailed { .. })));
    let script = directory.path().join("without-shebang");
    fs::write(&script, "exit 0\n")?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    request.program = AbsolutePath::new(&script)?;
    request.cwd = AbsolutePath::new(directory.path())?;
    assert!(matches!(
        run(&request),
        Err(TraceError::ProgramNotExecutable { .. })
    ));
    Ok(())
}

/// Forwarded termination signals remain observable as the command's exit signal.
#[test]
fn untraced_commands_receive_forwarded_sigterm_and_sighup() -> Outcome {
    for (forwarded, expected) in [
        (Signal::Terminate, libc::SIGTERM),
        (Signal::Hangup, libc::SIGHUP),
    ] {
        let directory = tempfile::tempdir()?;
        let ready = directory.path().join("ready");
        let request = shell("echo $$ > \"$1\"; exec /bin/sleep 30", &[ready
            .to_str()
            .ok_or("invalid ready path")?])?;
        let cancel = Cancellation::new();
        thread::scope(|scope| {
            let worker =
                scope.spawn(|| LinuxTracer::new().run_untraced(&request, &NoEcho, &cancel));
            let ready = read_pid(&ready);
            cancel.forward(forwarded);
            if ready.is_err() {
                cancel.cancel();
            }
            let result = worker.join().map_err(|_| "worker panicked")??;
            ready?;
            assert_eq!(result.termination, Termination::Signaled(expected));
            Ok::<_, Box<dyn Error>>(())
        })?;
    }
    Ok(())
}

/// Normal completion kills any children that remained in the original group.
#[test]
fn a_normal_exit_cleans_up_processes_left_in_the_group() -> Outcome {
    let directory = tempfile::tempdir()?;
    let ready = directory.path().join("ready");
    let request = shell("/bin/sleep 30 & echo $! > \"$1\"; exit 0", &[ready
        .to_str()
        .ok_or("invalid ready path")?])?;
    let result = LinuxTracer::new().run_untraced(&request, &NoEcho, &Cancellation::new())?;
    assert_eq!(result.termination, Termination::Exited(0));
    wait_exited(read_pid(&ready)?)
}
