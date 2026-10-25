// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The engine side of the step executor.
//!
//! An [`Engine`] holds what every task of one build shares: the tracer, the store,
//! where live output goes, the cancellation flag, the caller's home directory, the
//! in-memory cache of file states and hashes, the results of the `always` steps
//! that already ran, and the results that were not cached. For each run of a task
//! program, [`Engine::task`] makes a [`TaskSteps`], the [`StepExecutor`] that the
//! runtime hands each `run()` call to. For each step it:
//!
//! 1. derives the step key, counting identical commands earlier in the run;
//! 2. decides whether the step runs, with the cache check;
//! 3. on a cache hit, returns the recorded exit status and raw output, with `cached` set,
//!    storing new metadata the check found;
//! 4. otherwise looks the program up through the `PATH` of the step's environment,
//!    recording every location checked, and runs the command through the tracer;
//! 5. drops the cached hash of every path the step wrote, and records the state of every
//!    touched path, concurrently, applying the step's ignore and `track` patterns; and
//! 6. stores the step record under the step key; a result that is not cached replaces any
//!    older record with a record marked as not cached.
//!
//! The cache check ("Cache check"): with no record for the key, the step runs, and
//! the reason is that it never ran. With a record, every recorded observation is
//! compared with the file system by [`check`]; the step is a
//! hit only when nothing changed, and otherwise it runs, with every difference as
//! the reason.
//!
//! A step marked `always` runs at most once per build, when a pass first reaches it:
//! the first time its key comes up in the build, normally in the first pass, and it
//! is recorded like any other step. Every later time in the same build, whatever
//! changed, its first result is returned again without running ("Commands that
//! always run"). A key first reached in a later pass runs once then.
//!
//! Results that are not cached ("Results that are not cached"): a command killed by
//! a signal, every run that returns once cancellation has been requested, whether a
//! signal ended it or it exited with a code after it handled the interrupt, a run
//! that the tracer reports as incompletely traced, and a command that could not
//! start for a reason other than a missing or non-executable program. Their record
//! is marked with why, which makes it a record that is never a cache hit and
//! replaces any older record of the step key, so no older result is replayed. The
//! engine also keeps the paths such a command touched, with their states, in memory
//! for the rest of the build: when the step comes up again, it runs again only if
//! one of them changed, and otherwise its result is returned again without running.
//! In a later build, the marked record makes the step run again, with that stated
//! as the reason.
//!
//! A missing or non-executable program fails the task and is recorded like any
//! other failure, with every path checked and the failure message, which a cache hit
//! fails with again. A bare program name without `PATH` in the step's environment,
//! a malformed pattern, and a tracer failure fail the task without a record.
//!
//! Explanations ("Explaining decisions"): before a step runs, the engine turns the
//! cache check's reason into an [`Explanation`] (see [`explain`](crate::explain)), and
//! each step's [`StepDecision`] is kept for the line `kizz build` prints. For a task
//! run made with [`Engine::task_for`], which knows its [`TaskIdentity`], the engine
//! also stores, with each step record it writes, a last-writer record for every path
//! the step wrote and the step-position record with the explanation; a cached step's
//! position is stored when [`TaskSteps::finish`] stores the task record, which also
//! removes the positions the run no longer reached. [`Engine::program_wrote`] stores
//! a task program as the last writer of each file it changed through `fs.write` or
//! `fs.write_json`. A step that re-runs in consecutive builds for the same
//! self-inflicted reason adds a [`RepeatedWarning`].
//!
//! Owners ("Cleaning up the cache"): every step record carries the tasks that own
//! it, by [`TaskId`]. A run made with [`Engine::task_for`] adds
//! its task to the owners of the record it stores, in the transaction that stores
//! it, and keeps the owners of the record it replaces under the same key, a record
//! marked as not cached included; a run without a task identity
//! adds no owner and keeps the others. A step noted as cached, whether a hit on a
//! stored record or a result reused within the build, makes its task an owner of
//! the record under its key in the transaction of [`TaskSteps::finish`] that stores
//! its position; a hit that refreshes a record's observations keeps its owners. So
//! after a build, every task that stores a step position for a key owns the record
//! under that key. A path reset and `kizz cache clean` use the owners (see
//! [`cleanup`](crate::cleanup)).

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    fmt,
    path::PathBuf,
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use kizz_store::{
    AccessBits, BuildId, Capture, ContentHash, Explanation, LastWriter, NotCachedReason,
    Observation, PathKey, PositionKey, ProgramPath, Reason, RecordedOutput, RunReason, StepExit,
    StepKey, StepPosition, StepRecord, StepSpecRecord, Store, StoreError, TaskId, TaskRecord, Txn,
    Verdict, WrittenBy,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    RunRequest, RunResult, Termination, TraceError, Tracer,
};

use crate::{
    check::{Checked, check},
    explain::{
        Baseline, InputContext, RepeatedWarning, StepDecision, TaskIdentity, carried_from,
        changed_inputs, spec_reasons,
    },
    files::FileChanges,
    key::Occurrences,
    lookup::{ProgramLookup, find_program},
    observe::observe,
    patterns::PathPatterns,
    runtime::ProgramHash,
    states::FileStates,
    step::{StepError, StepExecutor, StepOutcome, StepSpec},
};

