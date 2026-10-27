// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz run` command: run one task by its name, once, with the Linux tracer
//! and the cache, showing each command and its output live ("Running one task
//! with `kizz run`").
//!
//! The command takes the cache lock, saying what it waits for while another
//! process holds it, as `kizz build` does; opens the store; discovers the tasks
//! under the current directory, where a configuration error anywhere fails it with
//! [`USAGE_ERROR`]; selects the task named on the command line, the nearest one
//! winning, with an error for an unknown name or a tie; and runs it once through
//! `kizz_core::run`, with its arguments in `task.args`. No other task runs, and no
//! pass follows. At the end it closes the database, then releases and deletes the
//! lock file.
//!
//! Output: for each command as it starts, a line tagged `run` with
//! `` `COMMAND` (step N): REASON ``, then its output live, as if every `run()` had
//! `stream = True`; for each cache hit, a line tagged `cached` with
//! `` `COMMAND` (step N) ``, then its stored output, standard error to standard
//! error and the rest to standard output. `--explain` prints the full reasons
//! under each of those lines instead of the one-line reason. The task's `print`
//! output, its failure and every warning print as in a build, the warnings about
//! unchecked exits once the run ends, and the summary is one line:
//! `VERDICT: TASK; N commands ran, M cached`. `--no-cache` runs every command, as
//! if each had `always = True`, and still records each result; nothing forces
//! caching.
//!
//! Report: `--output=FILE` writes the observation report of `kizz execute` for the
//! commands of the run once it ends, in the form that `--json` and `--compact`
//! choose, as `kizz execute` writes its own: each command that ran is the root of
//! its own process tree, each cache hit is listed as cached, without
//! observations, and the task program's own accesses are an entry of their own,
//! named as output names the task. Only then does the engine ask the tracer for
//! process trees. `--compact` or `--json` without `--output`, and an `--output`
//! that names a directory or a file in a directory that does not exist, are usage
//! errors, checked before the cache lock is taken, so nothing waits and nothing
//! runs. The report is written to a new file in the directory of `--output` and
//! renamed over it, whether the run succeeded, failed or was interrupted; it is not
//! written when kizz could not run the task at all, such as for an unknown name or
//! a cache that cannot be used. A report that cannot be written is an error, and a
//! run that otherwise succeeded then exits with [`USAGE_ERROR`]; any other status
//! stays what the run gave.
//!
//! Exit status: 0 when the task succeeds, [`TASK_FAILED`] when it fails,
//! [`USAGE_ERROR`] for usage and configuration errors, an unknown name, a tie, and
//! a cache that cannot be used, and [`INTERRUPTED`] when interrupted. Ctrl-C works
//! as in a build: the running command receives the interrupt once, nothing new
//! starts, its result is not cached, and kizz releases the lock and exits with
//! [`INTERRUPTED`]. Every line goes through the `ui` helpers, which drop a write
//! that fails ("Kizz never panics").

use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    process::ExitCode,
};

use kizz_core::{
    Engine, Render, ReportFormat, ReportMode, RunReport, StepDecision, StepError, StepEvents,
    StepOutcome, TaskInfo, select_by_name, task_label,
};
use kizz_store::{CacheLock, LockDetails, Store, cache_dir_from_env};
use kizz_trace::{AbsolutePath, Cancellation, CapturedOutput, StdioEcho};
use kizz_trace_linux::LinuxTracer;
use signal_hook::consts::SIGINT;

use crate::{
    build::{
        self, Output, failure, interrupt_signals, load_project, print_warnings, with_interrupts,
        worker_pool, worse,
    },
    cli::{INTERRUPTED, TASK_FAILED, USAGE_ERROR},
    execute::{checked_output, replace},
    ui,
};

/// The error of `--compact` or `--json` without `--output`.
const REPORT_NEEDS_OUTPUT: &str =
    "--compact and --json shape the report that --output writes, so they need --output";

/// What `kizz run` runs, and how.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// The name of the task.
    pub name: String,
    /// The task's arguments, which its program reads as `task.args`.
    pub args: Vec<String>,
    /// Run every command of the task, as if each had `always = True`.
    pub no_cache: bool,
    /// The file that the observation report replaces, `--output`, if any.
    pub output: Option<PathBuf>,
    /// Whether the report combines every process's observations, `--compact`.
    pub compact: bool,
    /// Whether the report is JSON, `--json`.
    pub json: bool,
    /// The options that `kizz run` shares with `kizz build`: `--explain`,
    /// `--fail-unchecked`, `--cache-unknown-syscalls`, and `--verbose`.
    pub build: build::Options,
}

