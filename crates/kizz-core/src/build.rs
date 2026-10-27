// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Building the selected tasks in passes ("Order of execution", "Failure and
//! interruption").
//!
//! [`build`] runs the selected tasks one at a time, in passes. The tasks start in
//! the default order, which [`load`](crate::project::load) already gives them: a
//! directory's `kizz.md` before those of its subdirectories, sibling directories in
//! byte order of their names, and task blocks in file order. The learned order
//! overrides it: a task whose stored [`LearnedOrder`] names producers among the
//! selected tasks runs after them, and every other task keeps its default place
//! ([`learned_order`]).
//!
//! Every pass tries every selected task, and a failed task does not stop the pass.
//! Each task program runs again in every pass, and every `run()` goes through the
//! engine's cache check, so a task whose recorded inputs changed runs those commands
//! again, including a failed task whose looked-for paths have appeared, while a task
//! whose inputs did not change is a cache hit, and a failed one fails again from the
//! cache without running anything. A missing file is a dependency like any other,
//! never a scheduling signal: nothing waits for a path to appear.
//!
//! A pass does work when a command runs in it or a task program changes a file in
//! it through `fs.write` or `fs.write_json`; a write that leaves the file as it was
//! is no change. After a pass that did work, another pass checks every selected
//! task. The build ends when a pass does no work: it succeeds if no task failed in
//! that pass, and otherwise it fails, listing each failed task with the paths it
//! looked for and did not find. The pass limit is one more than the number of tasks
//! ([`pass_limit`]); when the last pass it allows still does work, the build stops,
//! naming the paths that keep changing and who writes them, and the files that task
//! programs changed in it. A command marked
//! `always` runs at most once per build, when a pass first reaches it, normally the
//! first one, because the engine runs each `always` step key once per build; later
//! passes reuse its result, and one first reached in a later pass, or with a new step
//! key, runs once in that pass. Every command of a task marked `cache=never` runs
//! the same way, because the engine runs it as if it were marked `always`; a task
//! with a name and no targets is never selected, because selection goes by targets.
//!
//! When a task program ends, whether it succeeded or failed, each step it left
//! unchecked, which exited with a non-zero code or was killed by a signal, had no
//! `expect`, and whose result the program never checked, by reading its `rc` or
//! `signal` or by encoding it, is in [`TaskResult::unchecked`], with its step key
//! and whether the build ran a command under that key; a result from the cache
//! counts like any other ("Exit codes and failure"). Only the unchecked steps that
//! stand when the build settles warn, the final-state rule
//! ([`BuildReport::unchecked`]): those of each task's latest run in the build,
//! which in a settled build is its run in the final pass, because every pass tries
//! every selected task. A task that runs again in a later pass replaces its
//! earlier results, so a superseded unchecked step never warns, and a build that
//! does not settle or is interrupted warns about none. With
//! [`Engine::with_fail_unchecked`] (`kizz build --fail-unchecked`), a run whose
//! program succeeded but left a step unchecked fails with
//! [`TaskFailure::Unchecked`], which names every unchecked step; as for every
//! failure, the final pass decides whether the build fails, and a program that
//! failed keeps its own failure. Without the option, an unchecked step fails
//! nothing.
//!
//! Once cancellation is requested, the build starts nothing new: no further task or
//! command starts, the outcome is [`BuildOutcome::Interrupted`], and no learned
//! order is stored.
//!
//! After every build that is not interrupted, kizz records, for each task that ran,
//! the tasks whose outputs it consumed: the last writers of the paths its steps and
//! its program read, checked, listed, or ran, including the paths written into a
//! directory it listed, and the paths other task programs wrote in this build. A
//! task program that changes a file is stored as the file's last writer when its run
//! ends ([`Engine::program_wrote`]), so the order learned from it holds in a later
//! build in which the program does not write the file.
//! Each run of a task ends with [`TaskSteps::finish`], which stores the task record:
//! the program hash and a summary of the run.
//!
//! The builder keeps the step keys under which the build ran a command in a field
//! that only its own `&mut self` methods change, so no `RefCell` borrow, which
//! would panic while the cell is borrowed, is needed ("Kizz never panics").

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs, io,
    path::PathBuf,
};

