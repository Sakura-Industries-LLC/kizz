// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz build` command: build the tasks that a target expression selects in the
//! current directory and below, with the Linux tracer ("Command line", "Order of
//! execution", "Failure and interruption", "Lock file").
//!
//! Without a target, the command lists the available targets and exits with
//! [`USAGE_ERROR`]. Otherwise it takes the cache lock, saying what it waits for while
//! another process holds it; opens the store; discovers and selects the tasks; and
//! runs the build of `kizz_core` with the Linux tracer, which only this binary wires
//! in. At the end it closes the database, then releases and deletes the lock file.
//!
//! Output: one line for each command that runs, with its one-line reason; with
//! `--verbose` or `--explain`, one line for each command that came from the cache,
//! the first time the build reaches it; with `--explain`, the full reasons under each
//! of those lines. A failing task prints its error, the Starlark stack, and the
//! failing command's captured output. Warnings are loud and repeated in the final
//! summary, which counts the commands that ran and those that came from the cache,
//! and gives the number of passes the build settled in.
//!
//! Exit status: 0 on success, [`TASK_FAILED`] when a task fails or the build does not
//! settle, [`USAGE_ERROR`] for usage and configuration errors and for a cache that
//! cannot be used, and [`INTERRUPTED`] when interrupted. On Ctrl-C (`SIGINT`) the
//! build starts nothing new, and the tracer interrupts the running command, which
//! receives `SIGINT` exactly once, and kills whatever of it still runs 2 seconds
//! later; its result is not cached, however it ends. Kizz then closes the
//! database, releases and deletes the lock file, and exits with [`INTERRUPTED`].

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
};

use kizz_core::{
    BuildEvents, BuildOutcome, BuildReport, ChangingInput, Engine, LoadError, PrintSink, Project,
    Render, StepDecision, TargetExpr, TaskInfo, TaskResult, Warning, build, load, task_id,
    task_label,
};
use kizz_store::{
    CacheLock, LockDetails, NotCachedReason, StepKey, StepRecord, Store, TaskId, Verdict,
    cache_dir_from_env,
};
use kizz_trace::{AbsolutePath, Cancellation, StdioEcho};
use kizz_trace_linux::LinuxTracer;
use signal_hook::{consts::SIGINT, iterator::Signals};

use crate::{
    cli::{INTERRUPTED, TASK_FAILED, USAGE_ERROR},
    ui,
};

/// What `kizz build` prints beyond one line for each command that runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
    /// Print the full reason for every command, cached ones included.
    pub explain: bool,
    /// List the cached commands, and show environment values in reasons.
    pub verbose: bool,
}

/// Build the tasks that `targets` selects under the current directory, and return
/// the exit status.
pub(crate) fn run(
    targets: Option<&str>,
    options: Options,
) -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return failure(&format!("cannot read the current directory: {error}"));
        },
    };
    let Some(targets) = targets else {
        return missing_target(&cwd);
    };
    let expression = match targets.parse::<TargetExpr>() {
        Ok(expression) => expression,
        Err(error) => return failure(&error.to_string()),
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
    // From here on, Ctrl-C cancels the build instead of killing kizz, so kizz can
    // close the database and delete the lock file. The tracer forwards the
    // interrupt to the running command, which runs in a process group of its own,
    // so a terminal's Ctrl-C reaches it only this way; another Ctrl-C sets the same
    // flag again and sends nothing more.
    let signals = match Signals::new([SIGINT]) {
        Ok(signals) => signals,
        Err(error) => {
            drop(lock);
            return failure(&format!("cannot handle Ctrl-C: {error}"));
        },
    };
    let handle = signals.handle();
    let cancel = Cancellation::new();
    let status = thread::scope(|scope| {
        let watcher = cancel.clone();
        scope.spawn(move || {
            let mut signals = signals;
            for _signal in signals.forever() {
                watcher.cancel();
            }
        });
        let mut status = with_store(&cache, &cwd, &expression, options, &cancel);
        if let Err(error) = lock.release() {
            ui::print_error(&error.to_string());
            status = worse(status);
        }
        handle.close();
        status
    });
    ExitCode::from(status)
}

/// `status`, or [`USAGE_ERROR`] in place of success, after an error in using the
/// cache.
const fn worse(status: u8) -> u8 {
    if status == 0 { USAGE_ERROR } else { status }
}

/// Open the store in `cache`, build the tasks that `expression` selects under `cwd`,
/// and close the store; return the exit status.
fn with_store(
    cache: &Path,
    cwd: &Path,
    expression: &TargetExpr,
    options: Options,
    cancel: &Cancellation,
) -> u8 {
    let store = match Store::open(cache) {
        Ok(store) => store,
        Err(error) => {
            ui::print_error(&error.to_string());
            return USAGE_ERROR;
        },
    };
    let mut status = build_selected(&store, cwd, expression, options, cancel);
    if let Err(error) = store.close() {
        ui::print_error(&error.to_string());
        status = worse(status);
    }
    status
}

