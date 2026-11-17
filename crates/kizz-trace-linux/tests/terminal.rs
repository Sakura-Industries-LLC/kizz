// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Linux tracer runs a command in one of two ways: a command of a build in a
//! process group of its own with pipes or `/dev/null` as its standard streams, and a
//! command that keeps the terminal in the process group, session and standard
//! streams of the process that runs the tracer. In both ways the command starts with
//! no blocked signal and `SIGPIPE` at its default action, and keeps the signals that
//! the tracer's process ignores ignored. The tracer forwards the signals that its
//! caller forwards, and resumes a command that a stop signal stopped.
//!
//! Tests that need a tracer process whose standard streams and signal dispositions
//! they control run this test executable again in a helper mode, with its standard
//! streams as pipes that the test holds: [`helper_process`] does nothing in a normal
//! test run, and does the work of its mode when [`HELPER_VARIABLE`] is set.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests start plain std threads, and they assert"
)]

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsString,
    fs,
    io::{self, Write as _},
    mem,
    path::Path,
    process::{Command, Output, Stdio},
    ptr,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use kizz_trace::{
    AbsolutePath, Cancellation, CaptureMode, CapturedOutput, RunRequest, RunResult, Signal,
    StdioEcho, Termination, TraceError, Tracer,
};
use kizz_trace_linux::LinuxTracer;
use libc::c_int;

/// The variable that selects the helper mode of this test executable.
const HELPER_VARIABLE: &str = "KIZZ_TRACE_TEST_HELPER";
/// The variable that names the directory where a helper and its command write what
/// they find.
const DIR_VARIABLE: &str = "KIZZ_TRACE_TEST_DIR";
/// The variable that asks a helper to run its command so that it keeps the
/// terminal, when it is `1`.
const TERMINAL_VARIABLE: &str = "KIZZ_TRACE_TEST_TERMINAL";
/// The search path of the shell commands in these tests.
const SEARCH_PATH: &str = "/usr/bin:/bin";
/// How long a test waits for something another process does.
const PATIENCE: Duration = Duration::from_secs(30);
/// How long a test waits for a second delivery of a signal that must arrive once.
const SETTLE: Duration = Duration::from_millis(300);

/// The result of a test helper.
type Outcome<T> = Result<T, Box<dyn Error>>;

/// A request to run `program` with `args`, in `/`, with only `PATH` set, that keeps
/// the terminal when `terminal` is set.
fn request(
    program: &Path,
    args: &[&str],
    terminal: bool,
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
        terminal,
    })
}

/// A request to run a shell script, with `args` as its positional parameters, that
/// keeps the terminal when `terminal` is set.
fn shell(
    script: &str,
    args: &[&str],
    terminal: bool,
) -> Outcome<RunRequest> {
    let mut all = vec!["sh", "-c", script, "sh"];
    all.extend_from_slice(args);
    request(Path::new("/bin/sh"), &all, terminal)
}