use kizz_store::{
    AccessBits, LastWriter, LearnedOrder, PathKey, PathState, Reason, StepKey, StepRecord, Store,
    StoreError, TaskId, TaskLabel, Txn, Verdict, Writer, path_prefix,
};
use kizz_trace::{AbsolutePath, AccessKind, Accesses};

use crate::{
    engine::{Engine, NoStepEvents, StepEvents, TaskSteps},
    explain::{StepDecision, TaskIdentity},
    runtime::{PrintSink, TaskFailure, TaskInfo, TaskProgram, UncheckedExit},
    step::{StepError, StepExecutor, StepOutcome, StepSpec},
};

/// The message of a step that the build did not start because it was interrupted.
pub const INTERRUPTED: &str = "the build was interrupted, so this command did not start";

/// The number of passes a build may take: one more than the number of tasks.
#[must_use]
pub const fn pass_limit(tasks: usize) -> usize {
    tasks.saturating_add(1)
}

/// The identity of the task `task`.
#[must_use]
pub fn task_id(task: &TaskInfo) -> TaskId {
    TaskId {
        file: task.file.as_path().to_path_buf(),
        index: task.index,
    }
}

/// The task `task` as explanations name it.
#[must_use]
pub fn task_label(task: &TaskInfo) -> TaskLabel {
    TaskLabel {
        task: task_id(task),
        line: u32::try_from(task.line).unwrap_or(u32::MAX),
        heading: task.heading.clone(),
    }
}

/// Follows a build as it runs, for output.
pub trait BuildEvents {
    /// Pass `pass`, counting from 1, begins.
    fn pass_started(
        &self,
        _pass: usize,
    ) {
    }

    /// The run of a task in pass `pass` ended as `result` says.
    fn task_finished(
        &self,
        _pass: usize,
        _result: &TaskResult,
    ) {
    }
}

/// Follows nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEvents;

impl BuildEvents for NoEvents {}

/// How one run of a task in a pass ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    /// The task.
    pub task: TaskInfo,
    /// What the build decided about each step of the run, in order.
    pub decisions: Vec<StepDecision>,
    /// Whether the run succeeded, or why the task failed.
    pub result: Result<(), TaskFailure>,
    /// When the task failed, the paths it looked for and did not find, sorted.
    pub missing: Vec<PathBuf>,
    /// The files the task program changed through `fs.write` and `fs.write_json`
    /// in the run, its modules' top-level code included, in the order it first
    /// changed each, without repeats.
    pub changed: Vec<AbsolutePath>,
    /// The steps the run left unchecked, in the order they ran: each exited with a
    /// non-zero code or was killed by a signal, had no `expect`, and the program
    /// never read its result's `rc` or `signal` and never encoded the result.
    pub unchecked: Vec<UncheckedStep>,
}

impl TaskResult {
    /// Whether a command of the run ran, instead of coming from the cache.
    #[must_use]
    pub fn ran(&self) -> bool {
        self.decisions
            .iter()
            .any(|decision| matches!(decision.verdict, Verdict::Ran(_)))
    }

    /// Whether the task program changed a file in the run.
    #[must_use]
    pub const fn changed_files(&self) -> bool {
        !self.changed.is_empty()
    }

    /// Whether the run did work: a command ran, or the program changed a file.
    #[must_use]
    pub fn did_work(&self) -> bool {
        self.ran() || self.changed_files()
    }

    /// Whether the task failed.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.result.is_err()
    }
}

/// A step that a task run left unchecked ("Exit codes and failure"): its task,
/// its step key, the step itself, and whether the build ran a command under the
/// key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncheckedStep {
    /// The task whose program left it unchecked.
    pub task: TaskLabel,
    /// The step key.
    pub key: StepKey,
    /// The step, with its command, how it ended, and its output.
    pub exit: UncheckedExit,
    /// Whether the build ran a command under the step key, in this run or in an
    /// earlier run of the build; when it did not, every result under the key in
    /// the build came from the cache of an earlier build.
    pub ran: bool,
}