/// The identity of the most recent build this process started, so every build
/// gets a larger one than the last.
static LAST_BUILD: AtomicU64 = AtomicU64::new(0);

/// A new build identity: now, in nanoseconds since the Unix epoch, or one more
/// than the last build of this process when that is larger.
fn next_build() -> BuildId {
    let now = now_nanos();
    let mut last = LAST_BUILD.load(Ordering::SeqCst);
    loop {
        let id = now.max(last.saturating_add(1));
        match LAST_BUILD.compare_exchange_weak(last, id, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return BuildId(id),
            Err(current) => last = current,
        }
    }
}

/// A result that was not cached, kept in memory for the rest of the build.
#[derive(Debug, Clone)]
struct NotCachedRun {
    /// Why it was not cached.
    why: NotCachedReason,
    /// Every touched path that no ignore pattern matched, with its state.
    observations: Vec<Observation>,
    /// When the engine began recording the states, in nanoseconds since the Unix
    /// epoch.
    recorded_at_nanos: u64,
    /// What the step returned.
    result: Result<StepOutcome, StepError>,
}

/// What every task of a build shares to run, check, and record its steps.
pub struct Engine<'a> {
    /// Runs and traces the commands.
    tracer: &'a dyn Tracer,
    /// Holds the step records.
    store: &'a Store,
    /// Where live output goes for steps with `stream = True`.
    echo: &'a dyn EchoSink,
    /// Stops a running command when set.
    cancel: Cancellation,
    /// The caller's home directory, which a leading `~/` in a pattern means.
    home: Option<AbsolutePath>,
    /// The file states and hashes of the build.
    states: FileStates,
    /// The `always` steps that already ran in the build, by step key, with their
    /// result, which is `None` until the run ends.
    always_ran: Mutex<HashMap<StepKey, Option<Result<StepOutcome, StepError>>>>,
    /// The results of the build that were not cached, by step key.
    not_cached: Mutex<HashMap<StepKey, NotCachedRun>>,
    /// The identity of the build.
    build: BuildId,
    /// The warnings for steps that re-ran in consecutive builds for the same
    /// self-inflicted reason, in the order they ran.
    warnings: Mutex<Vec<RepeatedWarning>>,
}

impl fmt::Debug for Engine<'_> {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.debug_struct("Engine")
            .field("store", &self.store)
            .field("home", &self.home)
            .field("states", &self.states)
            .field("always_ran", &self.always_ran)
            .field("not_cached", &self.not_cached)
            .field("build", &self.build)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