/// Where and how `kizz run --output` writes its observation report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Destination {
    /// The absolute path of the file that the report replaces.
    path: PathBuf,
    /// Text or JSON, as `--json` chooses.
    format: ReportFormat,
    /// Hierarchical or compact, as `--compact` chooses.
    mode: ReportMode,
}

/// Where the report of `options` goes, checked against the working directory
/// `cwd`: `None` without `--output`.
///
/// # Errors
///
/// A message when `--compact` or `--json` is given without `--output`, or when
/// `--output` names a directory or a file in a directory that does not exist.
fn destination(
    cwd: &Path,
    options: &Options,
) -> Result<Option<Destination>, String> {
    let Some(output) = &options.output else {
        return if options.compact || options.json {
            Err(REPORT_NEEDS_OUTPUT.to_owned())
        } else {
            Ok(None)
        };
    };
    Ok(Some(Destination {
        path: checked_output(cwd, output)?,
        format: if options.json {
            ReportFormat::Json
        } else {
            ReportFormat::Text
        },
        mode: if options.compact {
            ReportMode::Compact
        } else {
            ReportMode::Hierarchical
        },
    }))
}

/// Run the task that `options` names under the current directory once, and return
/// the exit status.
pub(crate) fn run(options: &Options) -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return failure(&format!("cannot read the current directory: {error}"));
        },
    };
    // The report's options are checked before the cache lock is taken, so a usage
    // error never waits for another process.
    let destination = match destination(&cwd, options) {
        Ok(destination) => destination,
        Err(message) => return failure(&message),
    };
    let cache = match cache_dir_from_env() {
        Ok(cache) => cache,
        Err(error) => return failure(&error.to_string()),
    };
    let lock = match CacheLock::acquire(&cache, &LockDetails::current(), |holder| {
        ui::print_waiting(&holder.to_string());
    }) {
        Ok(lock) => lock,
        Err(error) => return failure(&error.to_string()),
    };
    // From here on, Ctrl-C cancels the run instead of killing kizz, so kizz can
    // close the database and delete the lock file, as a build does.
    let signals = match interrupt_signals(SIGINT) {
        Ok(signals) => signals,
        Err(error) => {
            drop(lock);
            return failure(&format!("cannot handle Ctrl-C: {error}"));
        },
    };
    let cancel = Cancellation::new();
    // When the signal thread cannot start, the run never starts: dropping it
    // releases the lock and deletes its file.
    let status = match with_interrupts(signals, &cancel, || {
        let mut status = with_store(&cache, &cwd, options, destination.as_ref(), &cancel);
        if let Err(error) = lock.release() {
            ui::print_error(&error.to_string());
            status = worse(status);
        }
        status
    }) {
        Ok(status) => status,
        Err(error) => {
            ui::print_error(&error.to_string());
            USAGE_ERROR
        },
    };
    ExitCode::from(status)
}

/// Open the store in `cache`, run the task that `options` names under `cwd`,
/// writing its report to `destination`, if any, and close the store; return the
/// exit status.
fn with_store(
    cache: &Path,
    cwd: &Path,
    options: &Options,
    destination: Option<&Destination>,
    cancel: &Cancellation,
) -> u8 {
    let store = match Store::open(cache) {
        Ok(store) => store,
        Err(error) => {
            ui::print_error(&error.to_string());
            return USAGE_ERROR;
        },
    };
    let mut status = run_named(&store, cwd, options, destination, cancel);
    if let Err(error) = store.close() {
        ui::print_error(&error.to_string());
        status = worse(status);
    }
    status
}