/// One pass of a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass {
    /// The number of the pass, counting from 1.
    pub number: usize,
    /// The run of each task the pass tried, in the order they ran.
    pub tasks: Vec<TaskResult>,
}

impl Pass {
    /// Whether a command ran during the pass.
    #[must_use]
    pub fn ran(&self) -> bool {
        self.tasks.iter().any(TaskResult::ran)
    }

    /// Whether a task program changed a file during the pass.
    #[must_use]
    pub fn changed_files(&self) -> bool {
        self.tasks.iter().any(TaskResult::changed_files)
    }

    /// Whether the pass did work: a command ran, or a task program changed a file.
    #[must_use]
    pub fn did_work(&self) -> bool {
        self.ran() || self.changed_files()
    }
}

/// A task that failed in the pass that ended the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTask {
    /// The task.
    pub task: TaskInfo,
    /// Why it failed.
    pub failure: TaskFailure,
    /// The paths it looked for and did not find, sorted; any of them may be the
    /// cause.
    pub missing: Vec<PathBuf>,
}

/// An input that changed and made a step run in the last pass a build allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangingInput {
    /// The absolute path.
    pub path: PathBuf,
    /// The task whose step read it.
    pub reader: TaskLabel,
    /// The position of that step in its task's run, counting from zero.
    pub position: u32,
    /// Who last wrote it.
    pub writer: Writer,
}

/// A file that a task program changed in the last pass a build allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramWrite {
    /// The absolute path.
    pub path: PathBuf,
    /// The task whose program changed it.
    pub writer: TaskLabel,
}

/// How a build ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
    /// A pass did no work, and no task failed in it.
    Succeeded,
    /// A pass did no work, and these tasks failed in it, in the order they ran.
    Failed {
        /// The failed tasks.
        failures: Vec<FailedTask>,
    },
    /// The last pass the limit allows still did work.
    NotSettled {
        /// The pass limit.
        limit: usize,
        /// The changed inputs that made steps run in that pass, sorted by path.
        changing: Vec<ChangingInput>,
        /// The files that task programs changed in that pass, each with its task,
        /// sorted by path, then by task.
        written: Vec<ProgramWrite>,
        /// The tasks that did work in that pass, in the order they ran.
        worked: Vec<TaskInfo>,
    },
    /// Cancellation was requested, so the build started nothing new.
    Interrupted,
}

/// What a build did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    /// The selected tasks, in the order every pass tried them.
    pub order: Vec<TaskId>,
    /// Every pass, in order, the one that ran nothing included.
    pub passes: Vec<Pass>,
    /// How the build ended.
    pub outcome: BuildOutcome,
}

impl BuildReport {
    /// The number of passes the build took to settle: the last pass that did work,
    /// by running a command or by a task program changing a file, or 1 when no pass
    /// did. The final pass, which did no work, only confirms that nothing changed,
    /// so it adds to the count only when it is the only pass. A build that did not
    /// settle or was interrupted reports every pass it tried.
    #[must_use]
    pub fn settled_in(&self) -> usize {
        match self.outcome {
            BuildOutcome::Succeeded | BuildOutcome::Failed { .. } => {
                self.passes
                    .iter()
                    .rev()
                    .find(|pass| pass.did_work())
                    .map_or(1, |pass| pass.number)
            },
            BuildOutcome::NotSettled { .. } | BuildOutcome::Interrupted => self.passes.len(),
        }
    }

