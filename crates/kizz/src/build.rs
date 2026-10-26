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
//! and a cache hit warns about none of these.
//!
//! Once the build settles, a step whose non-zero exit or signal its task program
//! left unchecked, with no `expect` and without reading `rc` or `signal` or
//! encoding the result, gets a loud warning, which names the task, the command and
//! how it ended ("Exit codes and failure"). Only the results that stand warn, each
//! task's latest run in the build ([`BuildReport::unchecked`]), so a result that a
//! later pass replaced never warns; the warnings come after the last pass, before
//! the lines that say how the build ended, each step once, and the summary repeats
//! them. A warning says that the result came from the cache only when the build ran
//! no command under the step's key, so a result replayed from the cache of an
//! earlier build warns again with that note. A build that does not settle, or that
//! is interrupted, warns about no unchecked exit. With `--fail-unchecked`, such a
//! task fails when its program ends, and its failure prints each unchecked command,
//! its exit status and its captured output, as for an unexpected exit.
//!
//! Exit status: 0 on success, [`TASK_FAILED`] when a task fails or the build does not
//! settle, [`USAGE_ERROR`] for usage and configuration errors and for a cache that
//! cannot be used, and [`INTERRUPTED`] when interrupted. On Ctrl-C (`SIGINT`) the
//! build starts nothing new, and the tracer interrupts the running command, which
//! receives `SIGINT` exactly once, and kills whatever of it still runs 2 seconds
//! later; its result is not cached, however it ends. Kizz then closes the
//! database, releases and deletes the lock file, and exits with [`INTERRUPTED`].
//!
//! Threads ("Kizz never panics"): the thread that handles Ctrl-C starts through
//! `kizz_threads`, which waits and retries while the system cannot start a thread for
//! the moment; when it still cannot start, kizz prints the error, which names the
//! thread and the system's error, releases and deletes the lock file, and exits with
//! [`USAGE_ERROR`]. Discovery and the build's parallel work run on one
//! [`WorkerPool`], whose workers start the same way; a pool that cannot start is an
//! error with the same status. A thread of a command's tracer that cannot start fails
//! that command's step, and so its task.
//!
//! Other calls that could panic ("Kizz never panics"): the delivery of Ctrl-C is
//! made only for a signal that `signal_hook` accepts, which `Signals::new` would
//! otherwise answer with a panic, and its thread looks for signals with
//! `Signals::pending`, never with `Signals::forever`, whose iterator panics when
//! reading the delivery fails. What the build printed so far lives in `Cell`s, never
//! in `RefCell`s, whose borrows panic. Engine state that stays locked when the
//! warnings are read is an error that kizz prints once, and the build then exits
//! with [`USAGE_ERROR`] instead of success. Every line goes through the `ui`
//! helpers, which drop a write that fails.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::c_int,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::Duration,
};

use kizz_core::{
    BuildEvents, BuildOutcome, BuildReport, ChangingInput, Engine, LoadError, PrintSink,
    ProgramWrite, Project, Render, StepConnections, StepDecision, TargetExpr, TaskInfo, TaskResult,
    UncheckedStep, UnknownCalls, Warning, WorkerPool, build, load, task_id, task_label,
};
use kizz_store::{
    CacheLock, LockDetails, NotCachedReason, StepKey, StepRecord, Store, TaskId, Verdict,
    cache_dir_from_env, connections_text, system_call_numbers,
};
use kizz_threads::{LockError, StartError};
use kizz_trace::{AbsolutePath, Cancellation, StdioEcho};
use kizz_trace_linux::LinuxTracer;
use signal_hook::{
    consts::{FORBIDDEN, SIGINT},
    iterator::{Handle, Signals},
};

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
    /// What the engine does beyond the defaults.
    pub engine: EngineOptions,
}