/// Discover the tasks under `cwd`, select those of `expression`, and build them with
/// `store`; return the exit status.
fn build_selected(
    store: &Store,
    cwd: &Path,
    expression: &TargetExpr,
    options: Options,
    cancel: &Cancellation,
) -> u8 {
    let Some(project) = load_project(cwd) else {
        return USAGE_ERROR;
    };
    let mut warnings = print_warnings(&project.warnings);
    let selected = expression.select(&project.tasks);
    if selected.is_empty() {
        let message = format!(
            "`{expression}` selects no task under `.`; {}",
            available_targets(&project)
        );
        ui::print_warning(&message);
        warnings.push(message);
    }
    let mut tasks = Vec::with_capacity(selected.len());
    for chosen in &selected {
        match TaskInfo::new(chosen.task, &chosen.selected) {
            Ok(task) => tasks.push(task),
            Err(error) => {
                ui::print_error(&error.to_string());
                return USAGE_ERROR;
            },
        }
    }
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
    );
    let output = Output {
        render: Render {
            base: cwd.to_path_buf(),
            home,
            verbose: options.verbose,
        },
        options,
        engine: &engine,
        store,
        seen: RefCell::new(BTreeSet::new()),
        repeated_shown: Cell::new(0),
        incomplete_shown: RefCell::new(BTreeSet::new()),
        warnings: RefCell::new(warnings),
    };
    let status = match build(&engine, &tasks, &caller, &output, &output) {
        Ok(report) => output.finish(&report),
        Err(error) => {
            ui::print_error(&format!("cannot use the cache: {error}"));
            USAGE_ERROR
        },
    };
    ui::print_warning_summary(&output.warnings.borrow());
    status
}

/// Load the project under `cwd`, printing its configuration errors and warnings
/// when it has errors.
fn load_project(cwd: &Path) -> Option<Project> {
    match load(cwd) {
        Ok(project) => Some(project),
        Err(LoadError::Config { errors, warnings }) => {
            let warnings = print_warnings(&warnings);
            for error in &errors {
                ui::print_error(&error.to_string());
            }
            ui::print_warning_summary(&warnings);
            None
        },
        Err(error) => {
            ui::print_error(&error.to_string());
            None
        },
    }
}

/// Fail because no target was given, listing the available targets under `cwd`.
fn missing_target(cwd: &Path) -> ExitCode {
    let Some(project) = load_project(cwd) else {
        return ExitCode::from(USAGE_ERROR);
    };
    let warnings = print_warnings(&project.warnings);
    ui::print_error(&format!(
        "no target given; give one target, or several joined with `+`; {}",
        available_targets(&project)
    ));
    ui::print_warning_summary(&warnings);
    ExitCode::from(USAGE_ERROR)
}

/// The targets of the tasks of `project`, sorted, as a clause of a message.
fn available_targets(project: &Project) -> String {
    let targets: BTreeSet<&str> = project
        .tasks
        .iter()
        .flat_map(|task| task.targets.iter().map(String::as_str))
        .collect();
    if targets.is_empty() {
        "there is no task under `.`".to_owned()
    } else {
        let targets: Vec<&str> = targets.into_iter().collect();
        format!("the available targets are: {}", targets.join(", "))
    }
}

/// Print each warning loudly and return their messages for the final summary.
fn print_warnings(warnings: &[Warning]) -> Vec<String> {
    let messages: Vec<String> = warnings.iter().map(ToString::to_string).collect();
    for message in &messages {
        ui::print_warning(message);
    }
    messages
}

/// Print `message` as an error and return the usage-error status.
fn failure(message: &str) -> ExitCode {
    ui::print_error(message);
    ExitCode::from(USAGE_ERROR)
}

/// A step of a task as the build output tells steps apart: the task, the position
/// of the step in the task's run, and its command.
type StepId = (TaskId, u32, String);

/// The step `decision` of the task `task`.
fn step_id(
    task: &TaskInfo,
    decision: &StepDecision,
) -> StepId {
    (task_id(task), decision.position, decision.command.clone())
}

/// What the build printed so far, and how it prints.
struct Output<'a> {
    /// How paths, tasks, and reasons read.
    render: Render,
    /// What to print beyond the commands that run.
    options: Options,
    /// The engine of the build, for its warnings and its results that were not
    /// cached.
    engine: &'a Engine<'a>,
    /// The store of the build, for the commands of results that were not cached.
    store: &'a Store,
    /// Every step the build reached so far.
    seen: RefCell<BTreeSet<StepId>>,
    /// How many of the engine's warnings for repeated self-inflicted reasons are
    /// printed.
    repeated_shown: Cell<usize>,
    /// The incompletely traced steps already warned about.
    incomplete_shown: RefCell<BTreeSet<StepKey>>,
    /// Every warning printed, for the final summary.
    warnings: RefCell<Vec<String>>,
}