/// Run a request with nothing forwarded and no cancellation.
fn run(request: &RunRequest) -> Result<RunResult, TraceError> {
    LinuxTracer::new().run(request, &StdioEcho, &Cancellation::new())
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

/// The moment `PATIENCE` from now.
fn deadline() -> Outcome<Instant> {
    Ok(Instant::now()
        .checked_add(PATIENCE)
        .ok_or("the deadline is out of range")?)
}

/// Wait until `condition` holds, failing with `what` when it does not in time.
fn wait_until(
    what: &str,
    condition: impl Fn() -> bool,
) -> Outcome<()> {
    let deadline = deadline()?;
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("{what} did not happen in time").into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Wait until `file` holds a complete line, and return the process ID on it.
fn read_pid(file: &Path) -> Outcome<i32> {
    wait_until(&format!("{} being written", file.display()), || {
        fs::read_to_string(file).is_ok_and(|content| content.ends_with('\n'))
    })?;
    Ok(fs::read_to_string(file)?.trim().parse()?)
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

/// Whether a process is gone: it has exited and its parent has collected it, so
/// the tracer no longer counts it among the live processes of its tree.
fn is_gone(pid: i32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

/// Run `request` on another thread while `act` acts on its handle, which forwards
/// signals and cancels; when `act` fails, cancel the run, so it ends, and return the
/// failure.
fn while_running(
    request: &RunRequest,
    act: impl FnOnce(&Cancellation) -> Outcome<()>,
) -> Outcome<RunResult> {
    let cancel = Cancellation::new();
    thread::scope(|scope| {
        let tracer = scope.spawn(|| LinuxTracer::new().run(request, &StdioEcho, &cancel));
        let acted = act(&cancel);
        if acted.is_err() {
            cancel.cancel();
        }
        let result = tracer.join().map_err(|_| "the tracing thread panicked")?;
        acted?;
        Ok(result?)
    })
}

/// The process group and the session on a line of `/proc/<pid>/stat`.
fn group_and_session(stat: &str) -> Outcome<(i32, i32)> {
    let (_, rest) = stat.rsplit_once(')').ok_or("the stat line has no name")?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let group = fields.get(2).ok_or("the stat line has no process group")?;
    let session = fields.get(3).ok_or("the stat line has no session")?;
    Ok((group.parse()?, session.parse()?))
}

/// The process ID on a line of `/proc/<pid>/stat`.
fn stat_pid(stat: &str) -> Outcome<i32> {
    Ok(stat
        .split_whitespace()
        .next()
        .ok_or("the stat line is empty")?
        .parse()?)
}

/// The process group and the session of the calling process.
fn own_group_and_session() -> (i32, i32) {
    // SAFETY: `getpgrp` and `getsid` have no requirements.
    unsafe { (libc::getpgrp(), libc::getsid(0)) }
}

/// What the descriptors 0, 1 and 2 of the calling process point to, one per line.
fn own_streams() -> Outcome<String> {
    let mut targets = String::new();
    for descriptor in 0..3 {
        let target = fs::read_link(format!("/proc/self/fd/{descriptor}"))?;
        targets.push_str(&target.to_string_lossy());
        targets.push('\n');
    }
    Ok(targets)
}

/// The script that a command runs to write, into the directory `$1`, its `stat`
/// line from `/proc/self/stat`, read by the shell itself, and what the shell's
/// descriptors 0, 1 and 2 point to, read before the shell redirects any of them.
const REPORT_SCRIPT: &str = r#"read -r stat < /proc/self/stat; streams=$(readlink /proc/$$/fd/0 /proc/$$/fd/1 /proc/$$/fd/2); echo "$stat" > "$1/stat"; echo "$streams" > "$1/streams""#;

/// Run this test executable in the helper `mode`, with `variables` set, `input` on
/// its standard input, and its standard output and error captured through pipes,
/// and return its output; kill it when it does not finish in time.
fn run_helper(
    mode: &str,
    variables: &[(&str, &Path)],
    input: &[u8],
) -> Outcome<Output> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "helper_process",
            "--nocapture",
            "--test-threads=1",
            "--quiet",
        ])
        .env_clear()
        .env(HELPER_VARIABLE, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in variables {
        command.env(name, value);
    }
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || drop(sender.send(child.wait_with_output())));
    if let Ok(output) = receiver.recv_timeout(PATIENCE) {
        Ok(output?)
    } else {
        // SAFETY: `kill` has no memory-safety requirements.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        Err(format!("the helper {mode} did not finish in time").into())
    }
}

/// The work of a helper mode, run by this test executable when a test starts it
/// with [`HELPER_VARIABLE`] set. In a normal test run it does nothing.
#[test]
fn helper_process() {
    let Some(mode) = std::env::var_os(HELPER_VARIABLE) else {
        return;
    };
    match mode.to_str().unwrap() {
        "terminal" => helper_terminal().unwrap(),
        "dispositions" => helper_dispositions().unwrap(),
        other => panic!("unknown helper mode {other}"),
    }
}

/// Write the process group, the session and the standard streams of this process
/// into the directory of [`DIR_VARIABLE`], then run a shell that keeps the terminal,
/// which writes its own there with [`REPORT_SCRIPT`], copies a line of its standard
/// input to `input`, and writes to its standard output and error; then write how the
/// run ended, and whether its output is empty, to `result`.
fn helper_terminal() -> Outcome<()> {
    let dir = std::env::var(DIR_VARIABLE)?;
    let dir = Path::new(&dir);
    let (group, session) = own_group_and_session();
    fs::write(dir.join("tracer_group"), format!("{group} {session}\n"))?;
    fs::write(dir.join("tracer_streams"), own_streams()?)?;
    let script = format!(
        r#"{REPORT_SCRIPT}; IFS= read -r line; echo "$line" > "$1/input"; echo "to standard output"; echo "to standard error" >&2"#
    );
    let dir_arg = dir.to_str().ok_or("the directory is not UTF-8")?;
    let result = run(&shell(&script, &[dir_arg], true)?)?;
    let empty = result.output == CapturedOutput::empty(CaptureMode::Split);
    fs::write(
        dir.join("result"),
        format!("{} {empty}\n", result.termination),
    )?;
    Ok(())
}

/// A signal handler that does nothing.
extern "C" fn do_nothing(_signal: c_int) {}

/// Ignore `SIGUSR1`, handle `SIGUSR2`, and block `SIGUSR2` and `SIGALRM` on this
/// thread, which the tracing thread inherits; then run `cat /proc/self/status`,
/// keeping the terminal when [`TERMINAL_VARIABLE`] is `1`, and write the captured
/// standard output, if any, to this process's standard output, where the status
/// that `cat` writes itself when it keeps the terminal goes too.
fn helper_dispositions() -> Outcome<()> {
    let handler: extern "C" fn(c_int) = do_nothing;
    // SAFETY: `signal` gets valid signal numbers, the ignore action, and a handler
    // that does nothing, which is async-signal-safe.
    let installed = unsafe {
        libc::signal(libc::SIGUSR1, libc::SIG_IGN) != libc::SIG_ERR
            && libc::signal(libc::SIGUSR2, handler as libc::sighandler_t) != libc::SIG_ERR
    };
    if !installed {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `sigset_t` is plain data, for which all zero bytes are a valid value.
    let mut blocked: libc::sigset_t = unsafe { mem::zeroed() };
    // SAFETY: `blocked` is a valid `sigset_t`, and the signal numbers are valid.
    let filled = unsafe {
        libc::sigemptyset(&raw mut blocked) == 0
            && libc::sigaddset(&raw mut blocked, libc::SIGUSR2) == 0
            && libc::sigaddset(&raw mut blocked, libc::SIGALRM) == 0
    };
    if !filled {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `blocked` is a valid `sigset_t`, and the old mask is not wanted.
    let error =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const blocked, ptr::null_mut()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error).into());
    }
    let terminal = std::env::var_os(TERMINAL_VARIABLE).is_some_and(|value| value == "1");
    let result = run(&request(
        Path::new("/bin/cat"),
        &["cat", "/proc/self/status"],
        terminal,
    )?)?;
    if result.termination != Termination::Exited(0) {
        return Err(format!("cat ended with {}", result.termination).into());
    }
    let (stdout, _) = split(&result.output)?;
    let mut out = io::stdout().lock();
    out.write_all(stdout.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// The bit of `signal` in a signal mask of `/proc/<pid>/status`.
fn signal_bit(signal: c_int) -> Outcome<u64> {
    let shift = u32::try_from(signal)?
        .checked_sub(1)
        .ok_or("there is no signal 0")?;
    Ok(1_u64
        .checked_shl(shift)
        .ok_or("the signal is out of range")?)
}

/// The signal mask on the line of `status` that starts with `name` and a colon.
fn signal_mask(
    status: &str,
    name: &str,
) -> Outcome<u64> {
    let prefix = format!("{name}:");
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .ok_or_else(|| format!("no {name} line in {status}"))?;
    Ok(u64::from_str_radix(line.trim(), 16)?)
}

/// Run the helper `dispositions` in the way `terminal` names, and check the masks of
/// the status that its `cat` reports: no blocked signal, `SIGUSR1` ignored as in the
/// helper, `SIGPIPE`, which the helper's Rust runtime ignores, at its default action,
/// and `SIGUSR2`, which the helper handles, neither ignored nor handled.
fn check_dispositions(terminal: bool) -> Outcome<()> {
    let flag = Path::new(if terminal { "1" } else { "0" });
    let output = run_helper("dispositions", &[(TERMINAL_VARIABLE, flag)], b"")?;
    let status = String::from_utf8(output.stdout)?;
    if !output.status.success() {
        return Err(format!(
            "the helper failed with {}: {status}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let blocked = signal_mask(&status, "SigBlk")?;
    let ignored = signal_mask(&status, "SigIgn")?;
    let caught = signal_mask(&status, "SigCgt")?;
    let mode = if terminal { "terminal" } else { "build" };
    if blocked != 0 {
        return Err(format!("{mode}: blocked signals {blocked:x}").into());
    }
    if ignored & signal_bit(libc::SIGUSR1)? == 0 {
        return Err(format!("{mode}: SIGUSR1 is not ignored: {ignored:x}").into());
    }
    if ignored & signal_bit(libc::SIGPIPE)? != 0 {
        return Err(format!("{mode}: SIGPIPE is ignored: {ignored:x}").into());
    }
    let usr2 = signal_bit(libc::SIGUSR2)?;
    if ignored & usr2 != 0 || caught & usr2 != 0 {
        return Err(format!("{mode}: SIGUSR2 is not at its default action").into());
    }
    Ok(())
}

/// A command that keeps the terminal runs in the process group and session of the
/// process that runs the tracer, with that process's standard input, output and
/// error: it reads the line that the test writes to the helper's standard input,
/// and what it writes reaches the helper's standard output and error, while the
/// result's output is empty.
#[test]
fn a_command_that_keeps_the_terminal_shares_the_group_session_and_streams() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().canonicalize().unwrap();
    let output = run_helper(
        "terminal",
        &[(DIR_VARIABLE, &dir_path)],
        b"typed at the terminal\n",
    )
    .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stdout}{stderr}");
    let read = |name: &str| fs::read_to_string(dir_path.join(name)).unwrap();
    assert_eq!(read("result"), "exit code 0 true\n");
    let stat = read("stat");
    let (group, session) = group_and_session(&stat).unwrap();
    assert_eq!(format!("{group} {session}\n"), read("tracer_group"));
    let streams = read("streams");
    assert_eq!(streams, read("tracer_streams"));
    assert_eq!(
        streams
            .lines()
            .filter(|line| line.starts_with("pipe:["))
            .count(),
        3,
        "{streams}"
    );
    assert_eq!(read("input"), "typed at the terminal\n");
    assert!(stdout.contains("to standard output\n"), "{stdout}");
    assert!(stderr.contains("to standard error\n"), "{stderr}");
}

/// A command of a build leads a process group of its own, in the session of the
/// process that runs the tracer, and its standard output and error are pipes, and
/// its standard input `/dev/null` or, with content, a pipe; merged capture gives
/// both output streams one pipe.
#[test]
fn a_command_of_a_build_has_a_process_group_of_its_own_and_pipes() {
    let (own_group, own_session) = own_group_and_session();
    for (content, capture) in [
        (None, CaptureMode::Split),
        (Some(b"input".to_vec()), CaptureMode::Split),
        (None, CaptureMode::Merged),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().canonicalize().unwrap();
        let mut request = shell(REPORT_SCRIPT, &[dir_path.to_str().unwrap()], false).unwrap();
        request.stdin.clone_from(&content);
        request.capture = capture;
        let result = run(&request).unwrap();
        assert_eq!(result.termination, Termination::Exited(0));
        let stat = fs::read_to_string(dir_path.join("stat")).unwrap();
        let (group, session) = group_and_session(&stat).unwrap();
        assert_eq!(group, stat_pid(&stat).unwrap(), "{stat}");
        assert_ne!(group, own_group, "{stat}");
        assert_eq!(session, own_session, "{stat}");
        let streams = fs::read_to_string(dir_path.join("streams")).unwrap();
        let lines: Vec<&str> = streams.lines().collect();
        assert_eq!(lines.len(), 3, "{streams}");
        let (stdin, stdout, stderr) = (lines[0], lines[1], lines[2]);
        if content.is_some() {
            assert!(stdin.starts_with("pipe:["), "{streams}");
        } else {
            assert_eq!(stdin, "/dev/null", "{streams}");
        }
        assert!(stdout.starts_with("pipe:["), "{streams}");
        assert!(stderr.starts_with("pipe:["), "{streams}");
        assert_eq!(
            stdout == stderr,
            capture == CaptureMode::Merged,
            "{streams}"
        );
    }
}

/// A command of a build starts with no blocked signal and `SIGPIPE` at its default
/// action, and a signal that the tracer's process ignores stays ignored in it, while
/// one that the tracer's process handles is at its default action.
#[test]
fn a_command_of_a_build_starts_with_the_signal_dispositions_of_the_tracer() {
    check_dispositions(false).unwrap();
}

/// A command that keeps the terminal starts with the same signal dispositions as a
/// command of a build.
#[test]
fn a_command_that_keeps_the_terminal_starts_with_the_signal_dispositions_of_the_tracer() {
    check_dispositions(true).unwrap();
}

/// A forwarded `SIGTERM` or `SIGHUP` reaches the command's first process once, in
/// both ways of running it, and its trap records it.
#[test]
fn a_forwarded_signal_reaches_the_first_process_once() {
    for terminal in [false, true] {
        for (signal, name) in [(Signal::Terminate, "TERM"), (Signal::Hangup, "HUP")] {
            let dir = tempfile::tempdir().unwrap();
            let dir_path = dir.path().canonicalize().unwrap();
            let script = format!(
                r#"trap 'echo {name} >> "$1/received"' {name}; echo $$ > "$1/ready"; while [ ! -e "$1/stop" ]; do sleep 0.05; done; exit 3"#
            );
            let request = shell(&script, &[dir_path.to_str().unwrap()], terminal).unwrap();
            let result = while_running(&request, |cancel| {
                read_pid(&dir_path.join("ready"))?;
                cancel.forward(signal);
                wait_until("the trap", || dir_path.join("received").exists())?;
                thread::sleep(SETTLE);
                fs::write(dir_path.join("stop"), b"")?;
                Ok(())
            })
            .unwrap();
            assert_eq!(result.termination, Termination::Exited(3), "{name}");
            assert_eq!(
                fs::read_to_string(dir_path.join("received")).unwrap(),
                format!("{name}\n"),
                "terminal {terminal}"
            );
        }
    }
}

/// Once the first process has exited, a forwarded signal reaches the child that it
/// left running, whose trap records it and ends it.
#[test]
fn a_forwarded_signal_reaches_a_child_after_the_first_process_exits() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().canonicalize().unwrap();
    let request = shell(
        r#"sh -c 'trap "echo TERM >> \"\$1/received\"; exit 5" TERM; echo $$ > "$1/child"; while :; do sleep 0.05; done' sh "$1" & echo $$ > "$1/ready""#,
        &[dir_path.to_str().unwrap()],
        false,
    )
    .unwrap();
    let mut child = 0;
    let result = while_running(&request, |cancel| {
        let root = read_pid(&dir_path.join("ready"))?;
        child = read_pid(&dir_path.join("child"))?;
        wait_until("the first process being collected", || is_gone(root))?;
        cancel.forward(Signal::Terminate);
        Ok(())
    })
    .unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    assert_eq!(
        fs::read_to_string(dir_path.join("received")).unwrap(),
        "TERM\n"
    );
    wait_until("the child's exit", || has_exited(child)).unwrap();
}

/// A command that nothing is forwarded to receives no signal from the tracer, in
/// both ways of running it.
#[test]
fn a_command_without_forwarded_signals_receives_none() {
    for terminal in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().canonicalize().unwrap();
        let request = shell(
            r#"for name in TERM HUP INT QUIT USR1 USR2; do trap "echo $name >> \"\$1/received\"" $name; done; sleep 0.3; exit 0"#,
            &[dir_path.to_str().unwrap()],
            terminal,
        )
        .unwrap();
        let result = run(&request).unwrap();
        assert_eq!(result.termination, Termination::Exited(0));
        assert!(
            !dir_path.join("received").exists(),
            "terminal {terminal}: {:?}",
            fs::read_to_string(dir_path.join("received"))
        );
    }
}

/// A command that stops itself with `SIGSTOP` or `SIGTSTP` is resumed by the tracer
/// and runs to its end, the limit of job control that the tracer documents.
#[test]
fn a_command_that_stops_itself_is_resumed() {
    for name in ["STOP", "TSTP"] {
        let request = shell(&format!("kill -{name} $$; echo resumed"), &[], false).unwrap();
        let result = run(&request).unwrap();
        assert_eq!(result.termination, Termination::Exited(0), "{name}");
        assert_eq!(
            split(&result.output).unwrap(),
            ("resumed\n".to_owned(), String::new()),
            "{name}"
        );
    }
}