/// Discover the tasks under `cwd`, select the one that `options` names, run it
/// once with `store`, and write its report to `destination`, if any; return the
/// exit status.
fn run_named(
    store: &Store,
    cwd: &Path,
    options: &Options,
    destination: Option<&Destination>,
    cancel: &Cancellation,
) -> u8 {
    let Some(pool) = worker_pool() else {
        return USAGE_ERROR;
    };
    let Some(project) = load_project(cwd, &pool) else {
        return USAGE_ERROR;
    };
    let warnings = print_warnings(&project.warnings);
    let task = match select_by_name(&project.tasks, &options.name)
        .map_err(|error| error.to_string())
        .and_then(|task| {
            TaskInfo::for_run(task, options.args.clone()).map_err(|error| error.to_string())
        }) {
        Ok(task) => task,
        Err(message) => {
            ui::print_error(&message);
            ui::print_warning_summary(&warnings);
            return USAGE_ERROR;
        },
    };
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute());
    let caller: BTreeMap<String, String> = env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let tracer = LinuxTracer::new();
    let engine = Engine::new(
        &tracer,
        store,
        &StdioEcho,
        cancel.clone(),
        home.as_deref()
            .and_then(|home| AbsolutePath::new(home).ok()),
        &pool,
    )
    .with_cache_unknown_system_calls(options.build.engine.cache_unknown_syscalls)
    .with_fail_unchecked(options.build.engine.fail_unchecked)
    .with_no_cache(options.no_cache)
    .with_echo_every_step(true)
    .with_command_name("kizz run")
    .with_report(destination.is_some());
    let render = Render {
        base: cwd.to_path_buf(),
        home,
        verbose: options.build.verbose,
    };
    let live = Live {
        render: render.clone(),
        explain: options.build.explain,
    };
    let output = Output::new(
        render.clone(),
        options.build,
        &engine,
        store,
        warnings,
        "kizz run",
    );
    let status = match kizz_core::run(&engine, &task, &caller, &output, &live) {
        Ok(report) => {
            let status = finish(&output, &render, &report);
            match destination {
                Some(destination) => write_report(destination, &render, &report, status),
                None => status,
            }
        },
        Err(error) => {
            ui::print_error(&format!("cannot use the cache: {error}"));
            USAGE_ERROR
        },
    };
    ui::print_warning_summary(&output.take_warnings());
    status
}

/// Write the observation report of the run `report` to `destination`, with its task
/// program named as `render` names the task, and return the exit status: `status`,
/// the run's own, or, when the report cannot be written, after kizz prints the
/// error, [`USAGE_ERROR`] for a run that succeeded, whose status is 0, and
/// `status` otherwise.
fn write_report(
    destination: &Destination,
    render: &Render,
    report: &RunReport,
    status: u8,
) -> u8 {
    let task = render.task(&task_label(&report.result.task));
    let Some(observed) = report.observation_report(&task) else {
        return status;
    };
    let text = observed.render(destination.format, destination.mode);
    match replace(&destination.path, text.as_bytes()) {
        Ok(()) => status,
        Err(error) => {
            ui::print_error(&format!(
                "cannot write the report to {}: {error}",
                destination.path.display()
            ));
            if status == 0 { USAGE_ERROR } else { status }
        },
    }
}

/// Print the warnings of the run `report`, its failure, the unchecked exits, how it
/// ended and its summary, rendered with `render`; return the exit status.
fn finish(
    output: &Output<'_>,
    render: &Render,
    report: &RunReport,
) -> u8 {
    let task = render.task(&task_label(&report.result.task));
    if let Err(error) = output.check_warnings() {
        output.unreadable(&error);
    }
    if let Err(failed) = &report.result.result {
        ui::print_task_failure(&task, &failed.to_string());
    }
    for step in report.unchecked() {
        output.warn(output.unchecked(&step));
    }
    let (verdict, status) = if report.interrupted {
        ui::print_error(
            "the run was interrupted: it started nothing new, the running command was \
             interrupted, and its result was not cached",
        );
        ("run interrupted", INTERRUPTED)
    } else if report.succeeded() {
        ("run succeeded", 0)
    } else {
        let missing: Vec<String> = report
            .result
            .missing
            .iter()
            .map(|path| format!("`{}`", render.path(path)))
            .collect();
        ui::print_failed_task(&task, &missing);
        ("run failed", TASK_FAILED)
    };
    let (ran, cached) = report.command_counts();
    ui::print_summary(&summary_line(verdict, &task, ran, cached));
    if output.had_unreadable() {
        worse(status)
    } else {
        status
    }
}

/// The summary line of a run: `VERDICT: TASK; N commands ran, M cached`.
fn summary_line(
    verdict: &str,
    task: &str,
    ran: usize,
    cached: usize,
) -> String {
    format!(
        "{verdict}: {task}; {} ran, {cached} cached",
        ui::count(ran, "command")
    )
}

/// Prints each step of the run as it starts or comes from the cache.
struct Live {
    /// How commands and reasons read.
    render: Render,
    /// Print the full reasons under each step, `--explain`.
    explain: bool,
}

impl Live {
    /// The step `decision` as its line names it: `` `COMMAND` (step N) ``.
    fn named(decision: &StepDecision) -> String {
        format!(
            "`{}` (step {})",
            decision.command,
            decision.position.saturating_add(1)
        )
    }

