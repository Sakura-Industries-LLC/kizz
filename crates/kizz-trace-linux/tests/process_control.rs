// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Linux tracer runs real processes that it starts itself: process control,
//! the seccomp-bpf filter, standard input, output capture, live echo, and
//! cancellation.
//!
//! Programs that the shell cannot express run this test executable again in a
//! helper mode: [`helper_process`] does nothing in a normal test run, and does the
//! work of its mode when the tracer starts the executable with the helper variable
//! set in the request's explicit environment.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: tests and helper processes start plain std threads, and the tests \
              assert, lock and print"
)]

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::{CString, OsString},
    fmt::Write as _,
    fs,
    io::{self, Write},
    mem,
    os::unix::{ffi::OsStringExt as _, process::ExitStatusExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

use kizz_trace::{
    AbsolutePath, AccessKind, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    IncompleteReason, OutputStream, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use kizz_trace_linux::{CANCEL_GRACE_PERIOD, LinuxTracer};

/// The variable that selects the helper mode of this test executable.
const HELPER_VARIABLE: &str = "KIZZ_TRACE_TEST_HELPER";
/// The variable that names the file a helper writes a process ID to.
const PID_FILE_VARIABLE: &str = "KIZZ_TRACE_TEST_PID_FILE";
/// The search path of the shell commands in these tests.
const SEARCH_PATH: &str = "/usr/bin:/bin";
/// How long a test waits for something another process does.
const PATIENCE: Duration = Duration::from_secs(30);

/// The result of a test helper.
type Outcome<T> = Result<T, Box<dyn Error>>;

/// An echo sink that records every chunk, and creates a marker file when a chunk
/// arrives, so a traced program can observe that echo is live.
#[derive(Debug, Default)]
struct Recorder {
    /// Every chunk, in the order it arrived.
    chunks: Mutex<Vec<(OutputStream, Vec<u8>)>>,
    /// The file created when the first chunk arrives.
    marker: Option<PathBuf>,
}

impl Recorder {
    /// The bytes echoed for `stream`, concatenated.
    fn echoed(
        &self,
        stream: OutputStream,
    ) -> Vec<u8> {
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(chunk_stream, _)| *chunk_stream == stream)
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect()
    }

    /// Whether nothing was echoed.
    fn is_empty(&self) -> bool {
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }
}

impl EchoSink for Recorder {
    fn echo(
        &self,
        stream: OutputStream,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((stream, bytes.to_vec()));
        if let Some(marker) = &self.marker {
            fs::write(marker, b"")?;
        }
        Ok(())
    }
}

/// A request to run `program` with `args`, in `/`, with only `PATH` set.
fn request(
    program: &Path,
    args: &[&str],
) -> Outcome<RunRequest> {
    Ok(RunRequest {
        program: AbsolutePath::new(program)?,
        args: args.iter().map(OsString::from).collect(),
        env: BTreeMap::from([(OsString::from("PATH"), OsString::from(SEARCH_PATH))]),
        cwd: AbsolutePath::new("/")?,
        stdin: None,
        capture: CaptureMode::Split,
        echo: false,
        tree: false,
    })
}

/// A request to run a shell script, with `args` as its positional parameters.
fn shell(
    script: &str,
    args: &[&str],
) -> Outcome<RunRequest> {
    let mut all = vec!["sh", "-c", script, "sh"];
    all.extend_from_slice(args);
    request(Path::new("/bin/sh"), &all)
}

/// A request to run this test executable in a helper mode.
fn helper(mode: &str) -> Outcome<RunRequest> {
    let executable = std::env::current_exe()?;
    let mut request = request(&executable, &[
        "process_control",
        "--exact",
        "helper_process",
        "--nocapture",
        "--test-threads=1",
        "--quiet",
    ])?;
    request.env = BTreeMap::from([(OsString::from(HELPER_VARIABLE), OsString::from(mode))]);
    Ok(request)
}

/// Run a request with an echo sink that records nothing in particular.
fn run(request: &RunRequest) -> Result<RunResult, TraceError> {
    LinuxTracer::new().run(request, &Recorder::default(), &Cancellation::new())
}

/// The standard output and standard error of a split capture.
fn split(output: &CapturedOutput) -> Outcome<(String, String)> {
    match output {
        CapturedOutput::Split { stdout, stderr } => {
            Ok((
                String::from_utf8(stdout.clone())?,
                String::from_utf8(stderr.clone())?,
            ))
        },
        CapturedOutput::Merged { .. } => Err("expected split output".into()),
    }
}

/// The output of a merged capture.
fn merged(output: &CapturedOutput) -> Outcome<String> {
    match output {
        CapturedOutput::Merged { output } => Ok(String::from_utf8(output.clone())?),
        CapturedOutput::Split { .. } => Err("expected merged output".into()),
    }
}

/// The moment `PATIENCE` from now.
fn deadline() -> Outcome<Instant> {
    Ok(Instant::now()
        .checked_add(PATIENCE)
        .ok_or("the deadline is out of range")?)
}

/// Wait until `file` holds a complete line, and return the process ID on it.
fn read_pid(file: &Path) -> Outcome<i32> {
    let deadline = deadline()?;
    loop {
        if let Ok(content) = fs::read_to_string(file)
            && content.ends_with('\n')
        {
            return Ok(content.trim().parse()?);
        }
        if Instant::now() >= deadline {
            return Err(format!("{} was not written", file.display()).into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a process has exited: it no longer exists, or it is a zombie that only
/// waits for its parent to collect it.
fn has_exited(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.trim_start().chars().next());
    matches!(state, Some('Z' | 'X'))
}

/// Wait until a process has exited, and report whether it did in time.
fn exits_in_time(pid: i32) -> Outcome<bool> {
    let deadline = deadline()?;
    while !has_exited(pid) {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(true)
}

/// Wait until a process is stopped by a signal, and report whether it was in time.
fn stops_in_time(pid: i32) -> Outcome<bool> {
    let deadline = deadline()?;
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next());
        if state == Some('T') {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Run `test` on a new thread and return its result, failing when it does not
/// finish in time, so a run that never ends fails the test instead of hanging it.
fn finishes_in_time<T: Send + 'static>(test: impl FnOnce() -> T + Send + 'static) -> Outcome<T> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || sender.send(test()).ok());
    Ok(receiver
        .recv_timeout(PATIENCE)
        .map_err(|error| format!("the test did not finish in time, or it panicked: {error}"))?)
}

/// Start a child of the calling thread that the tracer does not own.
fn unrelated_child(args: &[&str]) -> Outcome<std::process::Child> {
    Ok(Command::new("/bin/sh")
        .arg("-c")
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

/// The work of a helper mode, run by this test executable when the tracer starts it
/// with [`HELPER_VARIABLE`] set. In a normal test run it does nothing.
#[test]
fn helper_process() {
    let Some(mode) = std::env::var_os(HELPER_VARIABLE) else {
        return;
    };
    match mode.to_str().unwrap() {
        "threads" => helper_threads().unwrap(),
        "io_uring" => helper_io_uring().unwrap(),
        "x32" => helper_x32().unwrap(),
        "tracer" => helper_tracer().unwrap(),
        "creator" => helper_creator().unwrap(),
        "thread_exec" => helper_thread_exec().unwrap(),
        other => panic!("unknown helper mode {other}"),
    }
}

/// Start threads that each touch the file system and report on standard error.
fn helper_threads() -> Outcome<()> {
    let handles: Vec<_> = (0..8)
        .map(|index| {
            thread::spawn(move || -> io::Result<()> {
                fs::metadata("/")?;
                let entries = fs::read_dir("/")?.count();
                let mut stderr = io::stderr().lock();
                writeln!(
                    stderr,
                    "thread {index} done after listing {entries} entries"
                )
            })
        })
        .collect();
    for handle in handles {
        handle.join().map_err(|_| "a thread panicked")??;
    }
    Ok(())
}

/// Call `io_uring_setup` and report its result and `errno` on standard error.
fn helper_io_uring() -> Outcome<()> {
    let mut params = [0_u8; 120];
    // SAFETY: `params` is large enough for `struct io_uring_params`.
    let ret = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1_u32, params.as_mut_ptr()) };
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "io_uring_setup returned {ret} with errno {errno}")?;
    Ok(())
}

/// Make one system call through the x32 interface.
fn helper_x32() -> Outcome<()> {
    // SAFETY: `getpid` takes no arguments; the kernel either runs it or rejects the
    // x32 number with `ENOSYS`.
    let ret = unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "x32 getpid returned {ret}")?;
    Ok(())
}

/// Trace a shell that writes its process ID to the file named by
/// [`PID_FILE_VARIABLE`] and then sleeps, so the test can kill this tracer.
fn helper_tracer() -> Outcome<()> {
    let pid_file = std::env::var(PID_FILE_VARIABLE)?;
    let request = shell(r#"echo $$ > "$1"; exec sleep 1000"#, &[&pid_file])?;
    let result = run(&request);
    Err(format!("the traced shell was not killed: {result:?}").into())
}

/// Suspend the calling thread for good.
fn pause_forever() -> ! {
    loop {
        // SAFETY: `pause` has no requirements and is async-signal-safe.
        unsafe { libc::pause() };
    }
}

/// Park the calling thread for good.
fn park_forever() {
    loop {
        thread::park();
    }
}

/// Write this process's ID to the file named by [`PID_FILE_VARIABLE`], then keep
/// creating processes and threads that live on: forked processes that wait, vforked
/// processes that run `sleep`, and threads that wait.
fn helper_creator() -> Outcome<()> {
    let pid_file = std::env::var(PID_FILE_VARIABLE)?;
    fs::write(&pid_file, format!("{}\n", std::process::id()))?;
    let sleep = CString::new("/bin/sleep")?;
    let name = CString::new("sleep")?;
    let seconds = CString::new("1000")?;
    let argv = [name.as_ptr(), seconds.as_ptr(), std::ptr::null()];
    let unused: libc::c_long = 0;
    for _ in 0..100 {
        // SAFETY: the child only calls `pause`, which is async-signal-safe.
        if unsafe { libc::fork() } == 0 {
            pause_forever();
        }
        // SAFETY: without `CLONE_VM` the child gets its own copy of memory, as with
        // `fork`, and it only calls `execv` and `_exit` on data prepared before.
        let vforked = unsafe {
            libc::syscall(
                libc::SYS_clone,
                libc::c_long::from(libc::CLONE_VFORK | libc::SIGCHLD),
                unused,
                unused,
                unused,
                unused,
            )
        };
        if vforked == 0 {
            // SAFETY: `sleep` and `argv` are NUL-terminated strings and a
            // null-terminated array, and `_exit` has no requirements.
            unsafe {
                libc::execv(sleep.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        drop(thread::spawn(park_forever));
    }
    pause_forever()
}

/// Write this process's ID to the file named by [`PID_FILE_VARIABLE`], then run
/// this test executable again, with the same arguments and environment, from a
/// thread that does not lead the process. Each new image does the same, so the
/// process goes on running `exec` from such threads until it is killed.
fn helper_thread_exec() -> Outcome<()> {
    let pid_file = std::env::var(PID_FILE_VARIABLE)?;
    fs::write(&pid_file, format!("{}\n", std::process::id()))?;
    let program = CString::new(std::env::current_exe()?.into_os_string().into_vec())?;
    let arguments = std::env::args_os()
        .map(|arg| CString::new(arg.into_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    let runner = thread::spawn(move || {
        let mut pointers: Vec<*const libc::c_char> =
            arguments.iter().map(|arg| arg.as_ptr()).collect();
        pointers.push(std::ptr::null());
        // SAFETY: the program and the arguments are NUL-terminated strings that
        // outlive the call, and `pointers` is a null-terminated array.
        unsafe { libc::execv(program.as_ptr(), pointers.as_ptr()) };
        io::Error::last_os_error()
    });
    let error = runner
        .join()
        .map_err(|_| "the thread that runs exec panicked")?;
    Err(error.into())
}

#[test]
fn merged_capture_keeps_the_exact_order_of_interleaved_writes() {
    let mut request = shell(
        "i=0; while [ $i -lt 200 ]; do echo out$i; echo err$i >&2; i=$((i+1)); done",
        &[],
    )
    .unwrap();
    request.capture = CaptureMode::Merged;
    let result = run(&request).unwrap();
    let mut expected = String::new();
    for i in 0..200 {
        write!(expected, "out{i}\nerr{i}\n").unwrap();
    }
    assert_eq!(merged(&result.output).unwrap(), expected);
    assert_eq!(result.termination, Termination::Exited(0));
}

#[test]
fn split_capture_keeps_standard_output_and_standard_error_apart() {
    let request = shell("echo out1; echo err1 >&2; echo out2; echo err2 >&2", &[]).unwrap();
    let result = run(&request).unwrap();
    assert_eq!(
        split(&result.output).unwrap(),
        ("out1\nout2\n".to_owned(), "err1\nerr2\n".to_owned())
    );
}

#[test]
fn exit_codes_are_reported() {
    for code in [0, 1, 3, 255] {
        let result = run(&shell(&format!("exit {code}"), &[]).unwrap()).unwrap();
        assert_eq!(result.termination, Termination::Exited(code));
    }
}

#[test]
fn terminating_signals_are_reported() {
    for (name, signal) in [
        ("TERM", libc::SIGTERM),
        ("KILL", libc::SIGKILL),
        ("SEGV", libc::SIGSEGV),
    ] {
        let result = run(&shell(&format!("kill -{name} $$"), &[]).unwrap()).unwrap();
        assert_eq!(result.termination, Termination::Signaled(signal), "{name}");
    }
}

#[test]
fn a_handled_signal_is_delivered_to_the_program() {
    let request = shell(
        r"trap 'echo caught; exit 7' USR1; kill -USR1 $$; sleep 1000",
        &[],
    )
    .unwrap();
    let result = run(&request).unwrap();
    assert_eq!(result.termination, Termination::Exited(7));
    assert_eq!(split(&result.output).unwrap().0, "caught\n");
}

/// Cancellation interrupts every process of the tree once and kills, at the end of
/// the grace period, the background processes, which the shell starts with `SIGINT`
/// ignored.
#[test]
fn cancellation_stops_the_whole_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let dir_arg = dir.path().to_str().unwrap();
    let request = shell(
        r#"sleep 1000 & echo $! > "$1/first"; (sleep 1000; true) & echo $! > "$1/second"; echo $$ > "$1/root"; wait"#,
        &[dir_arg],
    ).unwrap();
    let cancel = Cancellation::new();
    let (result, pids) = thread::scope(|scope| {
        let tracer =
            scope.spawn(|| LinuxTracer::new().run(&request, &Recorder::default(), &cancel));
        let pids =
            ["first", "second", "root"].map(|name| read_pid(&dir.path().join(name)).unwrap());
        cancel.cancel();
        (tracer.join().unwrap(), pids)
    });
    let result = result.unwrap();
    // The shell ends at the interrupt; the run ends after the escalation.
    assert_eq!(result.termination, Termination::Signaled(libc::SIGINT));
    for pid in pids {
        assert!(
            exits_in_time(pid).unwrap(),
            "process {pid} survived cancellation"
        );
    }
}

/// Cancel the helper `mode` at ten different moments after it has written its
/// process ID, and fail unless each run ends at the interrupt, which the helper
/// does not handle, and its first process is gone.
///
/// Every process and thread of the tree holds the output pipes, so a run ends only
/// once all of them have exited, the processes created after the interrupt at the
/// escalation.
fn cancel_helper_at_different_moments(mode: &str) -> Outcome<()> {
    for delay in 0..10_u64 {
        let dir = tempfile::tempdir()?;
        let pid_file = dir.path().join("pid");
        let mut request = helper(mode)?;
        request.env.insert(
            OsString::from(PID_FILE_VARIABLE),
            pid_file.clone().into_os_string(),
        );
        let cancel = Cancellation::new();
        let tracer_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = LinuxTracer::new().run(&request, &Recorder::default(), &tracer_cancel);
            drop(sender.send(result));
        });
        let root = read_pid(&pid_file)?;
        thread::sleep(Duration::from_millis(delay));
        cancel.cancel();
        let result = receiver
            .recv_timeout(PATIENCE)
            .map_err(|_| format!("the run cancelled after {delay} ms did not end"))??;
        if result.termination != Termination::Signaled(libc::SIGINT) {
            return Err(format!("the run ended with {:?}", result.termination).into());
        }
        if !exits_in_time(root)? {
            return Err(format!("process {root} survived cancellation").into());
        }
    }
    Ok(())
}

#[test]
fn cancellation_while_processes_are_being_created_stops_the_whole_tree() {
    // Cancellation meets creation events whose new processes are not followed yet.
    cancel_helper_at_different_moments("creator").unwrap();
}

#[test]
fn cancellation_while_threads_run_exec_stops_the_whole_tree() {
    // Cancellation meets threads that do not lead their process while they run
    // `exec` and release their former IDs.
    cancel_helper_at_different_moments("thread_exec").unwrap();
}

/// Run `request`, cancel it once the file `ready` holds a process ID, and return the
/// result and how long the run went on after cancellation.
fn run_cancelled_when_ready(
    request: &RunRequest,
    ready: &Path,
) -> Outcome<(RunResult, Duration)> {
    let cancel = Cancellation::new();
    thread::scope(|scope| {
        let tracer = scope.spawn(|| LinuxTracer::new().run(request, &Recorder::default(), &cancel));
        let waited = read_pid(ready);
        // Cancelled even when `ready` never appeared, so the run ends.
        cancel.cancel();
        let cancelled = Instant::now();
        let result = tracer.join().map_err(|_| "the tracing thread panicked")?;
        let after = cancelled.elapsed();
        waited?;
        Ok((result?, after))
    })
}

/// A command that handles `SIGINT` receives exactly one, and its handler runs to the
/// end during the grace period: a process that it starts runs, the file that process
/// writes is among the run's accesses, and the run reports the exit code the
/// command chose, with its output captured.
#[test]
fn a_command_that_handles_the_interrupt_receives_it_once_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().canonicalize().unwrap();
    let dir_arg = dir_path.to_str().unwrap();
    let request = shell(
        r#"trap 'echo interrupt >> "$1/interrupts"; sleep 0.3; cp "$1/interrupts" "$1/handled"; echo handled; exit 42' INT; echo $$ > "$1/ready"; while :; do sleep 0.05; done"#,
        &[dir_arg],
    )
    .unwrap();
    let (result, after) = run_cancelled_when_ready(&request, &dir_path.join("ready")).unwrap();
    assert_eq!(result.termination, Termination::Exited(42));
    assert_eq!(
        split(&result.output).unwrap(),
        ("handled\n".to_owned(), String::new())
    );
    assert_eq!(
        fs::read_to_string(dir_path.join("interrupts")).unwrap(),
        "interrupt\n"
    );
    let handled = dir_path.join("handled");
    assert_eq!(fs::read_to_string(&handled).unwrap(), "interrupt\n");
    let kinds = result.accesses.get(&AbsolutePath::new(&handled).unwrap());
    assert!(
        kinds.is_some_and(|kinds| kinds.contains(AccessKind::Write)),
        "{kinds:?}"
    );
    assert!(after < CANCEL_GRACE_PERIOD, "the run took {after:?}");
}

/// A command that does not handle `SIGINT` ends at the interrupt, and the run
/// returns as soon as the tree has exited, well before the grace period ends.
#[test]
fn a_command_that_does_not_handle_the_interrupt_ends_at_it() {
    let dir = tempfile::tempdir().unwrap();
    let dir_arg = dir.path().to_str().unwrap();
    let request = shell(r#"echo $$ > "$1/ready"; exec sleep 1000"#, &[dir_arg]).unwrap();
    let (result, after) = run_cancelled_when_ready(&request, &dir.path().join("ready")).unwrap();
    assert_eq!(result.termination, Termination::Signaled(libc::SIGINT));
    assert!(after < CANCEL_GRACE_PERIOD, "the run took {after:?}");
}

/// A command that ignores `SIGINT`, and a process it started, are killed when the
/// grace period ends, and nothing of the tree survives.
#[test]
fn a_command_that_ignores_the_interrupt_is_killed_after_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let dir_arg = dir.path().to_str().unwrap();
    let request = shell(
        r#"trap '' INT; sleep 1000 & echo $! > "$1/child"; echo $$ > "$1/ready"; wait"#,
        &[dir_arg],
    )
    .unwrap();
    let (result, after) = run_cancelled_when_ready(&request, &dir.path().join("ready")).unwrap();
    assert_eq!(result.termination, Termination::Signaled(libc::SIGKILL));
    assert!(after >= CANCEL_GRACE_PERIOD, "the run took {after:?}");
    for name in ["child", "ready"] {
        let pid = read_pid(&dir.path().join(name)).unwrap();
        assert!(
            exits_in_time(pid).unwrap(),
            "process {pid} survived cancellation"
        );
    }
}

/// The program runs in a process group of its own, which its first process leads,
/// so a Ctrl-C that a terminal sends to the tracer's process group never reaches it.
#[test]
fn the_program_runs_in_a_process_group_of_its_own() {
    let result = run(&shell(r"cut -d ' ' -f 5 /proc/$$/stat; echo $$", &[]).unwrap()).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    let (stdout, _) = split(&result.output).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert_eq!(lines.first(), lines.get(1), "{stdout}");
    // SAFETY: `getpgrp` has no requirements.
    let own = unsafe { libc::getpgrp() };
    assert_ne!(lines.first().copied(), Some(own.to_string().as_str()));
}

#[test]
fn an_exited_child_of_the_calling_thread_keeps_its_status_for_its_owner() {
    finishes_in_time(|| {
        let mut unrelated = unrelated_child(&["exit 7"]).unwrap();
        let pid = i32::try_from(unrelated.id()).unwrap();
        assert!(exits_in_time(pid).unwrap());
        let result = run(&shell("sleep 0.1; exit 3", &[]).unwrap()).unwrap();
        assert_eq!(result.termination, Termination::Exited(3));
        assert_eq!(unrelated.wait().unwrap().code(), Some(7));
    })
    .unwrap();
}

#[test]
fn a_stopped_child_of_the_calling_thread_is_left_alone() {
    finishes_in_time(|| {
        let mut unrelated = unrelated_child(&["exec sleep 1000"]).unwrap();
        let pid = i32::try_from(unrelated.id()).unwrap();
        // SAFETY: `kill` has no memory-safety requirements.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        assert!(stops_in_time(pid).unwrap());
        let result = run(&shell("sleep 0.1; exit 3", &[]).unwrap()).unwrap();
        assert_eq!(result.termination, Termination::Exited(3));
        // The tracer neither resumed the child nor collected its stop.
        assert!(stops_in_time(pid).unwrap());
        // SAFETY: `siginfo_t` is plain data, for which all zero bytes are a valid
        // value.
        let mut info: libc::siginfo_t = unsafe { mem::zeroed() };
        // SAFETY: `info` is a valid `siginfo_t` for `waitid` to fill.
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                libc::id_t::try_from(pid).unwrap(),
                &raw mut info,
                libc::WSTOPPED | libc::WNOHANG,
            )
        };
        assert_eq!(ret, 0);
        // SAFETY: a successful `waitid` for a child filled in the child fields.
        assert_eq!(unsafe { info.si_pid() }, pid);
        assert_eq!(info.si_code, libc::CLD_STOPPED);
        unrelated.kill().unwrap();
        assert_eq!(unrelated.wait().unwrap().signal(), Some(libc::SIGKILL));
    })
    .unwrap();
}

#[test]
fn stdin_content_reaches_the_program() {
    let mut request = shell("cat", &[]).unwrap();
    let content: Vec<u8> = (0..=250_u8).cycle().take(1_000_000).collect();
    request.stdin = Some(content.clone());
    let result = run(&request).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    match result.output {
        CapturedOutput::Split { stdout, stderr } => {
            assert_eq!(stdout, content);
            assert_eq!(stderr, b"");
        },
        CapturedOutput::Merged { .. } => panic!("expected split output"),
    }
}

#[test]
fn a_program_that_ignores_its_stdin_content_still_finishes() {
    let mut request = shell("exit 0", &[]).unwrap();
    request.stdin = Some(vec![b'x'; 1_000_000]);
    let result = run(&request).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
}

#[test]
fn a_request_without_stdin_content_reads_end_of_file() {
    let request = shell("if read line; then echo got; else echo eof; fi", &[]).unwrap();
    let result = run(&request).unwrap();
    assert_eq!(split(&result.output).unwrap().0, "eof\n");
}

#[test]
fn live_echo_delivers_output_while_it_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("echoed");
    let mut request = shell(
        r#"echo ready; echo warning >&2; i=0
        while [ ! -e "$1" ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done
        if [ -e "$1" ]; then echo live; else echo late; fi"#,
        &[marker.to_str().unwrap()],
    )
    .unwrap();
    request.echo = true;
    let recorder = Recorder {
        chunks: Mutex::default(),
        marker: Some(marker.clone()),
    };
    let result = LinuxTracer::new()
        .run(&request, &recorder, &Cancellation::new())
        .unwrap();
    let (stdout, stderr) = split(&result.output).unwrap();
    assert_eq!(stdout, "ready\nlive\n");
    assert_eq!(stderr, "warning\n");
    assert_eq!(recorder.echoed(OutputStream::Stdout), stdout.as_bytes());
    assert_eq!(recorder.echoed(OutputStream::Stderr), stderr.as_bytes());
}

#[test]
fn live_echo_of_merged_output_is_the_merged_stream() {
    let mut request = shell("echo one; echo two >&2", &[]).unwrap();
    request.capture = CaptureMode::Merged;
    request.echo = true;
    let recorder = Recorder::default();
    let result = LinuxTracer::new()
        .run(&request, &recorder, &Cancellation::new())
        .unwrap();
    assert_eq!(merged(&result.output).unwrap(), "one\ntwo\n");
    assert_eq!(recorder.echoed(OutputStream::Merged), b"one\ntwo\n");
}

#[test]
fn output_is_not_echoed_unless_requested() {
    let recorder = Recorder::default();
    let result = LinuxTracer::new()
        .run(
            &shell("echo quiet", &[]).unwrap(),
            &recorder,
            &Cancellation::new(),
        )
        .unwrap();
    assert_eq!(split(&result.output).unwrap().0, "quiet\n");
    assert!(recorder.is_empty());
}

#[test]
fn a_shell_pipeline_of_child_processes_runs_to_completion() {
    let request = shell(
        r"printf 'b\na\nc\n' | sort | tr a-z A-Z; (echo nested; sh -c 'echo deeper') | cat; true & wait",
        &[],
    ).unwrap();
    let result = run(&request).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    assert_eq!(
        split(&result.output).unwrap().0,
        "A\nB\nC\nnested\ndeeper\n"
    );
    assert!(result.is_complete());
}

#[test]
fn a_background_process_keeps_the_run_going_until_it_exits() {
    let request = shell("(sleep 0.3; echo late) & echo early", &[]).unwrap();
    let result = run(&request).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    assert_eq!(split(&result.output).unwrap().0, "early\nlate\n");
}

#[test]
fn a_multi_threaded_program_runs_to_completion() {
    let result = run(&helper("threads").unwrap()).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    let (_, stderr) = split(&result.output).unwrap();
    for index in 0..8 {
        assert!(
            stderr.contains(&format!("thread {index} done after listing ")),
            "{stderr}"
        );
    }
    assert!(result.is_complete());
}

#[test]
fn io_uring_setup_returns_enosys() {
    let result = run(&helper("io_uring").unwrap()).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    let (_, stderr) = split(&result.output).unwrap();
    assert!(
        stderr.contains(&format!(
            "io_uring_setup returned -1 with errno {}\n",
            libc::ENOSYS
        )),
        "{stderr}"
    );
    assert!(result.is_complete());
}

#[test]
fn a_non_native_system_call_marks_the_run_incomplete() {
    let result = run(&helper("x32").unwrap()).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    assert!(
        split(&result.output)
            .unwrap()
            .1
            .contains("x32 getpid returned")
    );
    match result.incomplete {
        Some(IncompleteReason::NonNativeSystemCall { detail }) => {
            assert!(detail.contains("x32 interface"), "{detail}");
            assert!(
                detail.contains(&format!("system call {}", libc::SYS_getpid)),
                "{detail}"
            );
        },
        other => panic!("expected a non-native system call, got {other:?}"),
    }
}

#[test]
fn traced_processes_are_killed_when_the_tracer_dies() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let mut tracer = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_process",
            "--nocapture",
            "--test-threads=1",
            "--quiet",
        ])
        .env_clear()
        .env(HELPER_VARIABLE, "tracer")
        .env(PID_FILE_VARIABLE, &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let grandchild = read_pid(&pid_file).unwrap();
    assert!(!has_exited(grandchild));
    tracer.kill().unwrap();
    tracer.wait().unwrap();
    assert!(
        exits_in_time(grandchild).unwrap(),
        "process {grandchild} survived its tracer"
    );
}