    /// Whether the build succeeded.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self.outcome, BuildOutcome::Succeeded)
    }

    /// The unchecked steps that stand when the build settles, which warn (the
    /// final-state rule, "Exit codes and failure"): those of each task's latest run
    /// in the build. Every pass tries every selected task, so in a settled build
    /// these are the runs of the final pass, in the order it ran the tasks and each
    /// task its steps, each step once. A task that ran again in a later pass
    /// replaced its earlier results, so an unchecked step of an earlier pass stands
    /// only when the final pass still has it. A build that did not settle or was
    /// interrupted has no settled state, so it has none.
    #[must_use]
    pub fn unchecked(&self) -> Vec<UncheckedStep> {
        match self.outcome {
            BuildOutcome::Succeeded | BuildOutcome::Failed { .. } => {
                self.passes
                    .last()
                    .map(|pass| {
                        pass.tasks
                            .iter()
                            .flat_map(|result| result.unchecked.iter().cloned())
                            .collect()
                    })
                    .unwrap_or_default()
            },
            BuildOutcome::NotSettled { .. } | BuildOutcome::Interrupted => Vec::new(),
        }
    }
}

/// What one run of a task touched, for the learned order.
#[derive(Debug, Clone, Default)]
struct TaskTrace {
    /// The key of each step of the run.
    keys: Vec<StepKey>,
    /// Every file access the program made: module loads and the `fs` and `env`
    /// functions.
    accesses: Accesses,
}

/// The step executor that starts nothing once the build is interrupted.
struct CancelGuard<'s> {
    /// The steps of the task run.
    steps: &'s TaskSteps<'s>,
    /// The engine, which knows whether cancellation was requested.
    engine: &'s Engine<'s>,
}

impl StepExecutor for CancelGuard<'_> {
    fn execute(
        &self,
        step: &StepSpec,
    ) -> Result<StepOutcome, StepError> {
        if self.engine.is_cancelled() {
            return Err(StepError::new(INTERRUPTED));
        }
        self.steps.execute(step)
    }
}

/// What every pass of one build shares.
struct Builder<'b> {
    /// The engine of the build.
    engine: &'b Engine<'b>,
    /// The caller environment, which `env.caller` and `env.inherit` read.
    caller: &'b BTreeMap<String, String>,
    /// Where the lines that task programs print go.
    print: &'b dyn PrintSink,
    /// Follows each step as it starts or comes from the cache: nothing in a build,
    /// the live output of `kizz run`.
    step_events: &'b dyn StepEvents,
    /// The step keys under which the build ran a command so far, which only the
    /// builder's own `&mut self` methods change, so it needs no `RefCell`.
    ran: BTreeSet<StepKey>,
}