impl PrintSink for Output<'_> {
    fn print_line(
        &self,
        line: &str,
    ) {
        ui::print_line(line);
    }
}

impl BuildEvents for Output<'_> {
    fn pass_started(
        &self,
        pass: usize,
    ) {
        if pass > 1 {
            ui::print_summary(&format!(
                "pass {pass}: a command ran in pass {}, so every selected task is checked again",
                pass.saturating_sub(1)
            ));
        }
    }

    fn task_finished(
        &self,
        pass: usize,
        result: &TaskResult,
    ) {
        let task = self.render.task(&task_label(&result.task));
        for decision in &result.decisions {
            let first = self
                .seen
                .borrow_mut()
                .insert(step_id(&result.task, decision));
            self.print_step(&task, decision, first);
        }
        // A failure is shown when it is new: in the first pass, or when the task ran
        // a command; a task that fails again from the cache in a later pass is not
        // shown again.
        if let Err(failure) = &result.result
            && (pass == 1 || result.ran())
        {
            ui::print_task_failure(&task, &failure.to_string());
        }
        self.check_warnings();
    }
}

impl Output<'_> {
    /// Print the step `decision` of the task `task`, which the build reached for the
    /// first time when `first` is set.
    fn print_step(
        &self,
        task: &str,
        decision: &StepDecision,
        first: bool,
    ) {
        let named = format!(
            "`{}` in {task} step {}",
            decision.command,
            decision.position.saturating_add(1)
        );
        let cached = matches!(decision.verdict, Verdict::Cached);
        if cached && !(first && (self.options.verbose || self.options.explain)) {
            return;
        }
        if self.options.explain {
            if cached {
                ui::print_cached(&named);
            } else {
                ui::print_ran(&named);
            }
            for line in self.render.full(&decision.verdict) {
                ui::print_detail(&line);
            }
        } else if cached {
            ui::print_cached(&named);
        } else {
            ui::print_ran(&format!(
                "{named}: {}",
                self.render.one_line(&decision.verdict)
            ));
        }
    }

    /// Print, loudly, each warning the engine added since the last check: steps
    /// that re-ran in consecutive builds for the same self-inflicted reason, and
    /// runs that were traced incompletely.
    fn check_warnings(&self) {
        let repeated = self.engine.warnings();
        for warning in repeated.iter().skip(self.repeated_shown.get()) {
            self.warn(self.render.repeated(warning));
        }
        self.repeated_shown.set(repeated.len());
        for (key, why) in self.engine.not_cached() {
            if let NotCachedReason::Incomplete { detail } = why
                && self.incomplete_shown.borrow_mut().insert(key)
            {
                self.warn(self.incomplete(&key, &detail));
            }
        }
    }

    /// The warning for the incompletely traced step `key`.
    fn incomplete(
        &self,
        key: &StepKey,
        detail: &str,
    ) -> String {
        let record = self
            .store
            .read()
            .and_then(|txn| txn.get::<StepRecord>(key))
            .ok()
            .flatten();
        let command = record.map_or_else(
            || "a command".to_owned(),
            |record| {
                format!(
                    "`{}` in {}",
                    record.spec.argv.join(" "),
                    self.render.path(&record.spec.cwd)
                )
            },
        );
        format!(
            "{command} was traced incompletely, so its result was not cached and it runs \
             again on the next build: {detail}"
        )
    }

    /// Print `message` as a loud warning and keep it for the final summary.
    fn warn(
        &self,
        message: String,
    ) {
        ui::print_warning(&message);
        self.warnings.borrow_mut().push(message);
    }

    /// Print how the build ended and its summary; return the exit status.
    fn finish(
        &self,
        report: &BuildReport,
    ) -> u8 {
        let (verdict, failed, status) = match &report.outcome {
            BuildOutcome::Succeeded => ("build succeeded", 0, 0),
            BuildOutcome::Failed { failures } => {
                for failed in failures {
                    let missing: Vec<String> = failed
                        .missing
                        .iter()
                        .map(|path| format!("`{}`", self.render.path(path)))
                        .collect();
                    ui::print_failed_task(&self.render.task(&task_label(&failed.task)), &missing);
                }
                ("build failed", failures.len(), TASK_FAILED)
            },
            BuildOutcome::NotSettled {
                limit,
                changing,
                ran,
            } => {
                self.not_settled(*limit, changing, ran);
                ("build did not settle", 0, TASK_FAILED)
            },
            BuildOutcome::Interrupted => {
                ui::print_error(
                    "the build was interrupted: it started nothing new, the running command \
                     was interrupted, and its result was not cached",
                );
                ("build interrupted", 0, INTERRUPTED)
            },
        };
        let tasks = ui::count(report.order.len(), "task");
        let failed = if failed == 0 {
            String::new()
        } else {
            format!(", {failed} failed")
        };
        let passes = ui::count_as(report.settled_in(), "pass", "passes");
        let passes = match report.outcome {
            BuildOutcome::Succeeded | BuildOutcome::Failed { .. } => format!("settled in {passes}"),
            BuildOutcome::NotSettled { .. } | BuildOutcome::Interrupted => {
                format!("stopped after {passes}")
            },
        };
        let (ran, cached) = command_counts(report);
        ui::print_summary(&format!(
            "{verdict}: {tasks}{failed}, {passes}; {} ran, {cached} cached",
            ui::count(ran, "command")
        ));
        status
    }

    /// Print why a build that reached its pass limit `limit` did not settle: the
    /// inputs `changing` that kept changing and who writes them, or else the tasks
    /// `ran` that kept running commands.
    fn not_settled(
        &self,
        limit: usize,
        changing: &[ChangingInput],
        ran: &[TaskInfo],
    ) {
        ui::print_error(&format!(
            "the build did not settle: its last allowed pass, pass {limit}, still ran a command"
        ));
        if changing.is_empty() {
            for task in ran {
                ui::print_error_detail(&format!(
                    "{} kept running commands",
                    self.render.task(&task_label(task))
                ));
            }
            return;
        }
        ui::print_error_detail("these inputs kept changing:");
        for input in changing {
            ui::print_error_detail(&format!(
                "`{}`, read by step {} of {}, last written by {}",
                self.render.path(&input.path),
                input.position.saturating_add(1),
                self.render.task(&input.reader),
                self.render.writer(&input.writer)
            ));
        }
    }
}