/// The `kizz build` options that change what the engine does: neither is in any
/// step key or record.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EngineOptions {
    /// Cache the results of commands that made system calls that the tracer does
    /// not know, `--cache-unknown-syscalls`.
    pub cache_unknown_syscalls: bool,
    /// Fail a task whose program leaves a non-zero exit unchecked,
    /// `--fail-unchecked`.
    pub fail_unchecked: bool,
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
    let signals = match interrupt_signals(SIGINT) {
        Ok(signals) => signals,
        Err(error) => {
            drop(lock);
            return failure(&format!("cannot handle Ctrl-C: {error}"));
        },
    };
    let cancel = Cancellation::new();
    // When the signal thread cannot start, the build never runs: dropping it
    // releases the lock and deletes its file, and kizz exits with
    // [`USAGE_ERROR`], its status for its own errors.
    let status = match with_interrupts(signals, &cancel, || {
        let mut status = with_store(&cache, &cwd, &expression, options, &cancel);
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

/// The largest signal number, plus one, that `signal_hook`'s delivery accepts: its
/// `Handle::add_signal` panics at or above it.
const SIGNAL_LIMIT: c_int = 128;

/// How long the thread that handles Ctrl-C sleeps between its looks for signals.
const SIGNAL_POLL: Duration = Duration::from_millis(20);

/// A delivery of `signal`, or the error of a signal that cannot be handled.
///
/// `Signals::new` "panics" "if the given signal is forbidden" or "if the signal
/// number is negative or larger than internal limit" (`signal_hook`'s documentation
/// of `Handle::add_signal`, whose restrictions `Signals::new` shares), so this
/// refuses such a signal first, with an error, and calls `Signals::new` only for
/// one that it accepts ("Kizz never panics").
///
/// # Errors
///
/// A message for a signal that `signal_hook` would panic on, or the error of
/// `Signals::new`.
fn interrupt_signals(signal: c_int) -> Result<Signals, String> {
    if signal <= 0 || signal >= SIGNAL_LIMIT || FORBIDDEN.contains(&signal) {
        return Err(format!("signal {signal} cannot be handled"));
    }
    Signals::new([signal]).map_err(|error| error.to_string())
}

/// Run `work` while a thread cancels `cancel` at each signal that `signals`
/// delivers, and return what `work` returns.
///
/// The thread starts through `kizz_threads`, which waits and retries while the
/// system cannot start a thread for the moment. When it still cannot start, `work`
/// never runs and is dropped, and the error names the thread and the system's error.
///
/// The thread looks for signals with `Signals::pending`, which never blocks and
/// cannot panic, every [`SIGNAL_POLL`], until the delivery is closed; it does not
/// use `Signals::forever` or `Signals::wait`, whose code panics with "Unexpected
/// error" when reading the delivery fails. The scope that runs it waits for it
/// before it returns, also when `work` panics. A [`CloseOnDrop`] therefore closes
/// the delivery whichever way `work` ends, and the thread ends at its next look.
fn with_interrupts<T>(
    signals: Signals,
    cancel: &Cancellation,
    work: impl FnOnce() -> T,
) -> Result<T, StartError> {
    let handle = signals.handle();
    thread::scope(|scope| {
        let _close = CloseOnDrop(handle);
        let watcher = cancel.clone();
        kizz_threads::spawn_scoped(
            scope,
            "kizz-signals",
            "the thread that handles Ctrl-C",
            move || {
                let mut signals = signals;
                while !signals.is_closed() {
                    for _signal in signals.pending() {
                        watcher.cancel();
                    }
                    thread::sleep(SIGNAL_POLL);
                }
            },
        )?;
        Ok(work())
    })
}

/// Closes a signal delivery when dropped, which ends the thread that looks for its
/// signals; dropped at the end of [`with_interrupts`]'s scope, and while a panic
/// unwinds out of it, before the scope waits for that thread.
struct CloseOnDrop(Handle);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
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
    let Some(pool) = worker_pool() else {
        return USAGE_ERROR;
    };
    let Some(project) = load_project(cwd, &pool) else {
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
        &pool,
    )
    .with_cache_unknown_system_calls(options.engine.cache_unknown_syscalls)
    .with_fail_unchecked(options.engine.fail_unchecked);
    let output = Output {
        render: Render {
            base: cwd.to_path_buf(),
            home,
            verbose: options.verbose,
        },
        options,
        engine: &engine,
        store,
        seen: Cell::new(BTreeSet::new()),
        pass_ran: Cell::new(false),
        pass_changed: Cell::new(false),
        repeated_shown: Cell::new(0),
        incomplete_shown: Cell::new(BTreeSet::new()),
        changed_shown: Cell::new(BTreeSet::new()),
        unknown_shown: Cell::new(BTreeSet::new()),
        connections_shown: Cell::new(BTreeSet::new()),
        warnings: Cell::new(warnings),
        unreadable: Cell::new(None),
    };
    let status = match build(&engine, &tasks, &caller, &output, &output) {
        Ok(report) => output.finish(&report),
        Err(error) => {
            ui::print_error(&format!("cannot use the cache: {error}"));
            USAGE_ERROR
        },
    };
    ui::print_warning_summary(&output.warnings.take());
    status
}

/// The worker pool of a command, which runs its parallel work, or `None` after
/// printing why a worker could not start.
pub(crate) fn worker_pool() -> Option<WorkerPool> {
    match WorkerPool::new() {
        Ok(pool) => Some(pool),
        Err(error) => {
            ui::print_error(&error.to_string());
            None
        },
    }
}

/// Load the project under `cwd`, walking on `pool`, printing its configuration
/// errors and warnings when it has errors.
fn load_project(
    cwd: &Path,
    pool: &WorkerPool,
) -> Option<Project> {
    match load(cwd, pool) {
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
    let Some(pool) = worker_pool() else {
        return ExitCode::from(USAGE_ERROR);
    };
    let Some(project) = load_project(cwd, &pool) else {
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

/// Change the value in `cell` with `change`, and return what `change` returns: the
/// value is taken out, changed and put back with `Cell::take` and `Cell::set`,
/// which cannot fail, where a `RefCell` borrow would panic while the cell is
/// borrowed ("Kizz never panics"). No `change` here reaches its own cell.
fn update<T: Default, R>(
    cell: &Cell<T>,
    change: impl FnOnce(&mut T) -> R,
) -> R {
    let mut value = cell.take();
    let result = change(&mut value);
    cell.set(value);
    result
}

/// Whether `key` is new to the set in `cell`, which then holds it.
fn first_time<K: Ord>(
    cell: &Cell<BTreeSet<K>>,
    key: K,
) -> bool {
    update(cell, |keys| keys.insert(key))
}

/// What the build printed so far, and how it prints.
///
/// The state that the build's callbacks change through `&self` lives in `Cell`s,
/// changed with [`update`], never in `RefCell`s, whose borrows panic while the cell
/// is borrowed ("Kizz never panics").
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
    seen: Cell<BTreeSet<StepId>>,
    /// Whether a command ran in the current pass.
    pass_ran: Cell<bool>,
    /// Whether a task program changed a file in the current pass.
    pass_changed: Cell<bool>,
    /// How many of the engine's warnings for repeated self-inflicted reasons are
    /// printed.
    repeated_shown: Cell<usize>,
    /// The incompletely traced steps already warned about.
    incomplete_shown: Cell<BTreeSet<StepKey>>,
    /// The steps already warned about because something other than the command
    /// changed a file it read while it ran.
    changed_shown: Cell<BTreeSet<StepKey>>,
    /// The steps already warned about because their command made system calls that
    /// the tracer does not know.
    unknown_shown: Cell<BTreeSet<StepKey>>,
    /// The steps already warned about because their command made connections.
    connections_shown: Cell<BTreeSet<StepKey>>,
    /// Every warning printed, for the final summary.
    warnings: Cell<Vec<String>>,
    /// The first error of engine state that stayed locked when the warnings were
    /// read, if any.
    unreadable: Cell<Option<LockError>>,
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
            let first = first_time(&self.seen, step_id(&result.task, decision));
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
        if let Err(error) = self.check_warnings() {
            // Only the first such error of the build is printed.
            self.unreadable(&error);
        }
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
    /// step once per build. Unchecked exits warn only once the build settles, in
    /// [`Output::finish`].
    ///
    /// # Errors
    ///
    /// The [`LockError`] of engine state that stayed locked, after the warnings that
    /// could be read.
    fn check_warnings(&self) -> Result<(), LockError> {
        let repeated = self.engine.warnings()?;
        for warning in repeated.iter().skip(self.repeated_shown.get()) {
            self.warn(self.render.repeated(warning));
        }
        self.repeated_shown.set(repeated.len());
        for (key, why) in self.engine.not_cached()? {
            match why {
                NotCachedReason::Incomplete { detail } => {
                    if first_time(&self.incomplete_shown, key) {
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
                    if !outside.is_empty() && first_time(&self.changed_shown, key) {
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
        for calls in self.engine.unknown_system_calls()? {
            if first_time(&self.unknown_shown, calls.key) {
                self.warn(self.unknown_calls(&calls));
            }
        }
        for made in self.engine.connections()? {
            if first_time(&self.connections_shown, made.key) {
                self.warn(self.connected(&made));
            }
        }
        Ok(())
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

    /// The warning for the step `step`, whose non-zero exit or signal its task
    /// program left unchecked in a result that stands when the build settles: the
    /// task, the command and how it ended, that the program has no `expect` and
    /// never read `rc` or `signal`, and, when the build ran no command under the
    /// step's key, that the result came from the cache; then either that the task
    /// fails, with `--fail-unchecked`, or that the option would fail it.
    fn unchecked(
        &self,
        step: &UncheckedStep,
    ) -> String {
        let cached = if step.ran {
            ""
        } else {
            "; the result came from the cache"
        };
        let outcome = if self.engine.fails_unchecked() {
            "the task fails because `kizz build --fail-unchecked` was given"
        } else {
            "`kizz build --fail-unchecked` makes this fail the task"
        };
        format!(
            "{} left {} unchecked: {}, but its `run()` has no `expect` and the task program never \
             read the result's `rc` or `signal`{cached}; {outcome}",
            self.render.task(&step.task),
            self.command(&step.key),
            step.exit.termination,
        )
    }

    /// Print `message` as a loud warning and keep it for the final summary.
    fn warn(
        &self,
        message: String,
    ) {
        ui::print_warning(&message);
        update(&self.warnings, |warnings| warnings.push(message));
    }

    /// Print, once, the error of engine state that stayed locked, so the build's
    /// warnings cannot be read, and keep it, so the build exits with
    /// [`USAGE_ERROR`] at the end instead of success; return whether this call
    /// printed it, which only the first error of a build does.
    fn unreadable(
        &self,
        error: &LockError,
    ) -> bool {
        update(&self.unreadable, |unreadable| {
            if unreadable.is_some() {
                return false;
            }
            ui::print_error(&format!("cannot read the build's warnings: {error}"));
            *unreadable = Some(error.clone());
            true
        })
    }

    /// Whether engine state stayed locked at some check, so warnings may be missing.
    fn had_unreadable(&self) -> bool {
        update(&self.unreadable, |unreadable| unreadable.is_some())
    }

    /// Print the unchecked exits that stand, then how the build ended and its
    /// summary; return the exit status.
    fn finish(
        &self,
        report: &BuildReport,
    ) -> u8 {
        for step in report.unchecked() {
            self.warn(self.unchecked(&step));
        }
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
        if self.had_unreadable() {
            worse(status)
        } else {
            status
        }
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

/// Unit tests for counting the commands of a build, for the line at the start of a
/// pass, for the thread that handles Ctrl-C, and for the cells of the output.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: a plain std thread bounds how long a test waits, and the tests \
              assert"
)]
mod tests {
    use std::{
        cell::Cell,
        collections::BTreeSet,
        panic::{self, AssertUnwindSafe},
        path::PathBuf,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use kizz_core::{
        BuildEvents as _, BuildOutcome, BuildReport, Engine, Pass, Render, StepDecision, TaskInfo,
        TaskResult, WorkerPool,
    };
    use kizz_store::{Explanation, Reason, Store, Verdict};
    use kizz_threads::{
        LOCK_PATIENCE,
        testing::{Failure, fail_starts_named},
    };
    use kizz_trace::{AbsolutePath, Cancellation, StdioEcho};
    use kizz_trace_linux::LinuxTracer;
    use signal_hook::{
        consts::{SIGKILL, SIGSEGV, SIGUSR1, SIGUSR2},
        iterator::Signals,
        low_level,
    };

    use super::{
        EngineOptions, Options, Output, SIGNAL_LIMIT, USAGE_ERROR, command_counts, first_time,
        interrupt_signals, pass_line, update, with_interrupts,
    };

    /// A signal that `signal_hook` would panic on is an error before any
    /// registration: a forbidden signal, zero, a negative number, and a number at
    /// its limit; an ordinary signal gives a delivery.
    #[test]
    fn a_signal_that_would_panic_is_refused_with_an_error() {
        for signal in [SIGKILL, SIGSEGV, 0, -1, SIGNAL_LIMIT] {
            let error = interrupt_signals(signal).unwrap_err();
            assert_eq!(error, format!("signal {signal} cannot be handled"));
        }
        assert!(interrupt_signals(SIGUSR2).is_ok());
    }

    /// A signal that arrives while the build runs cancels it: the thread that
    /// handles Ctrl-C looks for signals with `Signals::pending`, which cannot panic.
    #[test]
    fn a_signal_during_the_build_cancels_it() {
        let signals = interrupt_signals(SIGUSR1).unwrap();
        let cancel = Cancellation::new();
        let seen = with_interrupts(signals, &cancel, || {
            low_level::raise(SIGUSR1).unwrap();
            let deadline = Instant::now().checked_add(PATIENCE).unwrap();
            while !cancel.is_cancelled() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            cancel.is_cancelled()
        })
        .unwrap();
        assert!(seen, "the signal did not cancel the build");
    }

    /// A change through a cell stays in it and returns its result, and a key is
    /// new only the first time.
    #[test]
    fn cells_change_in_place_without_a_borrow() {
        let cell = Cell::new(vec![1]);
        assert_eq!(
            update(&cell, |values| {
                values.push(2);
                values.len()
            }),
            2
        );
        assert_eq!(cell.take(), [1, 2]);
        let seen = Cell::new(BTreeSet::new());
        assert!(first_time(&seen, "a"));
        assert!(!first_time(&seen, "a"));
        assert!(first_time(&seen, "b"));
        assert_eq!(seen.take(), BTreeSet::from(["a", "b"]));
    }

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
            unchecked: Vec::new(),
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

    /// How long the test waits for a build that panicked to end.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// A build that panics while the signal thread waits for signals still ends:
    /// the delivery is closed while the panic unwinds, before the scope waits for
    /// that thread, and the panic reaches the caller. Without the close, the thread
    /// waits for a signal for good, and so does the scope, as kizz did when it could
    /// not start a thread during a build.
    #[test]
    fn a_panic_in_the_build_does_not_wait_for_the_signal_thread() {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            // A signal that nothing in the test process uses.
            let signals = Signals::new([SIGUSR2]).unwrap();
            let cancel = Cancellation::new();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                with_interrupts(signals, &cancel, || -> u8 {
                    panic::resume_unwind(Box::new("the build could not start a thread"))
                })
            }));
            sender.send(outcome.is_err()).ok();
        });
        assert_eq!(
            receiver.recv_timeout(PATIENCE),
            Ok(true),
            "the build that panicked did not end"
        );
    }

    /// A build that ends normally returns its result after the signal thread ended.
    #[test]
    fn a_build_returns_its_result_after_the_signal_thread() {
        let signals = Signals::new([SIGUSR2]).unwrap();
        let cancel = Cancellation::new();
        assert_eq!(with_interrupts(signals, &cancel, || 7).unwrap(), 7);
        assert!(!cancel.is_cancelled());
    }

    /// When the signal thread cannot start, even after its retries, the build never
    /// runs, and the error names the thread and error 11 instead of a panic.
    #[test]
    fn a_signal_thread_that_cannot_start_is_an_error() {
        let signals = Signals::new([SIGUSR2]).unwrap();
        let cancel = Cancellation::new();
        let failures = fail_starts_named(11, Failure::Busy, "kizz-signals");
        let mut ran = false;
        let error = with_interrupts(signals, &cancel, || ran = true).unwrap_err();
        assert!(!ran);
        assert_eq!(failures.remaining(), 0);
        assert_eq!(error.error().raw_os_error(), Some(11));
        assert!(
            error.to_string().starts_with(
                "could not start the thread that handles Ctrl-C after 11 attempts in 22.3 seconds"
            ),
            "{error}"
        );
    }

    /// The short patience of the engine in the tests, in place of
    /// [`LOCK_PATIENCE`], so state that stays locked fails at once.
    const TEST_PATIENCE: Duration = Duration::from_millis(50);

    /// The output of a build of `engine`, which prints only the commands that run.
    fn output<'a>(
        engine: &'a Engine<'a>,
        store: &'a Store,
    ) -> Output<'a> {
        Output {
            render: Render {
                base: PathBuf::from("/w"),
                home: None,
                verbose: false,
            },
            options: Options {
                explain: false,
                verbose: false,
                engine: EngineOptions {
                    cache_unknown_syscalls: false,
                    fail_unchecked: false,
                },
            },
            engine,
            store,
            seen: Cell::new(BTreeSet::new()),
            pass_ran: Cell::new(false),
            pass_changed: Cell::new(false),
            repeated_shown: Cell::new(0),
            incomplete_shown: Cell::new(BTreeSet::new()),
            changed_shown: Cell::new(BTreeSet::new()),
            unknown_shown: Cell::new(BTreeSet::new()),
            connections_shown: Cell::new(BTreeSet::new()),
            warnings: Cell::new(Vec::new()),
            unreadable: Cell::new(None),
        }
    }

    /// When the engine's state stays locked as a finished task's warnings are
    /// read, the reading is an error that names the state; the build prints it
    /// once, keeps the first error, and exits with [`USAGE_ERROR`] instead of
    /// success. A build whose warnings were read exits with 0. The engine waits
    /// only the tests' short patience, so the test takes a fraction of a second.
    #[test]
    fn warnings_that_stay_locked_are_printed_once_and_fail_the_build() {
        let started = Instant::now();
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let pool = WorkerPool::new().unwrap();
        let tracer = LinuxTracer::new();
        let engine = Engine::new(
            &tracer,
            &store,
            &StdioEcho,
            Cancellation::new(),
            None,
            &pool,
        )
        .with_lock_patience(TEST_PATIENCE);
        let report = BuildReport {
            order: Vec::new(),
            passes: Vec::new(),
            outcome: BuildOutcome::Succeeded,
        };

        let readable = output(&engine, &store);
        readable.task_finished(1, &result(&[]));
        assert!(!readable.had_unreadable());
        assert_eq!(readable.finish(&report), 0);

        let locked = output(&engine, &store);
        let held = engine.hold_warnings().unwrap();
        let error = locked.check_warnings().unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("could not lock the build's warnings for repeated reasons"),
            "{error}"
        );
        assert!(locked.unreadable(&error), "the first error was not printed");
        assert!(!locked.unreadable(&error), "the error was printed again");
        // A task that finishes while the state is still locked meets the error
        // again, which is neither printed nor kept.
        locked.task_finished(1, &result(&[]));
        drop(held);
        assert!(locked.had_unreadable());
        assert_eq!(locked.finish(&report), USAGE_ERROR);
        assert_eq!(locked.unreadable.take(), Some(error));
        assert!(
            started.elapsed() < LOCK_PATIENCE,
            "the test waited {:?}",
            started.elapsed()
        );
    }
}