impl Builder<'_> {
    /// Run the task `task` once: load its program, run it, and finish the run,
    /// storing its program as the last writer of every file it changed.
    ///
    /// Each step the run left unchecked is in its result, with its step key and
    /// whether the build ran a command under that key so far; when the engine
    /// fails unchecked steps and the program succeeded, the run fails with every
    /// unchecked step instead, before its task record is stored.
    fn run_task(
        &mut self,
        task: &TaskInfo,
    ) -> Result<(TaskResult, TaskTrace), StoreError> {
        let load = TaskProgram::load(task.clone(), self.print);
        let mut accesses = load.accesses;
        let mut changes = load.changes;
        let (result, decisions, keys, exits) = match load.program {
            Err(failure) => (Err(failure), Vec::new(), Vec::new(), Vec::new()),
            Ok(program) => {
                let steps = self
                    .engine
                    .task_for(TaskIdentity::of(&program))
                    .with_cache_never(task.cache_never)
                    .with_events(self.step_events);
                let executor = CancelGuard {
                    steps: &steps,
                    engine: self.engine,
                };
                let run = program.run(self.caller, &executor, self.print);
                accesses.merge(run.accesses);
                changes.merge(run.changes);
                let result = match run.result {
                    Ok(()) if self.engine.fails_unchecked() && !run.unchecked.is_empty() => {
                        Err(TaskFailure::Unchecked {
                            exits: run.unchecked.clone(),
                            invoked: self.engine.command_name(),
                        })
                    },
                    other => other,
                };
                steps.finish(result.is_ok())?;
                (result, steps.decisions(), steps.keys(), run.unchecked)
            },
        };
        let position_key = |position: u32| {
            usize::try_from(position)
                .ok()
                .and_then(|position| keys.get(position))
                .copied()
        };
        let ran = &mut self.ran;
        for decision in &decisions {
            if matches!(decision.verdict, Verdict::Ran(_))
                && let Some(key) = position_key(decision.position)
            {
                ran.insert(key);
            }
        }
        let unchecked: Vec<UncheckedStep> = exits
            .into_iter()
            .filter_map(|exit| {
                let key = position_key(exit.position)?;
                Some(UncheckedStep {
                    task: task_label(task),
                    key,
                    ran: ran.contains(&key),
                    exit,
                })
            })
            .collect();
        self.engine.program_wrote(&task_id(task), &changes)?;
        let missing = if result.is_err() {
            missing_paths(self.engine.store(), &keys, &accesses)?
        } else {
            Vec::new()
        };
        Ok((
            TaskResult {
                task: task.clone(),
                decisions,
                result,
                missing,
                changed: changes.paths(),
                unchecked,
            },
            TaskTrace { keys, accesses },
        ))
    }

    /// Run one pass, numbered `number`, over `tasks`, keeping what each run touched
    /// in `traces`. Returns the pass and whether the build was interrupted.
    fn pass(
        &mut self,
        number: usize,
        tasks: &[TaskInfo],
        traces: &mut BTreeMap<TaskId, TaskTrace>,
        events: &dyn BuildEvents,
    ) -> Result<(Pass, bool), StoreError> {
        events.pass_started(number);
        let mut results = Vec::new();
        for task in tasks {
            if self.engine.is_cancelled() {
                break;
            }
            let (result, trace) = self.run_task(task)?;
            traces.insert(task_id(task), trace);
            events.task_finished(number, &result);
            results.push(result);
        }
        let pass = Pass {
            number,
            tasks: results,
        };
        Ok((pass, self.engine.is_cancelled()))
    }
}

/// Build `tasks`, the selected tasks in the default order, with `engine`, in
/// passes until a pass does no work; `caller` is the caller environment, the lines
/// task programs print go to `print`, and `events` follows the build.
///
/// # Errors
///
/// A [`StoreError`] when a record cannot be read or written.
pub fn build(
    engine: &Engine<'_>,
    tasks: &[TaskInfo],
    caller: &BTreeMap<String, String>,
    print: &dyn PrintSink,
    events: &dyn BuildEvents,
) -> Result<BuildReport, StoreError> {
    let store = engine.store();
    let ordered: Vec<TaskInfo> = stored_order(store, tasks)?
        .into_iter()
        .filter_map(|index| tasks.get(index).cloned())
        .collect();
    let mut builder = Builder {
        engine,
        caller,
        print,
        step_events: &NoStepEvents,
        ran: BTreeSet::new(),
    };
    let limit = pass_limit(ordered.len());
    let mut passes: Vec<Pass> = Vec::new();
    let mut traces = BTreeMap::new();
    let outcome = loop {
        let number = passes.len().saturating_add(1);
        let (pass, interrupted) = builder.pass(number, &ordered, &mut traces, events)?;
        let outcome = if interrupted {
            Some(BuildOutcome::Interrupted)
        } else if !pass.did_work() {
            Some(settled(&pass))
        } else if number >= limit {
            Some(not_settled(&pass, limit))
        } else {
            None
        };
        passes.push(pass);
        if let Some(outcome) = outcome {
            break outcome;
        }
    };
    if outcome != BuildOutcome::Interrupted {
        learn(store, &traces)?;
    }
    Ok(BuildReport {
        order: ordered.iter().map(task_id).collect(),
        passes,
        outcome,
    })
}

