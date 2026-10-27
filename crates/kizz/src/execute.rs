// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz execute` command: run one program with the Linux tracer, trace its
//! whole process tree, and write an observation report ("Observing any command
//! with `kizz execute`").
//!
//! Before the program starts, kizz checks that the directory that holds `--output`
//! exists and that `--output` is not a directory; a relative `--output` is relative
//! to the working directory. Either failure is a command-line error, with
//! [`USAGE_ERROR`], and nothing runs.
//!
//! The program gets kizz's whole environment, unchanged, and its working directory,
//! and `kizz_core`'s [`execute`] looks a name without `/` up in that environment's
//! `PATH`. It keeps kizz's standard input, output and error, process group and
//! session. Kizz prints no banner, writes nothing to standard output, and writes to
//! standard error only its own fatal errors: a program that could not start, a
//! tracer that failed, or a report that could not be written.
//!
//! Signals: when the command starts, kizz reads whether `SIGINT`, `SIGQUIT`,
//! `SIGTERM` and `SIGHUP` are ignored. A signal that is ignored stays so, for kizz
//! and for the program, and kizz never forwards it, as a shell keeps a signal that
//! was ignored when it started. Kizz handles each of the others with a handler of
//! `signal_hook`, which `exec` resets, so the program starts with their default
//! action: `SIGINT` and `SIGQUIT` it drops, so kizz ignores them while the program
//! runs, as `time` does, and `SIGTERM` and `SIGHUP` it forwards to the program
//! through the tracer. A `SIGTERM` or `SIGHUP` sent to the whole process group
//! reaches the program directly and once more through kizz, since kizz cannot tell
//! it from one sent to kizz alone. Kizz keeps the default action of `SIGTSTP`,
//! `SIGTTIN` and `SIGTTOU`, so Ctrl-Z stops kizz with the program.
//!
//! The thread that forwards signals starts through `kizz_threads`, which waits and
//! retries while the system cannot start a thread for the moment, and looks for
//! signals with `Signals::pending`, as the one of `kizz build` does. When it still
//! cannot start, or the signals cannot be handled, the program never runs: the
//! report says that the command `failed`, kizz prints the error, and it exits with
//! [`STATUS_FAILED`].
//!
//! Once the whole process tree has ended, kizz writes the report, in the form that
//! `--json` and `--compact` choose, to a new file in the directory of `--output`,
//! and renames it over `--output`, so the program never sees it and `--output` is
//! replaced at once. The report is written whatever happened to the program. Kizz
//! exits with the status of [`kizz_core::exit_status`], or with [`STATUS_FAILED`]
//! when the report could not be written, after it prints the error.
//!
//! Kizz never opens the cache, never takes its lock, and never creates a cache
//! directory.

use std::{
    ffi::{OsStr, OsString, c_int},
    fs::{self, OpenOptions},
    io::{self, Write as _},
    mem,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    process::{self, ExitCode},
    ptr, thread,
};

use kizz_core::{
    CommandOutcome, ExecuteRequest, Executed, ReportFormat, ReportMode, STATUS_FAILED, execute,
    not_executed,
};
use kizz_threads::StartError;
use kizz_trace::{AbsolutePath, Cancellation, Signal};
use kizz_trace_linux::LinuxTracer;
use signal_hook::{
    consts::{FORBIDDEN, SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};

use crate::{
    build::{CloseOnDrop, SIGNAL_LIMIT, SIGNAL_POLL},
    cli::USAGE_ERROR,
    ui,
};

/// The signals that `kizz execute` handles while the program runs, unless they
/// were ignored when it started: `SIGINT` and `SIGQUIT`, which it drops, and
/// `SIGTERM` and `SIGHUP`, which it forwards.
const HANDLED: [c_int; 4] = [SIGINT, SIGQUIT, SIGTERM, SIGHUP];

/// How many names a temporary report file tries before kizz gives up, when each
/// is taken.
const TEMPORARY_ATTEMPTS: u32 = 100;

/// The parsed command line of `kizz execute`.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// The file that the report replaces, `--output`.
    pub output: PathBuf,
    /// Whether the report combines every process's observations, `--compact`.
    pub compact: bool,
    /// Whether the report is JSON, `--json`.
    pub json: bool,
    /// The program, as given, and its arguments.
    pub program: Vec<OsString>,
}