    /// Print the full reasons of `decision` under its line, with `--explain`.
    fn details(
        &self,
        decision: &StepDecision,
    ) {
        if self.explain {
            for line in self.render.full(&decision.verdict) {
                ui::print_detail(&line);
            }
        }
    }
}

impl StepEvents for Live {
    fn step_started(
        &self,
        decision: &StepDecision,
    ) {
        let named = Self::named(decision);
        if self.explain {
            ui::print_run(&named);
        } else {
            ui::print_run(&format!(
                "{named}: {}",
                self.render.one_line(&decision.verdict)
            ));
        }
        self.details(decision);
    }

    fn step_cached(
        &self,
        decision: &StepDecision,
        result: &Result<StepOutcome, StepError>,
    ) {
        ui::print_cached(&Self::named(decision));
        self.details(decision);
        if let Ok(outcome) = result {
            replay(&outcome.output);
        }
    }
}

/// Print the stored output `output` of a cache hit: standard error to standard
/// error, and standard output, or the merged output, to standard output.
fn replay(output: &CapturedOutput) {
    match output {
        CapturedOutput::Split { stdout, stderr } => {
            ui::print_output(stdout);
            ui::print_error_output(stderr);
        },
        CapturedOutput::Merged { output } => ui::print_output(output),
    }
}

/// Unit tests for the lines of `kizz run`.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert"
)]
mod tests {
    use std::{fs, path::PathBuf};

    use kizz_core::{ReportFormat, ReportMode, StepDecision};
    use kizz_store::Verdict;

    use super::{Destination, Live, Options, REPORT_NEEDS_OUTPUT, destination, summary_line};
    use crate::build;

    /// The options of `kizz run lint` with `output`, `--compact` when `compact` is
    /// set and `--json` when `json` is set.
    fn options(
        output: Option<&str>,
        compact: bool,
        json: bool,
    ) -> Options {
        Options {
            name: "lint".to_owned(),
            args: Vec::new(),
            no_cache: false,
            output: output.map(PathBuf::from),
            compact,
            json,
            build: build::Options {
                explain: false,
                verbose: false,
                engine: build::EngineOptions {
                    cache_unknown_syscalls: false,
                    fail_unchecked: false,
                },
            },
        }
    }

    /// Without `--output` there is no report, and `--compact` or `--json` alone is
    /// an error; `--output` resolves against the working directory, with text and
    /// hierarchical by default and each option choosing its form; a file in a
    /// directory that does not exist and a directory are errors.
    #[test]
    fn the_report_options_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        fs::create_dir(cwd.join("sub")).unwrap();
        assert_eq!(destination(cwd, &options(None, false, false)), Ok(None));
        for (compact, json) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                destination(cwd, &options(None, compact, json)),
                Err(REPORT_NEEDS_OUTPUT.to_owned())
            );
        }
        for (compact, json, format, mode) in [
            (false, false, ReportFormat::Text, ReportMode::Hierarchical),
            (true, false, ReportFormat::Text, ReportMode::Compact),
            (false, true, ReportFormat::Json, ReportMode::Hierarchical),
            (true, true, ReportFormat::Json, ReportMode::Compact),
        ] {
            assert_eq!(
                destination(cwd, &options(Some("report"), compact, json)),
                Ok(Some(Destination {
                    path: cwd.join("report"),
                    format,
                    mode,
                }))
            );
        }
        let missing = destination(cwd, &options(Some("missing/report"), false, false)).unwrap_err();
        assert!(
            missing.starts_with("the directory of --output does not exist"),
            "{missing}"
        );
        let directory = destination(cwd, &options(Some("sub"), false, true)).unwrap_err();
        assert!(
            directory.starts_with("--output names a directory"),
            "{directory}"
        );
    }

    /// A step is named by its command and its position, counting from 1.
    #[test]
    fn a_step_is_named_by_its_command_and_position() {
        let decision = StepDecision {
            position: 2,
            command: "tool a b".to_owned(),
            verdict: Verdict::Cached,
        };
        assert_eq!(Live::named(&decision), "`tool a b` (step 3)");
    }

    /// The summary is one line, without passes.
    #[test]
    fn the_summary_is_one_line_without_passes() {
        assert_eq!(
            summary_line("run succeeded", "kizz.md:3 (Lint)", 1, 2),
            "run succeeded: kizz.md:3 (Lint); 1 command ran, 2 cached"
        );
        assert_eq!(
            summary_line("run failed", "kizz.md:3 (Lint)", 0, 0),
            "run failed: kizz.md:3 (Lint); 0 commands ran, 0 cached"
        );
    }
}