/// Run `task` once with `engine`, as `kizz run` does: no other task and no further
/// pass, even when its program changes a file; `caller` is the caller environment,
/// the lines its program prints go to `print`, and `step_events` follows each step
/// as it starts or comes from the cache.
///
/// The run stores what a build of that one task in one pass stores: the step
/// records and positions, the task record, the task program as the last writer of
/// each file it changed, and, unless the run was interrupted, the task's learned
/// order.
///
/// # Errors
///
/// A [`StoreError`] when a record cannot be read or written.
pub(crate) fn run_task_once(
    engine: &Engine<'_>,
    task: &TaskInfo,
    caller: &BTreeMap<String, String>,
    print: &dyn PrintSink,
    step_events: &dyn StepEvents,
) -> Result<TaskResult, StoreError> {
    let mut builder = Builder {
        engine,
        caller,
        print,
        step_events,
        ran: BTreeSet::new(),
    };
    let (result, trace) = builder.run_task(task)?;
    if !engine.is_cancelled() {
        learn(engine.store(), &BTreeMap::from([(task_id(task), trace)]))?;
    }
    Ok(result)
}

/// The outcome of a build whose pass `pass` did no work.
fn settled(pass: &Pass) -> BuildOutcome {
    let failures: Vec<FailedTask> = pass
        .tasks
        .iter()
        .filter_map(|result| {
            result.result.as_ref().err().map(|failure| {
                FailedTask {
                    task: result.task.clone(),
                    failure: failure.clone(),
                    missing: result.missing.clone(),
                }
            })
        })
        .collect();
    if failures.is_empty() {
        BuildOutcome::Succeeded
    } else {
        BuildOutcome::Failed { failures }
    }
}

/// The outcome of a build whose pass `pass`, the last one `limit` allows, still did
/// work.
fn not_settled(
    pass: &Pass,
    limit: usize,
) -> BuildOutcome {
    let mut changing = Vec::new();
    let mut written = Vec::new();
    for result in &pass.tasks {
        let reader = task_label(&result.task);
        for decision in &result.decisions {
            let Verdict::Ran(explanation) = &decision.verdict else {
                continue;
            };
            for input in explanation.reasons.iter().flat_map(Reason::inputs) {
                changing.push(ChangingInput {
                    path: input.path.clone(),
                    reader: reader.clone(),
                    position: decision.position,
                    writer: input.writer.clone(),
                });
            }
        }
        for path in &result.changed {
            written.push(ProgramWrite {
                path: path.as_path().to_path_buf(),
                writer: reader.clone(),
            });
        }
    }
    changing.sort_by(|a, b| {
        (&a.path, &a.reader.task, a.position).cmp(&(&b.path, &b.reader.task, b.position))
    });
    written.sort_by(|a, b| (&a.path, &a.writer.task).cmp(&(&b.path, &b.writer.task)));
    written.dedup();
    BuildOutcome::NotSettled {
        limit,
        changing,
        written,
        worked: pass
            .tasks
            .iter()
            .filter(|result| result.did_work())
            .map(|result| result.task.clone())
            .collect(),
    }
}

/// Whether the error `error` says that nothing exists at a path.
fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// The paths a failed task run looked for and did not find: the paths its steps
/// found absent, from the records of the step keys `keys`, and the paths its program
/// checked through `accesses` that do not exist; never a path the task only wrote.
///
/// # Errors
///
/// A [`StoreError`] when a step record cannot be read.
fn missing_paths(
    store: &Store,
    keys: &[StepKey],
    accesses: &Accesses,
) -> Result<Vec<PathBuf>, StoreError> {
    let mut missing = BTreeSet::new();
    let txn = store.read()?;
    let keys: BTreeSet<&StepKey> = keys.iter().collect();
    for key in keys {
        let Some(record) = txn.get::<StepRecord>(key)? else {
            continue;
        };
        for observation in record.observations {
            if observation.state == PathState::Absent
                && !observation.access.contains(AccessBits::WRITE)
            {
                missing.insert(observation.path);
            }
        }
    }
    for (path, kinds) in accesses {
        if !kinds.contains(AccessKind::Write)
            && fs::symlink_metadata(path.as_path()).is_err_and(|error| is_missing(&error))
        {
            missing.insert(path.as_path().to_path_buf());
        }
    }
    Ok(missing.into_iter().collect())
}