#[test]
fn tracers_on_parallel_threads_wait_only_for_their_own_processes() {
    let results: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|code| {
                scope.spawn(move || {
                    let script = format!("sleep 0.{code}; (exit {code}) & wait $!; exit $?");
                    run(&shell(&script, &[]).unwrap()).map(|result| (code, result.termination))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    for result in results {
        let (code, termination) = result.unwrap();
        assert_eq!(termination, Termination::Exited(code));
    }
}

#[test]
fn the_environment_and_working_directory_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let mut request = shell(r#"echo "${HOME-unset}|$KIZZ_VALUE|$(pwd -P)""#, &[]).unwrap();
    request.cwd = AbsolutePath::new(dir.path().canonicalize().unwrap()).unwrap();
    request
        .env
        .insert(OsString::from("KIZZ_VALUE"), OsString::from("set"));
    let result = run(&request).unwrap();
    assert_eq!(
        split(&result.output).unwrap().0,
        format!("unset|set|{}\n", request.cwd)
    );
}

#[test]
fn a_missing_program_is_reported() {
    let request = request(Path::new("/nonexistent/kizz-test-program"), &["program"]).unwrap();
    let error = run(&request).unwrap_err();
    assert!(
        matches!(error, TraceError::ProgramNotFound { .. }),
        "{error:?}"
    );
}

#[test]
fn a_program_that_is_not_executable_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("script");
    fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    let request = request(&program, &["script"]).unwrap();
    let error = run(&request).unwrap_err();
    assert!(
        matches!(error, TraceError::ProgramNotExecutable { .. }),
        "{error:?}"
    );
}

#[test]
fn a_missing_working_directory_fails_to_start() {
    let mut request = shell("exit 0", &[]).unwrap();
    request.cwd = AbsolutePath::new("/nonexistent/kizz-test-directory").unwrap();
    let error = run(&request).unwrap_err();
    assert!(matches!(error, TraceError::StartFailed { .. }), "{error:?}");
}