/// Lock `mutex`, whether or not a thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<'a> Engine<'a> {
    /// An engine for one build that runs commands with `tracer`, echoes live output
    /// to `echo`, stops commands when `cancel` is set, stores records in `store`,
    /// and expands `~/` in patterns to `home`, the caller's home directory.
    #[must_use]
    pub fn new(
        tracer: &'a dyn Tracer,
        store: &'a Store,
        echo: &'a dyn EchoSink,
        cancel: Cancellation,
        home: Option<AbsolutePath>,
    ) -> Self {
        Self {
            tracer,
            store,
            echo,
            cancel,
            home,
            states: FileStates::new(),
            always_ran: Mutex::new(HashMap::new()),
            not_cached: Mutex::new(HashMap::new()),
            build: next_build(),
            warnings: Mutex::new(Vec::new()),
        }
    }

    /// The identity of the build.
    #[must_use]
    pub const fn build(&self) -> BuildId {
        self.build
    }

    /// Whether cancellation was requested, so the build starts nothing new.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The store that holds the build's records.
    #[must_use]
    pub(crate) const fn store(&self) -> &'a Store {
        self.store
    }

    /// The warnings for steps that re-ran in consecutive builds for the same
    /// self-inflicted reason, in the order they ran, one per step.
    #[must_use]
    pub fn warnings(&self) -> Vec<RepeatedWarning> {
        lock(&self.warnings).clone()
    }

    /// Keep `warning`, unless the same step already has one in this build.
    fn warn(
        &self,
        warning: RepeatedWarning,
    ) {
        let mut warnings = lock(&self.warnings);
        let known = warnings.iter().any(|known| {
            known.task.as_ref().map(|task| &task.task)
                == warning.task.as_ref().map(|task| &task.task)
                && known.position == warning.position
                && known.command == warning.command
        });
        if !known {
            warnings.push(warning);
        }
    }

    /// The number of files whose content this engine has hashed, to record steps
    /// and to check the cache.
    #[must_use]
    pub fn files_hashed(&self) -> u64 {
        self.states.files_hashed()
    }

    /// The file states and hashes of the build.
    #[must_use]
    pub const fn file_states(&self) -> &FileStates {
        &self.states
    }

    /// Store the program of the task `task` as the last writer of each file in
    /// `changes`, the files it changed through `fs.write` and `fs.write_json` in this
    /// build, with the time of each change, and drop their cached hashes. A file
    /// whose last writer wrote it later, such as a step that the program ran after
    /// its write, keeps that writer.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] when a record cannot be read or written.
    pub fn program_wrote(
        &self,
        task: &TaskId,
        changes: &FileChanges,
    ) -> Result<(), StoreError> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut written = Accesses::new();
        for file in changes.files() {
            written.record(file.path.clone(), AccessKind::Write);
        }
        self.states.forget_written(&written);
        let mut txn = self.store.write()?;
        for file in changes.files() {
            let key = PathKey(file.path.as_path().to_path_buf());
            if txn
                .get::<LastWriter>(&key)?
                .is_some_and(|entry| entry.written_at_nanos > file.at_nanos)
            {
                continue;
            }
            txn.put(&key, &LastWriter {
                task: task.clone(),
                by: WrittenBy::Program,
                build: self.build,
                written_at_nanos: file.at_nanos,
            })?;
        }
        txn.commit()
    }

    /// The steps of the build whose last result was not cached, with why, sorted by
    /// step key.
    #[must_use]
    pub fn not_cached(&self) -> Vec<(StepKey, NotCachedReason)> {
        let mut steps: Vec<_> = lock(&self.not_cached)
            .iter()
            .map(|(key, run)| (*key, run.why.clone()))
            .collect();
        steps.sort_by_key(|(key, _)| *key);
        steps
    }

    /// The step executor of one run of a task program whose hash is `program` and
    /// whose directory is `task_dir`, without a task identity: its steps are checked,
    /// run, recorded, and explained, but no step position, last writer, or task
    /// record is stored for them, and no task becomes an owner of their records,
    /// whose other owners are kept. Builds use [`Engine::task_for`].
    #[must_use]
    pub fn task(
        &self,
        program: ProgramHash,
        task_dir: AbsolutePath,
    ) -> TaskSteps<'_> {
        TaskSteps {
            engine: self,
            program,
            task_dir,
            identity: None,
            occurrences: RefCell::new(Occurrences::new()),
            keys: RefCell::new(Vec::new()),
            decisions: RefCell::new(Vec::new()),
            cached_positions: RefCell::new(Vec::new()),
        }
    }

    /// The step executor of one run of the task `identity`, which stores the step
    /// positions and last writers of its steps, makes the task an owner of the
    /// record of every step it runs or finds cached, and stores its task record when
    /// the run ends with [`TaskSteps::finish`].
    #[must_use]
    pub fn task_for(
        &self,
        identity: TaskIdentity,
    ) -> TaskSteps<'_> {
        TaskSteps {
            engine: self,
            program: identity.program,
            task_dir: identity.dir.clone(),
            identity: Some(identity),
            occurrences: RefCell::new(Occurrences::new()),
            keys: RefCell::new(Vec::new()),
            decisions: RefCell::new(Vec::new()),
            cached_positions: RefCell::new(Vec::new()),
        }
    }

    /// The result of the `always` step `key` when it already ran in this build, as
    /// a result that did not run; otherwise mark it as running and return `None`.
    fn first_always_result(
        &self,
        key: StepKey,
    ) -> Option<Result<StepOutcome, StepError>> {
        let mut ran = lock(&self.always_ran);
        match ran.get(&key) {
            Some(Some(result)) => Some(result.clone()),
            Some(None) => {
                Some(Err(StepError::new(
                    "an `always` step came up again while its one run in the build had not ended",
                )))
            },
            None => {
                ran.insert(key, None);
                None
            },
        }
    }

    /// Keep `result`, the result of the one run of the `always` step `key` in this
    /// build, to return every later time the step comes up in the build.
    fn finish_always(
        &self,
        key: StepKey,
        result: &Result<StepOutcome, StepError>,
    ) {
        let result = result.clone().map(|outcome| {
            StepOutcome {
                cached: true,
                ..outcome
            }
        });
        lock(&self.always_ran).insert(key, Some(result));
    }
}

/// The step executor of one run of a task program.
#[derive(Debug)]
pub struct TaskSteps<'a> {
    /// The engine.
    engine: &'a Engine<'a>,
    /// The hash of the task program.
    program: ProgramHash,
    /// The task directory, at which relative patterns are anchored.
    task_dir: AbsolutePath,
    /// The task's identity, when the run stores step positions, last writers, and a
    /// task record.
    identity: Option<TaskIdentity>,
    /// Counts identical commands in the run.
    occurrences: RefCell<Occurrences>,
    /// The key of each step of the run, in order.
    keys: RefCell<Vec<StepKey>>,
    /// What the build decided about each step of the run, in order.
    decisions: RefCell<Vec<StepDecision>>,
    /// The step positions of the cached steps, stored by [`TaskSteps::finish`],
    /// which also makes the task an owner of the record each of them names.
    cached_positions: RefCell<Vec<(PositionKey, StepPosition)>>,
}

/// What is known about a step before it runs.
struct Pending {
    /// The step key.
    key: StepKey,
    /// The number of identical commands earlier in the run.
    occurrence: u32,
    /// Why the step runs.
    reason: RunReason,
    /// The step's ignore patterns.
    ignore: PathPatterns,
    /// The step's `track` patterns.
    track: PathPatterns,
    /// The position of the step in the task's run.
    position: u32,
    /// Why the step runs, explained.
    explanation: Explanation,
    /// The self-inflicted inputs that made the step run in its task's previous
    /// build, for its step position.
    carried: Vec<PathBuf>,
}

/// Whether a step runs, as the cache check decided.
enum Decision {
    /// The step runs, for this reason; with the recorded state the cache check
    /// compared, when it compared one.
    Run(RunReason, Option<Baseline>),
    /// The record is a cache hit.
    Hit {
        /// The record.
        record: Box<StepRecord>,
        /// The comparison of its observations with the file system.
        checked: Checked,
        /// When the check began, in nanoseconds since the Unix epoch.
        checked_at_nanos: u64,
    },
    /// A result of this build that was not cached is returned again, because
    /// nothing it touched changed.
    Replay(Result<StepOutcome, StepError>),
}