/// The order of tasks whose producers, by index, are `producers`: `producers[i]`
/// lists the tasks that must run before task `i`, and the indices are the default
/// order. Each task, in the default order, runs once its producers have run, which
/// are placed first, each in the same way and in their default order; every task
/// without such an edge keeps its default place. A cycle of producers is broken
/// where it closes: a producer already being placed is not waited for. Unknown
/// indices and a task naming itself are ignored.
#[must_use]
pub fn learned_order(producers: &[Vec<usize>]) -> Vec<usize> {
    /// Place `task` after its producers, unless it is placed or being placed.
    fn place(
        task: usize,
        producers: &[Vec<usize>],
        visiting: &mut BTreeSet<usize>,
        order: &mut Vec<usize>,
    ) {
        if !visiting.insert(task) {
            return;
        }
        let before: BTreeSet<usize> = producers
            .get(task)
            .map(|before| before.iter().copied().collect())
            .unwrap_or_default();
        for producer in before {
            if producer < producers.len() {
                place(producer, producers, visiting, order);
            }
        }
        order.push(task);
    }

    let mut visiting = BTreeSet::new();
    let mut order = Vec::with_capacity(producers.len());
    for task in 0..producers.len() {
        place(task, producers, &mut visiting, &mut order);
    }
    order
}

/// The order of `tasks`, given in the default order, as indices: the learned order
/// stored for them, which runs each task's selected producers first and keeps the
/// default order elsewhere.
///
/// # Errors
///
/// A [`StoreError`] when a learned-order record cannot be read.
fn stored_order(
    store: &Store,
    tasks: &[TaskInfo],
) -> Result<Vec<usize>, StoreError> {
    let ids: Vec<TaskId> = tasks.iter().map(task_id).collect();
    let index: HashMap<&TaskId, usize> = ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
    let txn = store.read()?;
    let mut producers = Vec::with_capacity(ids.len());
    for id in &ids {
        let before: Vec<usize> = txn
            .get::<LearnedOrder>(id)?
            .map(|order| {
                order
                    .producers
                    .iter()
                    .filter_map(|producer| index.get(producer).copied())
                    .collect()
            })
            .unwrap_or_default();
        producers.push(before);
    }
    Ok(learned_order(&producers))
}

/// The paths a task run consumed, with whether it listed each one: every path its
/// steps' records or its program accesses show it read, checked, listed, or ran.
///
/// # Errors
///
/// A [`StoreError`] when a step record cannot be read.
fn consumed(
    txn: &Txn<'_>,
    trace: &TaskTrace,
) -> Result<BTreeMap<PathBuf, bool>, StoreError> {
    let mut paths: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let mut add = |path: PathBuf, listed: bool| {
        let entry = paths.entry(path).or_insert(false);
        *entry = *entry || listed;
    };
    let keys: BTreeSet<&StepKey> = trace.keys.iter().collect();
    for key in keys {
        let Some(record) = txn.get::<StepRecord>(key)? else {
            continue;
        };
        for observation in record.observations {
            if observation.access.0 & !AccessBits::WRITE != 0 {
                let listed = observation.access.contains(AccessBits::LIST);
                add(observation.path, listed);
            }
        }
    }
    for (path, kinds) in &trace.accesses {
        if kinds.iter().any(|kind| kind != AccessKind::Write) {
            add(
                path.as_path().to_path_buf(),
                kinds.contains(AccessKind::List),
            );
        }
    }
    Ok(paths)
}