/// Run `kizz execute` with `options` and return its exit status, as the module's
/// documentation says.
pub(crate) fn run(options: Options) -> ExitCode {
    let Options {
        output,
        compact,
        json,
        program,
    } = options;
    let cwd = match working_directory() {
        Ok(cwd) => cwd,
        Err(message) => {
            ui::print_error(&message);
            return ExitCode::from(USAGE_ERROR);
        },
    };
    let output = match checked_output(cwd.as_path(), &output) {
        Ok(output) => output,
        Err(message) => {
            ui::print_error(&message);
            return ExitCode::from(USAGE_ERROR);
        },
    };
    let request = ExecuteRequest {
        args: program,
        env: std::env::vars_os().collect(),
        cwd,
    };
    let executed = traced(request);
    if let Some(message) = failure(&executed.outcome) {
        ui::print_error(&message);
    }
    let format = if json {
        ReportFormat::Json
    } else {
        ReportFormat::Text
    };
    let mode = if compact {
        ReportMode::Compact
    } else {
        ReportMode::Hierarchical
    };
    let report = executed.report.render(format, mode);
    match replace(&output, report.as_bytes()) {
        Ok(()) => ExitCode::from(executed.status),
        Err(error) => {
            ui::print_error(&format!(
                "cannot write the report to {}: {error}",
                output.display()
            ));
            ExitCode::from(STATUS_FAILED)
        },
    }
}

/// The working directory of kizz, which the program gets.
///
/// # Errors
///
/// A message when it cannot be read.
fn working_directory() -> Result<AbsolutePath, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    AbsolutePath::new(cwd).map_err(|error| format!("cannot use the current directory: {error}"))
}

/// The absolute path of `output`, relative to `cwd` when it is relative, once it is
/// checked: it names a file, not a directory, and the directory that holds it
/// exists. `kizz run --output` checks its file with it too.
///
/// # Errors
///
/// A message that says what is wrong with `output`.
pub(crate) fn checked_output(
    cwd: &Path,
    output: &Path,
) -> Result<PathBuf, String> {
    let path = cwd.join(output);
    let shown = output.display();
    if output.as_os_str().as_bytes().ends_with(b"/") {
        return Err(format!("--output names a directory: {shown}"));
    }
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!("--output names a directory: {shown}"));
    }
    let (Some(parent), Some(_)) = (path.parent(), path.file_name()) else {
        return Err(format!("--output names no file: {shown}"));
    };
    if !fs::metadata(parent).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!(
            "the directory of --output does not exist: {}",
            parent.display()
        ));
    }
    Ok(path)
}

/// Whether `signal` is ignored in this process: its action is `SIG_IGN`. A signal
/// whose action cannot be read counts as not ignored.
fn is_ignored(signal: c_int) -> bool {
    // SAFETY: an all-zero `sigaction` is a valid value of the plain C structure.
    let mut action: libc::sigaction = unsafe { mem::zeroed() };
    // SAFETY: with a null new action, `sigaction` only writes the current action of
    // `signal` into `action`, which is valid for writing, and changes nothing.
    let read = unsafe { libc::sigaction(signal, ptr::null(), &raw mut action) };
    read == 0 && action.sa_sigaction == libc::SIG_IGN
}

/// The delivery of `signals`, or the error of a signal that cannot be handled.
///
/// `Signals::new` panics for a signal that `signal_hook` forbids or that is out of
/// its range, so this refuses such a signal first, with an error, as `kizz build`
/// does ("Kizz never panics").
///
/// # Errors
///
/// A message for a signal that `signal_hook` would panic on, or the error of
/// `Signals::new`.
fn delivery(signals: &[c_int]) -> Result<Signals, String> {
    if let Some(signal) = signals
        .iter()
        .find(|signal| **signal <= 0 || **signal >= SIGNAL_LIMIT || FORBIDDEN.contains(*signal))
    {
        return Err(format!("signal {signal} cannot be handled"));
    }
    Signals::new(signals).map_err(|error| error.to_string())
}

/// Run the command of `request` with the Linux tracer while a thread forwards the
/// signals that kizz handles, as the module's documentation says, and return what
/// it gave; when the signals cannot be handled, the command never runs and is
/// `failed`.
fn traced(request: ExecuteRequest) -> Executed {
    let handled: Vec<c_int> = HANDLED
        .into_iter()
        .filter(|signal| !is_ignored(*signal))
        .collect();
    let signals = match delivery(&handled) {
        Ok(signals) => signals,
        Err(error) => return not_executed(request, format!("cannot handle signals: {error}")),
    };
    let cancel = Cancellation::new();
    let tracer = LinuxTracer::new();
    let work = request.clone();
    match with_forwarding(signals, &cancel, || execute(&tracer, work, &cancel)) {
        Ok(executed) => executed,
        Err(error) => not_executed(request, error.to_string()),
    }
}