/// How a step ended, before its record is stored.
struct Finished {
    /// The program that ran, if one was found.
    program: Option<AbsolutePath>,
    /// Every path touched, the locations the lookup checked included.
    accesses: Accesses,
    /// How the command ended.
    exit: StepExit,
    /// The raw captured output.
    output: CapturedOutput,
    /// How long the command ran.
    duration: Duration,
    /// When the command started.
    ran_at: SystemTime,
    /// The failure message of a missing or non-executable program, or of a command
    /// that could not start.
    failure: Option<String>,
}

/// A step error from a store error.
fn store_error(error: &StoreError) -> StepError {
    StepError::new(format_args!("cannot record the step in the cache: {error}"))
}

/// The paths in `checked`, as a list for messages.
fn checked_list(checked: &Accesses) -> String {
    checked
        .iter()
        .map(|(path, _)| path.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The message of a step whose program `name` the lookup did not find.
fn missing_program_message(
    name: &str,
    lookup: &ProgramLookup,
) -> String {
    let checked = checked_list(lookup.checked());
    match lookup {
        ProgramLookup::NotExecutable { candidate, .. } => {
            format!("program `{name}` is not executable: {candidate}; checked {checked}")
        },
        ProgramLookup::NotFound { .. } | ProgramLookup::Found { .. } => {
            format!(
                "program `{name}` was not found in the task environment's PATH; checked \
                 {checked}"
            )
        },
    }
}

/// The capture mode as the store records it.
const fn capture(mode: CaptureMode) -> Capture {
    match mode {
        CaptureMode::Split => Capture::Split,
        CaptureMode::Merged => Capture::Merged,
    }
}

/// Captured output as the store records it.
fn recorded_output(output: CapturedOutput) -> RecordedOutput {
    match output {
        CapturedOutput::Split { stdout, stderr } => RecordedOutput::Split { stdout, stderr },
        CapturedOutput::Merged { output } => RecordedOutput::Merged { output },
    }
}

/// Recorded output as the program receives it.
fn captured_output(output: RecordedOutput) -> CapturedOutput {
    match output {
        RecordedOutput::Split { stdout, stderr } => CapturedOutput::Split { stdout, stderr },
        RecordedOutput::Merged { output } => CapturedOutput::Merged { output },
    }
}

/// A duration in nanoseconds.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// A time in nanoseconds since the Unix epoch.
fn epoch_nanos(time: SystemTime) -> u64 {
    nanos(
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

/// Now, in nanoseconds since the Unix epoch.
fn now_nanos() -> u64 {
    epoch_nanos(SystemTime::now())
}

/// How a command that ran ended, as the store records it.
const fn step_exit(termination: Termination) -> StepExit {
    match termination {
        Termination::Exited(code) => StepExit::Exited { code },
        Termination::Signaled(signal) => StepExit::Signaled { signal },
    }
}

/// What a step whose record is `record` returns on a cache hit: the recorded exit
/// status and raw output, with `cached` set, or the recorded failure of a missing or
/// non-executable program.
fn replay(record: StepRecord) -> Result<StepOutcome, StepError> {
    let termination = match record.exit {
        StepExit::Exited { code } => Termination::Exited(code),
        StepExit::Signaled { signal } => Termination::Signaled(signal),
        StepExit::ProgramNotFound | StepExit::ProgramNotExecutable | StepExit::NotStarted => {
            let program = record.spec.argv.first().map_or("", String::as_str);
            return Err(StepError::new(record.failure.unwrap_or_else(|| {
                format!("program `{program}` could not be run")
            })));
        },
    };
    Ok(StepOutcome {
        termination,
        output: captured_output(record.output),
        cached: true,
    })
}

impl TaskSteps<'_> {
    /// The key of each step so far, in order.
    #[must_use]
    pub fn keys(&self) -> Vec<StepKey> {
        self.keys.borrow().clone()
    }

    /// What the build decided about each step so far, in order: why each one ran,
    /// or that it was cached.
    #[must_use]
    pub fn decisions(&self) -> Vec<StepDecision> {
        self.decisions.borrow().clone()
    }

    /// End the run of the task: store the step positions of its cached steps, add
    /// the task to the owners of the record each of them names, remove the positions
    /// beyond its last step, and store its task record, all in one transaction, with
    /// `succeeded` saying whether the run succeeded. Without a task identity this
    /// stores nothing.
    ///
    /// Every way a step is noted as cached makes its task an owner here: a hit on a
    /// stored record, and a result reused within the build, an `always` result or a
    /// result that was not cached, whose run stored the record under the key. A
    /// record that the task already owns is not written again for its owners. A step
    /// that ran stored its position and became its record's owner when it ran.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] when the records cannot be written.
    pub fn finish(
        &self,
        succeeded: bool,
    ) -> Result<(), StoreError> {
        let Some(identity) = &self.identity else {
            return Ok(());
        };
        let steps = u32::try_from(self.keys.borrow().len()).unwrap_or(u32::MAX);
        let mut txn = self.engine.store.write()?;
        for (key, position) in self.cached_positions.take() {
            if let Some(mut record) = txn.get::<StepRecord>(&position.step)?
                && record.owners.insert(identity.id.clone())
            {
                txn.put(&position.step, &record)?;
            }
            txn.put(&key, &position)?;
        }
        for (key, _) in txn.scan::<StepPosition>(&identity.id.key_prefix())? {
            if key.task == identity.id && key.position >= steps {
                txn.delete::<StepPosition>(&key)?;
            }
        }
        txn.put(
            &identity.id,
            &identity.task_record(succeeded, self.engine.build, steps),
        )?;
        txn.commit()
    }

    /// The patterns `patterns` of a step, anchored at the task directory.
    fn patterns(
        &self,
        patterns: &[String],
    ) -> Result<PathPatterns, StepError> {
        PathPatterns::new(patterns, &self.task_dir, self.engine.home.as_ref())
            .map_err(|error| StepError::new(format_args!("run(): {error}")))
    }

    /// The record stored under `key`, if there is one.
    fn record(
        &self,
        key: &StepKey,
    ) -> Result<Option<StepRecord>, StepError> {
        self.engine
            .store
            .read()
            .and_then(|txn| txn.get::<StepRecord>(key))
            .map_err(|error| store_error(&error))
    }

    /// Store `record`, the record of a cache hit with refreshed observations, under
    /// `key`, keeping the owners of the record stored there; and forget any result
    /// of the build under `key` that was not cached.
    fn put(
        &self,
        key: &StepKey,
        record: &mut StepRecord,
    ) -> Result<(), StepError> {
        let write = |record: &mut StepRecord| -> Result<(), StoreError> {
            let mut txn = self.engine.store.write()?;
            if let Some(stored) = txn.get::<StepRecord>(key)? {
                record.owners.extend(stored.owners);
            }
            txn.put(key, record)?;
            txn.commit()
        };
        write(record).map_err(|error| store_error(&error))?;
        lock(&self.engine.not_cached).remove(key);
        Ok(())
    }

    /// Store `record`, the record of a run of the step `pending`, and, with a task
    /// identity, a last-writer record for every path the step wrote and the step's
    /// position with its explanation, all in one transaction; then forget any
    /// result of the build under its key that was not cached.
    ///
    /// The record keeps the owners of the record it replaces under the same key,
    /// whether the run is a run for changed inputs or one whose result is not
    /// cached, and with a task identity it adds the task to them, in the same
    /// transaction, so the task owns the record that its step position names.
    fn put_run(
        &self,
        pending: &Pending,
        record: &mut StepRecord,
    ) -> Result<(), StepError> {
        let write = |record: &mut StepRecord| -> Result<(), StoreError> {
            let mut txn = self.engine.store.write()?;
            if let Some(stored) = txn.get::<StepRecord>(&pending.key)? {
                record.owners.extend(stored.owners);
            }
            if let Some(identity) = &self.identity {
                record.owners.insert(identity.id.clone());
            }
            txn.put(&pending.key, record)?;
            if let Some(identity) = &self.identity {
                for observation in &record.observations {
                    if observation.access.contains(AccessBits::WRITE) {
                        txn.put(&PathKey(observation.path.clone()), &LastWriter {
                            task: identity.id.clone(),
                            by: WrittenBy::Step {
                                step: pending.key,
                                position: pending.position,
                            },
                            build: self.engine.build,
                            written_at_nanos: record.recorded_at_nanos,
                        })?;
                    }
                }
                txn.put(
                    &PositionKey {
                        task: identity.id.clone(),
                        position: pending.position,
                    },
                    &StepPosition {
                        step: pending.key,
                        build: self.engine.build,
                        verdict: Verdict::Ran(pending.explanation.clone()),
                        carried: pending.carried.clone(),
                    },
                )?;
            }
            txn.commit()
        };
        write(record).map_err(|error| store_error(&error))?;
        lock(&self.engine.not_cached).remove(&pending.key);
        Ok(())
    }

    /// Decide whether the step `step`, whose key is `key`, runs.
    fn decide(
        &self,
        key: StepKey,
        step: &StepSpec,
    ) -> Result<Decision, StepError> {
        let remembered = lock(&self.engine.not_cached).get(&key).cloned();
        if step.always {
            let known = remembered.is_some() || self.record(&key)?.is_some();
            if let Some(first) = self.engine.first_always_result(key) {
                return Ok(Decision::Replay(first));
            }
            let reason = if known {
                RunReason::Always
            } else {
                RunReason::NeverRun
            };
            return Ok(Decision::Run(reason, None));
        }
        if let Some(run) = remembered {
            let checked = check(
                &run.observations,
                run.recorded_at_nanos,
                &self.engine.states,
            );
            return Ok(if checked.is_hit() {
                Decision::Replay(run.result)
            } else {
                Decision::Run(
                    RunReason::NotCached {
                        why: run.why,
                        differences: checked.differences,
                    },
                    Some(Baseline::new(&run.observations, run.recorded_at_nanos)),
                )
            });
        }
        let Some(record) = self.record(&key)? else {
            return Ok(Decision::Run(RunReason::NeverRun, None));
        };
        let checked_at_nanos = now_nanos();
        let checked = check(
            &record.observations,
            record.recorded_at_nanos,
            &self.engine.states,
        );
        if let Some(why) = record.not_cached {
            // The record of a run of an earlier build whose result was not cached:
            // never a hit, so the step runs again, with that as the reason.
            return Ok(Decision::Run(
                RunReason::NotCached {
                    why,
                    differences: checked.differences,
                },
                Some(Baseline::new(
                    &record.observations,
                    record.recorded_at_nanos,
                )),
            ));
        }
        Ok(if checked.is_hit() {
            Decision::Hit {
                record: Box::new(record),
                checked,
                checked_at_nanos,
            }
        } else {
            Decision::Run(
                RunReason::InputsChanged {
                    differences: checked.differences,
                },
                Some(Baseline::new(
                    &record.observations,
                    record.recorded_at_nanos,
                )),
            )
        })
    }

    /// The step-position record of the step at `position`, before this decision.
    fn stored_position(
        &self,
        txn: &Txn<'_>,
        position: u32,
    ) -> Result<Option<StepPosition>, StoreError> {
        match &self.identity {
            Some(identity) => {
                txn.get::<StepPosition>(&PositionKey {
                    task: identity.id.clone(),
                    position,
                })
            },
            None => Ok(None),
        }
    }

    /// Explain why the step at `position`, whose key is `key` and whose
    /// specification is `spec`, runs for `reason`, with `baseline`, the recorded
    /// state the cache check compared; and return the self-inflicted inputs its
    /// position carries from the task's previous build.
    fn explain(
        &self,
        position: u32,
        key: StepKey,
        spec: &StepSpecRecord,
        reason: &RunReason,
        baseline: Option<&Baseline>,
    ) -> Result<(Explanation, Vec<PathBuf>), StoreError> {
        let txn = self.engine.store.read()?;
        let previous = self.stored_position(&txn, position)?;
        let context = InputContext {
            identity: self.identity.as_ref(),
            position,
            key,
            build: self.engine.build,
            baseline,
        };
        let reasons = match reason {
            RunReason::NeverRun => self.never_run(&txn, previous.as_ref(), key, spec)?,
            RunReason::Always => vec![Reason::Always],
            RunReason::InputsChanged { differences } => {
                vec![Reason::InputsChanged {
                    inputs: changed_inputs(&txn, &context, differences)?,
                }]
            },
            RunReason::NotCached { why, differences } => {
                vec![Reason::NotCached {
                    why: why.clone(),
                    inputs: changed_inputs(&txn, &context, differences)?,
                }]
            },
        };
        let carried = carried_from(previous.as_ref(), self.engine.build);
        let mut explanation = Explanation {
            reasons,
            repeated: Vec::new(),
        };
        explanation.repeated = explanation
            .self_inflicted()
            .into_iter()
            .filter(|path| carried.binary_search(path).is_ok())
            .collect();
        Ok((explanation, carried))
    }

    /// The reasons of a step key with no record: what changed since the
    /// specification the task used at the same position last time, `previous`, or
    /// that the step never ran.
    fn never_run(
        &self,
        txn: &Txn<'_>,
        previous: Option<&StepPosition>,
        key: StepKey,
        spec: &StepSpecRecord,
    ) -> Result<Vec<Reason>, StoreError> {
        let Some(previous) = previous.filter(|previous| previous.step != key) else {
            return Ok(vec![Reason::NeverRun]);
        };
        let Some(old) = txn.get::<StepRecord>(&previous.step)? else {
            return Ok(vec![Reason::NeverRun]);
        };
        let old_task = match &self.identity {
            Some(identity) => txn.get::<TaskRecord>(&identity.id)?,
            None => None,
        };
        let reasons = spec_reasons(
            &old.spec,
            spec,
            old_task.as_ref(),
            self.identity.as_ref().map(|identity| &identity.parts),
        );
        Ok(if reasons.is_empty() {
            vec![Reason::NeverRun]
        } else {
            reasons
        })
    }

    /// Note that the step `step` at `position`, whose key is `key`, was cached; its
    /// position is stored, and its task becomes an owner of the record under `key`,
    /// when the run finishes, unless it already ran in this build with the same key,
    /// whose reasons it keeps and whose record its run made it an owner of.
    fn note_cached(
        &self,
        position: u32,
        key: StepKey,
        step: &StepSpec,
    ) -> Result<(), StepError> {
        self.decisions.borrow_mut().push(StepDecision {
            position,
            command: step.command(),
            verdict: Verdict::Cached,
        });
        let Some(identity) = &self.identity else {
            return Ok(());
        };
        let previous = self
            .engine
            .store
            .read()
            .and_then(|txn| self.stored_position(&txn, position))
            .map_err(|error| store_error(&error))?;
        if let Some(previous) = &previous
            && previous.build == self.engine.build
            && previous.step == key
            && matches!(previous.verdict, Verdict::Ran(_))
        {
            return Ok(());
        }
        let stored = StepPosition {
            step: key,
            build: self.engine.build,
            verdict: Verdict::Cached,
            carried: carried_from(previous.as_ref(), self.engine.build),
        };
        self.cached_positions.borrow_mut().push((
            PositionKey {
                task: identity.id.clone(),
                position,
            },
            stored,
        ));
        Ok(())
    }

    /// Note that the step `step` of `pending` ran, and warn when it re-ran in
    /// consecutive builds for the same self-inflicted reason.
    fn note_ran(
        &self,
        step: &StepSpec,
        pending: &Pending,
    ) {
        let command = step.command();
        if !pending.explanation.repeated.is_empty() {
            self.engine.warn(RepeatedWarning {
                task: self.identity.as_ref().map(TaskIdentity::label),
                position: pending.position,
                command: command.clone(),
                paths: pending.explanation.repeated.clone(),
            });
        }
        self.decisions.borrow_mut().push(StepDecision {
            position: pending.position,
            command,
            verdict: Verdict::Ran(pending.explanation.clone()),
        });
    }

    /// Return the result of the cache hit on `record`, stored under `key`, after
    /// storing the new metadata in `checked` when it has any, with the owners of the
    /// stored record. The task becomes an owner when [`TaskSteps::finish`] stores
    /// the step's position, and only if it is not one already.
    fn hit(
        &self,
        key: &StepKey,
        mut record: StepRecord,
        checked: Checked,
        checked_at_nanos: u64,
    ) -> Result<StepOutcome, StepError> {
        if checked.refresh {
            record.observations = checked.observations;
            record.recorded_at_nanos = checked_at_nanos;
            self.put(key, &mut record)?;
        }
        replay(record)
    }

    /// Run `step` with the found program `program`; `checked` holds the locations
    /// the lookup checked.
    fn run(
        &self,
        step: &StepSpec,
        pending: &Pending,
        program: AbsolutePath,
        checked: Accesses,
    ) -> Result<StepOutcome, StepError> {
        let request = RunRequest {
            program: program.clone(),
            args: step.argv.iter().map(OsString::from).collect(),
            env: step
                .env
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect(),
            cwd: step.cwd.clone(),
            stdin: step.stdin.as_ref().map(|text| text.as_bytes().to_vec()),
            capture: step.capture,
            echo: step.stream,
        };
        let ran_at = SystemTime::now();
        let started = Instant::now();
        let result = self
            .engine
            .tracer
            .run(&request, self.engine.echo, &self.engine.cancel);
        let duration = started.elapsed();
        let lookup_checked = (!checked.is_empty()).then(|| checked_list(&checked));
        let mut accesses = checked;
        match result {
            Ok(result) => {
                let why = self.not_cached_reason(&result);
                accesses.merge(result.accesses);
                self.engine.states.forget_written(&accesses);
                let outcome = StepOutcome {
                    termination: result.termination,
                    output: result.output.clone(),
                    cached: false,
                };
                let finished = Finished {
                    program: Some(program),
                    accesses,
                    exit: step_exit(result.termination),
                    output: result.output,
                    duration,
                    ran_at,
                    failure: None,
                };
                if let Some(why) = why {
                    self.remember(step, pending, finished, why, Ok(outcome.clone()))?;
                } else {
                    self.store(step, pending, finished)?;
                }
                Ok(outcome)
            },
            Err(error) if error.is_missing_program() => {
                if let Some(touched) = error.accesses() {
                    accesses.merge(touched.clone());
                }
                self.engine.states.forget_written(&accesses);
                let exit = if matches!(error, TraceError::ProgramNotFound { .. }) {
                    StepExit::ProgramNotFound
                } else {
                    StepExit::ProgramNotExecutable
                };
                let message = match lookup_checked {
                    None => error.to_string(),
                    Some(list) => format!("{error}; checked {list}"),
                };
                self.store(step, pending, Finished {
                    program: Some(program),
                    accesses,
                    exit,
                    output: CapturedOutput::empty(step.capture),
                    duration,
                    ran_at,
                    failure: Some(message.clone()),
                })?;
                Err(StepError::new(message))
            },
            Err(error) if !error.is_tracer_failure() => {
                if let Some(touched) = error.accesses() {
                    accesses.merge(touched.clone());
                }
                self.engine.states.forget_written(&accesses);
                let message = error.to_string();
                let failure = StepError::new(&message);
                let finished = Finished {
                    program: Some(program),
                    accesses,
                    exit: StepExit::NotStarted,
                    output: CapturedOutput::empty(step.capture),
                    duration,
                    ran_at,
                    failure: Some(message.clone()),
                };
                let why = NotCachedReason::StartFailed { message };
                self.remember(step, pending, finished, why, Err(failure.clone()))?;
                Err(failure)
            },
            Err(error) => Err(StepError::new(&error)),
        }
    }

    /// Why the result `result` of a run is not cached, or `None` when it is.
    ///
    /// Every run that returns once cancellation has been requested is interrupted,
    /// however it ended: by a signal, or with an exit code that the command chose
    /// after it handled the interrupt.
    fn not_cached_reason(
        &self,
        result: &RunResult,
    ) -> Option<NotCachedReason> {
        if self.engine.cancel.is_cancelled() {
            return Some(match result.termination {
                Termination::Signaled(signal) => NotCachedReason::Interrupted { signal },
                Termination::Exited(code) => NotCachedReason::InterruptedExited { code },
            });
        }
        if let Termination::Signaled(signal) = result.termination {
            return Some(NotCachedReason::Signaled { signal });
        }
        result.incomplete.as_ref().map(|reason| {
            NotCachedReason::Incomplete {
                detail: reason.to_string(),
            }
        })
    }

    /// Store the record of `step`, which ended as `finished` with a result that is
    /// not cached for the reason `why`, in place of any older record of its key,
    /// whose owners it keeps, so no older result is replayed; and keep the result in
    /// memory for the rest of the build, with the states of the paths it touched.
    fn remember(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
        why: NotCachedReason,
        result: Result<StepOutcome, StepError>,
    ) -> Result<(), StepError> {
        let mut record = self.record_of(step, pending, finished, Some(why.clone()));
        self.put_run(pending, &mut record)?;
        let result = result.map(|outcome| {
            StepOutcome {
                cached: true,
                ..outcome
            }
        });
        lock(&self.engine.not_cached).insert(pending.key, NotCachedRun {
            why,
            observations: record.observations,
            recorded_at_nanos: record.recorded_at_nanos,
            result,
        });
        Ok(())
    }

    /// Record the state of every path `finished` touched and store the record of
    /// `step`, with the owners of the record it replaces and its task.
    fn store(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
    ) -> Result<(), StepError> {
        let mut record = self.record_of(step, pending, finished, None);
        self.put_run(pending, &mut record)
    }

    /// The specification of `step`, whose occurrence number is `occurrence`, as its
    /// record holds it.
    fn spec_record(
        &self,
        step: &StepSpec,
        occurrence: u32,
    ) -> StepSpecRecord {
        StepSpecRecord {
            program_hash: ContentHash(*self.program.as_bytes()),
            cwd: step.cwd.as_path().to_path_buf(),
            argv: step.argv.clone(),
            env: step.env.clone(),
            stdin: step.stdin.clone(),
            capture: capture(step.capture),
            stream: step.stream,
            always: step.always,
            ignore: step.ignore.clone(),
            track: step.track.clone(),
            expect: step.expect.clone(),
            occurrence,
        }
    }

    /// The record of `step`, which ended as `finished`, with the state of every path
    /// it touched; `not_cached` says why its result is not cached, if it is not. Its
    /// owners are empty until [`TaskSteps::put_run`] stores it.
    fn record_of(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
        not_cached: Option<NotCachedReason>,
    ) -> StepRecord {
        let recorded_at_nanos = now_nanos();
        let observed = observe(
            &finished.accesses,
            &pending.ignore,
            &pending.track,
            &self.engine.states,
        );
        StepRecord {
            spec: self.spec_record(step, pending.occurrence),
            program: finished
                .program
                .map(|program| ProgramPath(program.into_path_buf())),
            observations: observed.observations,
            ignored: observed.ignored,
            exit: finished.exit,
            output: recorded_output(finished.output),
            duration_nanos: nanos(finished.duration),
            ran_at_nanos: epoch_nanos(finished.ran_at),
            reason: pending.reason.clone(),
            recorded_at_nanos,
            failure: finished.failure,
            not_cached,
            owners: BTreeSet::new(),
        }
    }

    /// Look the program of `step`, whose first argument is `name`, up and run it,
    /// or record that it is missing or not executable.
    fn look_up_and_run(
        &self,
        step: &StepSpec,
        name: &str,
        pending: &Pending,
    ) -> Result<StepOutcome, StepError> {
        let lookup = find_program(name, &step.env, &step.cwd).map_err(StepError::new)?;
        let exit = match lookup {
            ProgramLookup::Found { program, checked } => {
                return self.run(step, pending, program, checked);
            },
            ProgramLookup::NotFound { .. } => StepExit::ProgramNotFound,
            ProgramLookup::NotExecutable { .. } => StepExit::ProgramNotExecutable,
        };
        let message = missing_program_message(name, &lookup);
        self.store(step, pending, Finished {
            program: None,
            accesses: lookup.checked().clone(),
            exit,
            output: CapturedOutput::empty(step.capture),
            duration: Duration::ZERO,
            ran_at: SystemTime::now(),
            failure: Some(message.clone()),
        })?;
        Err(StepError::new(message))
    }
}

impl StepExecutor for TaskSteps<'_> {
    fn execute(
        &self,
        step: &StepSpec,
    ) -> Result<StepOutcome, StepError> {
        let Some(name) = step.argv.first() else {
            return Err(StepError::new("run() needs a program"));
        };
        let position = u32::try_from(self.keys.borrow().len()).unwrap_or(u32::MAX);
        let (key, occurrence) = self
            .occurrences
            .borrow_mut()
            .next_key(self.program, step)
            .map_err(StepError::new)?;
        self.keys.borrow_mut().push(key);
        let ignore = self.patterns(&step.ignore)?;
        let track = self.patterns(&step.track)?;
        let (reason, baseline) = match self.decide(key, step)? {
            Decision::Run(reason, baseline) => (reason, baseline),
            Decision::Hit {
                record,
                checked,
                checked_at_nanos,
            } => {
                let result = self.hit(&key, *record, checked, checked_at_nanos);
                self.note_cached(position, key, step)?;
                return result;
            },
            Decision::Replay(result) => {
                self.note_cached(position, key, step)?;
                return result;
            },
        };
        let spec = self.spec_record(step, occurrence);
        let (explanation, carried) = self
            .explain(position, key, &spec, &reason, baseline.as_ref())
            .map_err(|error| store_error(&error))?;
        let pending = Pending {
            key,
            occurrence,
            reason,
            ignore,
            track,
            position,
            explanation,
            carried,
        };
        self.note_ran(step, &pending);
        let result = self.look_up_and_run(step, name, &pending);
        if step.always {
            self.engine.finish_always(key, &result);
        }
        result
    }
}