/// Store the learned order of every task in `traces`: the tasks whose outputs its
/// last run consumed, which are the last writers of the paths it consumed, the
/// last writers of the paths written into a directory it listed, and the tasks whose
/// programs wrote either in this build; never the task itself.
///
/// # Errors
///
/// A [`StoreError`] when a record cannot be read or written.
fn learn(
    store: &Store,
    traces: &BTreeMap<TaskId, TaskTrace>,
) -> Result<(), StoreError> {
    let mut program_writes: BTreeMap<PathBuf, BTreeSet<TaskId>> = BTreeMap::new();
    for (task, trace) in traces {
        for (path, kinds) in &trace.accesses {
            if kinds.contains(AccessKind::Write) {
                program_writes
                    .entry(path.as_path().to_path_buf())
                    .or_default()
                    .insert(task.clone());
            }
        }
    }
    let mut learned: Vec<(TaskId, BTreeSet<TaskId>)> = Vec::new();
    {
        let txn = store.read()?;
        for (task, trace) in traces {
            let mut producers = BTreeSet::new();
            for (path, listed) in consumed(&txn, trace)? {
                if let Some(writer) = txn.get::<LastWriter>(&PathKey(path.clone()))? {
                    producers.insert(writer.task);
                }
                if let Some(writers) = program_writes.get(&path) {
                    producers.extend(writers.iter().cloned());
                }
                if listed {
                    for (key, writer) in txn.scan::<LastWriter>(&path_prefix(&path))? {
                        if key.0.parent() == Some(path.as_path()) {
                            producers.insert(writer.task);
                        }
                    }
                    for (written, writers) in &program_writes {
                        if written.parent() == Some(path.as_path()) {
                            producers.extend(writers.iter().cloned());
                        }
                    }
                }
            }
            producers.remove(task);
            learned.push((task.clone(), producers));
        }
    }
    let mut txn = store.write()?;
    for (task, producers) in learned {
        if producers.is_empty() {
            txn.delete::<LearnedOrder>(&task)?;
        } else {
            txn.put(&task, &LearnedOrder { producers })?;
        }
    }
    txn.commit()
}

/// Unit tests for the order of tasks and the pass limit.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use super::{learned_order, pass_limit};

    /// Without producers, the tasks keep the default order.
    #[test]
    fn without_producers_the_default_order_holds() {
        assert_eq!(learned_order(&[vec![], vec![], vec![]]), vec![0, 1, 2]);
        assert_eq!(learned_order(&[]), Vec::<usize>::new());
    }

    /// Producers run before their consumers, and every other task keeps its
    /// default place.
    #[test]
    fn producers_run_first_and_the_default_order_holds_elsewhere() {
        // Task 0 consumes task 2's outputs; task 1 has no producers.
        assert_eq!(learned_order(&[vec![2], vec![], vec![]]), vec![2, 0, 1]);
        // A chain: 0 consumes 1, which consumes 3; 2 is free.
        assert_eq!(learned_order(&[vec![1], vec![3], vec![], vec![]]), vec![
            3, 1, 0, 2
        ]);
        // Several producers, all before the consumer, in their default order.
        assert_eq!(learned_order(&[vec![3, 2], vec![], vec![], vec![]]), vec![
            2, 3, 0, 1
        ]);
        // A producer that already ran keeps its place.
        assert_eq!(learned_order(&[vec![], vec![0], vec![]]), vec![0, 1, 2]);
    }

    /// Unknown producers and a task naming itself are ignored, and a cycle is
    /// broken where it closes.
    #[test]
    fn cycles_and_unknown_producers_are_broken_or_ignored() {
        assert_eq!(learned_order(&[vec![0, 9], vec![]]), vec![0, 1]);
        assert_eq!(learned_order(&[vec![1], vec![0]]), vec![1, 0]);
        assert_eq!(learned_order(&[vec![2], vec![0], vec![1], vec![]]), vec![
            1, 2, 0, 3
        ]);
    }

    /// The pass limit is one more than the number of tasks.
    #[test]
    fn the_pass_limit_is_one_more_than_the_number_of_tasks() {
        assert_eq!(pass_limit(0), 1);
        assert_eq!(pass_limit(2), 3);
        assert_eq!(pass_limit(usize::MAX), usize::MAX);
    }
}