/// Run `work` while a thread forwards each `SIGTERM` and `SIGHUP` that `signals`
/// delivers through `cancel`, and drops every other signal, and return what `work`
/// returns.
///
/// The thread starts through `kizz_threads`; when it cannot start, `work` never
/// runs, and the error names the thread and the system's error. It looks for
/// signals with `Signals::pending` every [`SIGNAL_POLL`], until a [`CloseOnDrop`]
/// closes the delivery when the scope ends, whichever way `work` ends.
fn with_forwarding<T>(
    signals: Signals,
    cancel: &Cancellation,
    work: impl FnOnce() -> T,
) -> Result<T, StartError> {
    let handle = signals.handle();
    thread::scope(|scope| {
        let _close = CloseOnDrop(handle);
        let forwarder = cancel.clone();
        kizz_threads::spawn_scoped(
            scope,
            "kizz-signals",
            "the thread that forwards signals to the program",
            move || {
                let mut signals = signals;
                while !signals.is_closed() {
                    for signal in signals.pending() {
                        match signal {
                            SIGTERM => forwarder.forward(Signal::Terminate),
                            SIGHUP => forwarder.forward(Signal::Hangup),
                            _ => {},
                        }
                    }
                    thread::sleep(SIGNAL_POLL);
                }
            },
        )?;
        Ok(work())
    })
}

/// The error that kizz prints for `outcome`, when the program could not start or
/// the tracer failed, and `None` when the program ran to its end.
fn failure(outcome: &CommandOutcome) -> Option<String> {
    match outcome {
        CommandOutcome::Exited(_) | CommandOutcome::Signaled(_) => None,
        CommandOutcome::NotFound(message)
        | CommandOutcome::NotExecutable(message)
        | CommandOutcome::StartFailed(message)
        | CommandOutcome::Failed(message) => Some(message.clone()),
    }
}

/// Write `content` to a new file in the directory of `path` and rename it over
/// `path`, so `path` is replaced at once; the new file is removed when a step
/// fails. `kizz run --output` writes its report with it too.
///
/// # Errors
///
/// The error of creating, writing or renaming the new file.
pub(crate) fn replace(
    path: &Path,
    content: &[u8],
) -> io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other("the path names no file"));
    };
    let mut taken = None;
    for attempt in 0..TEMPORARY_ATTEMPTS {
        let temporary = parent.join(temporary_name(name, attempt));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(mut file) => {
                let written = file
                    .write_all(content)
                    .and_then(|()| file.sync_all())
                    .and_then(|()| fs::rename(&temporary, path));
                if written.is_err() {
                    drop(fs::remove_file(&temporary));
                }
                return written;
            },
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => taken = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(taken.unwrap_or_else(|| io::Error::other("no name for a temporary file was free")))
}

/// The name of the temporary file of the report that replaces the file `name`, for
/// the attempt `attempt`: hidden, with kizz's process ID, such as
/// `.report.txt.kizz-4242-0.tmp`.
fn temporary_name(
    name: &OsStr,
    attempt: u32,
) -> OsString {
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".kizz-{}-{attempt}.tmp", process::id()));
    temporary
}

/// Unit tests of the checks of `--output` and the replacement of the report.
#[cfg(test)]
#[allow(clippy::disallowed_macros, reason = "test code: the tests assert")]
mod tests {
    use std::{fs, path::Path};

    use super::{checked_output, replace};

    /// A relative `--output` is relative to the working directory, and an absolute
    /// one stays as it is, once its directory exists.
    #[test]
    fn outputs_resolve_against_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        assert_eq!(
            checked_output(cwd, Path::new("report.txt")).unwrap(),
            cwd.join("report.txt")
        );
        let absolute = cwd.join("other.json");
        assert_eq!(checked_output(Path::new("/"), &absolute).unwrap(), absolute);
    }

    /// A directory, a path that ends with `/`, an empty path, and a file in a
    /// directory that does not exist are refused, each with its message.
    #[test]
    fn outputs_that_name_no_file_in_an_existing_directory_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        fs::create_dir(cwd.join("sub")).unwrap();
        for (output, message) in [
            ("sub", "--output names a directory"),
            ("new/", "--output names a directory"),
            ("", "--output names a directory"),
            (
                "missing/report.txt",
                "the directory of --output does not exist",
            ),
        ] {
            let error = checked_output(cwd, Path::new(output)).unwrap_err();
            assert!(error.starts_with(message), "{output:?}: {error}");
        }
    }

    /// The report replaces an existing file, and no temporary file stays behind; a
    /// directory that is gone is an error.
    #[test]
    fn the_report_replaces_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.txt");
        fs::write(&path, "old").unwrap();
        replace(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("report.txt")]);
        assert!(replace(&dir.path().join("gone/report.txt"), b"new").is_err());
    }
}
