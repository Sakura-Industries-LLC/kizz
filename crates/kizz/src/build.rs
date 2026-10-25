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
//! of those lines. Each pass after the first starts with a line that says what the
//! pass before did: a command ran, a task program changed a file, or both. A failing
//! task prints its error, the Starlark stack, and the failing command's captured
//! output. Warnings are loud and repeated in the final summary, which counts the
//! commands that ran and those that came from the cache, and gives the number of
//! passes the build settled in. Among them: a command traced incompletely, which
//! names the reason, such as a system call that kizz cannot trace; a command during
//! whose run something else changed a file after the command read it; a command
//! that made system calls that the tracer does not know, once per step and build,
//! which says whether its result was cached, and without `--cache-unknown-syscalls`
//! that the command runs again on the next build; and a command that connected to
//! the network or to Unix sockets, once per step and build, which names each
//! address or socket, says that kizz cannot see what came back and that the result
//! is still cached, and that `always = True` runs a command on every build. A
//! command that rewrote its own input gets no warning, because its reason says so,
//! and a cache hit warns about nothing.
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
    BuildEvents, BuildOutcome, BuildReport, ChangingInput, Engine, LoadError, PrintSink,
    ProgramWrite, Project, Render, StepConnections, StepDecision, TargetExpr, TaskInfo, TaskResult,
    UnknownCalls, Warning, build, load, task_id, task_label,
};
use kizz_store::{
    CacheLock, LockDetails, NotCachedReason, StepKey, StepRecord, Store, TaskId, Verdict,
    cache_dir_from_env, connections_text, system_call_numbers,
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
    /// Cache the results of commands that made system calls that the tracer does
    /// not know, `--cache-unknown-syscalls`.
    pub cache_unknown_syscalls: bool,
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
    )
    .with_cache_unknown_system_calls(options.cache_unknown_syscalls);
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
        pass_ran: Cell::new(false),
        pass_changed: Cell::new(false),
        repeated_shown: Cell::new(0),
        incomplete_shown: RefCell::new(BTreeSet::new()),
        changed_shown: RefCell::new(BTreeSet::new()),
        unknown_shown: RefCell::new(BTreeSet::new()),
        connections_shown: RefCell::new(BTreeSet::new()),
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

/// What a pass did, as the build output says it: `a command ran`, `a task program
/// changed a file`, or both; `ran` says whether a command ran in it, and `changed`
/// whether a task program changed a file in it.
const fn work_done(
    ran: bool,
    changed: bool,
) -> &'static str {
    match (ran, changed) {
        (true, true) => "a command ran and a task program changed a file",
        (false, true) => "a task program changed a file",
        (_, false) => "a command ran",
    }
}