/// The number of commands that ran in the build, every run counted, and the number
/// of steps that came from the cache every time the build reached them.
fn command_counts(report: &BuildReport) -> (usize, usize) {
    let mut ran = 0_usize;
    let mut reached = BTreeSet::new();
    let mut ran_steps = BTreeSet::new();
    for result in report.passes.iter().flat_map(|pass| &pass.tasks) {
        for decision in &result.decisions {
            let step = step_id(&result.task, decision);
            if matches!(decision.verdict, Verdict::Ran(_)) {
                ran = ran.saturating_add(1);
                ran_steps.insert(step.clone());
            }
            reached.insert(step);
        }
    }
    (ran, reached.difference(&ran_steps).count())
}

/// Unit tests for counting the commands of a build.
#[cfg(test)]
mod tests {
    use kizz_core::{BuildOutcome, BuildReport, Pass, StepDecision, TaskInfo, TaskResult};
    use kizz_store::{Explanation, Reason, Verdict};
    use kizz_trace::AbsolutePath;

    use super::command_counts;

    /// A task run whose steps have the verdicts `verdicts`, ran when `true`.
    fn result(verdicts: &[bool]) -> TaskResult {
        TaskResult {
            task: TaskInfo {
                dir: AbsolutePath::new("/w").unwrap(),
                file: AbsolutePath::new("/w/kizz.md").unwrap(),
                relative_file: "kizz.md".into(),
                line: 3,
                index: 0,
                name: Some("Build".to_owned()),
                targets: vec!["ci".to_owned()],
                selected: vec!["ci".to_owned()],
                source: String::new(),
            },
            decisions: verdicts
                .iter()
                .zip(0_u32..)
                .map(|(&ran, position)| {
                    StepDecision {
                        position,
                        command: format!("tool {position}"),
                        verdict: if ran {
                            Verdict::Ran(Explanation {
                                reasons: vec![Reason::NeverRun],
                                repeated: Vec::new(),
                            })
                        } else {
                            Verdict::Cached
                        },
                    }
                })
                .collect(),
            result: Ok(()),
            missing: Vec::new(),
        }
    }

    /// Every run counts as a command that ran, and a step counts as cached only
    /// when it came from the cache every time the build reached it.
    #[test]
    fn counts_every_run_and_the_steps_that_never_ran() {
        let report = BuildReport {
            order: Vec::new(),
            passes: vec![
                Pass {
                    number: 1,
                    tasks: vec![result(&[true, false, false])],
                },
                Pass {
                    number: 2,
                    tasks: vec![result(&[true, true, false])],
                },
                Pass {
                    number: 3,
                    tasks: vec![result(&[false, false, false])],
                },
            ],
            outcome: BuildOutcome::Succeeded,
        };
        assert_eq!(command_counts(&report), (3, 1));
    }
}