/// The line at the start of pass `pass`, a pass after the first, which says what
/// the pass before did: `ran` says whether a command ran in it, and `changed`
/// whether a task program changed a file in it.
fn pass_line(
    pass: usize,
    ran: bool,
    changed: bool,
) -> String {
    format!(
        "pass {pass}: {} in pass {}, so every selected task is checked again",
        work_done(ran, changed),
        pass.saturating_sub(1)
    )
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
    /// Whether a command ran in the current pass.
    pass_ran: Cell<bool>,
    /// Whether a task program changed a file in the current pass.
    pass_changed: Cell<bool>,
    /// How many of the engine's warnings for repeated self-inflicted reasons are
    /// printed.
    repeated_shown: Cell<usize>,
    /// The incompletely traced steps already warned about.
    incomplete_shown: RefCell<BTreeSet<StepKey>>,
    /// The steps already warned about because something other than the command
    /// changed a file it read while it ran.
    changed_shown: RefCell<BTreeSet<StepKey>>,
    /// The steps already warned about because their command made system calls that
    /// the tracer does not know.
    unknown_shown: RefCell<BTreeSet<StepKey>>,
    /// The steps already warned about because their command made connections.
    connections_shown: RefCell<BTreeSet<StepKey>>,
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
            ui::print_summary(&pass_line(
                pass,
                self.pass_ran.get(),
                self.pass_changed.get(),
            ));
        }
        self.pass_ran.set(false);
        self.pass_changed.set(false);
    }

    fn task_finished(
        &self,
        pass: usize,
        result: &TaskResult,
    ) {
        let task = self.render.task(&task_label(&result.task));
        self.pass_ran.set(self.pass_ran.get() || result.ran());
        self.pass_changed
            .set(self.pass_changed.get() || result.changed_files());
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
    /// that re-ran in consecutive builds for the same self-inflicted reason, runs
    /// that were traced incompletely, runs in which something other than the
    /// command changed a file after the command read it, runs that made system
    /// calls that the tracer does not know, and runs that made connections, each
    /// step once per build.
    fn check_warnings(&self) {
        let repeated = self.engine.warnings();
        for warning in repeated.iter().skip(self.repeated_shown.get()) {
            self.warn(self.render.repeated(warning));
        }
        self.repeated_shown.set(repeated.len());
        for (key, why) in self.engine.not_cached() {
            match why {
                NotCachedReason::Incomplete { detail } => {
                    if self.incomplete_shown.borrow_mut().insert(key) {
                        self.warn(self.incomplete(&key, &detail));
                    }
                },
                NotCachedReason::ChangedAfterRead { files } => {
                    // A command that rewrote its own input gets no warning: its
                    // build line and explanations give the reason.
                    let outside: Vec<&Path> = files
                        .iter()
                        .filter(|file| !file.written)
                        .map(|file| file.path.as_path())
                        .collect();
                    if !outside.is_empty() && self.changed_shown.borrow_mut().insert(key) {
                        self.warn(self.changed_outside(&key, &outside));
                    }
                },
                NotCachedReason::Signaled { .. }
                | NotCachedReason::Interrupted { .. }
                | NotCachedReason::InterruptedExited { .. }
                | NotCachedReason::StartFailed { .. }
                | NotCachedReason::UnknownSystemCalls { .. } => {},
            }
        }
        for calls in self.engine.unknown_system_calls() {
            if self.unknown_shown.borrow_mut().insert(calls.key) {
                self.warn(self.unknown_calls(&calls));
            }
        }
        for made in self.engine.connections() {
            if self.connections_shown.borrow_mut().insert(made.key) {
                self.warn(self.connected(&made));
            }
        }
    }

    /// The command of the step `key` as warnings name it: its arguments and its
    /// working directory, or `a command` when its record cannot be read.
    fn command(
        &self,
        key: &StepKey,
    ) -> String {
        let record = self
            .store
            .read()
            .and_then(|txn| txn.get::<StepRecord>(key))
            .ok()
            .flatten();
        record.map_or_else(
            || "a command".to_owned(),
            |record| {
                format!(
                    "`{}` in {}",
                    record.spec.argv.join(" "),
                    self.render.path(&record.spec.cwd)
                )
            },
        )
    }

    /// The warning for the incompletely traced step `key`.
    fn incomplete(
        &self,
        key: &StepKey,
        detail: &str,
    ) -> String {
        format!(
            "{} was traced incompletely, so its result was not cached and it runs again on \
             the next build: {detail}",
            self.command(key)
        )
    }

    /// The warning for the step `key`, in which something other than the command
    /// changed the files `paths` while it ran, after the command read them.
    fn changed_outside(
        &self,
        key: &StepKey,
        paths: &[&Path],
    ) -> String {
        let shown: Vec<String> = paths
            .iter()
            .map(|path| format!("`{}`", self.render.path(path)))
            .collect();
        let it = if paths.len() == 1 { "it" } else { "them" };
        format!(
            "something else changed {} while {} ran, after the command read {it}, so its \
             result was not cached and it runs again on the next build",
            shown.join(", "),
            self.command(key)
        )
    }

    /// The warning for the run `calls`, which made system calls that the tracer does
    /// not know: the command, each number, and whether its result was cached; when
    /// the build does not cache such results, also that the command runs again on
    /// the next build, and the option that would cache it.
    fn unknown_calls(
        &self,
        calls: &UnknownCalls,
    ) -> String {
        let outcome = if calls.cached {
            "; its result was cached because `kizz build --cache-unknown-syscalls` was given"
        } else if self.engine.caches_unknown_system_calls() {
            "; its result was not cached for another reason, and it runs again on the next build"
        } else {
            ", so its result was not cached and it runs again on the next build; `kizz build \
             --cache-unknown-syscalls` caches the results of such commands"
        };
        format!(
            "{} made {}, which the tracer does not know{outcome}",
            self.command(&calls.key),
            system_call_numbers(&calls.numbers)
        )
    }

    /// The warning for the run `made`, which connected to the network or to Unix
    /// sockets: the command and each address or socket, that kizz cannot see what
    /// came back, whether its result is still cached, and that `always = True` runs
    /// a command on every build.
    fn connected(
        &self,
        made: &StepConnections,
    ) -> String {
        let outcome = if made.cached {
            "and its result is still cached"
        } else {
            "and its result was not cached for another reason"
        };
        format!(
            "{} connected to {}; kizz cannot see what came back, {outcome}; `always = True` \
             runs a command on every build",
            self.command(&made.key),
            connections_text(&made.connections, |path| {
                format!("`{}`", self.render.path(path))
            })
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
                written,
                worked,
            } => {
                self.not_settled(*limit, changing, written, worked);
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
    /// inputs `changing` that kept changing and who writes them, the files `written`
    /// that task programs kept changing, or else the tasks `worked` that kept
    /// running commands.
    fn not_settled(
        &self,
        limit: usize,
        changing: &[ChangingInput],
        written: &[ProgramWrite],
        worked: &[TaskInfo],
    ) {
        let what = match (self.pass_ran.get(), self.pass_changed.get()) {
            (true, true) => "still ran a command, and a task program still changed a file",
            (false, true) => "a task program still changed a file",
            (_, false) => "still ran a command",
        };
        ui::print_error(&format!(
            "the build did not settle: its last allowed pass, pass {limit}, {what}"
        ));
        if changing.is_empty() && written.is_empty() {
            for task in worked {
                ui::print_error_detail(&format!(
                    "{} kept running commands",
                    self.render.task(&task_label(task))
                ));
            }
            return;
        }
        if !changing.is_empty() {
            ui::print_error_detail("these inputs kept changing:");
        }
        for input in changing {
            ui::print_error_detail(&format!(
                "`{}`, read by step {} of {}, last written by {}",
                self.render.path(&input.path),
                input.position.saturating_add(1),
                self.render.task(&input.reader),
                self.render.writer(&input.writer)
            ));
        }
        if !written.is_empty() {
            ui::print_error_detail("task programs kept changing these files:");
        }
        for file in written {
            ui::print_error_detail(&format!(
                "`{}`, written by the program of {}",
                self.render.path(&file.path),
                self.render.task(&file.writer)
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

/// Unit tests for counting the commands of a build and for the line at the start
/// of a pass.
#[cfg(test)]
mod tests {
    use kizz_core::{BuildOutcome, BuildReport, Pass, StepDecision, TaskInfo, TaskResult};
    use kizz_store::{Explanation, Reason, Verdict};
    use kizz_trace::AbsolutePath;

    use super::{command_counts, pass_line};

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
            changed: Vec::new(),
        }
    }

    /// The line at the start of a pass says what the pass before did.
    #[test]
    fn the_pass_line_says_what_the_pass_before_did() {
        assert_eq!(
            pass_line(2, true, false),
            "pass 2: a command ran in pass 1, so every selected task is checked again"
        );
        assert_eq!(
            pass_line(3, false, true),
            "pass 3: a task program changed a file in pass 2, so every selected task is checked \
             again"
        );
        assert_eq!(
            pass_line(4, true, true),
            "pass 4: a command ran and a task program changed a file in pass 3, so every \
             selected task is checked again"
        );
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
