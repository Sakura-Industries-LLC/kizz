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
//! A step marked `always` runs once per build, in the first pass: the first time
//! its key comes up in the build, and it is recorded like any other step. Every
//! later time in the same build, whatever changed, its first result is returned
//! again without running ("Commands that always run").
//!
//! Results that are not cached ("Results that are not cached"): a command killed by
//! a signal, interrupted ones included, a run that the tracer reports as
//! incompletely traced, and a command that could not start for a reason other than a
//! missing or non-executable program. Their record is marked with why, which makes
//! it a record that is never a cache hit and replaces any older record of the step
//! key, so no older result is replayed. The engine also keeps the paths such a
//! command touched, with their states, in memory for the rest of the build: when
//! the step comes up again, it runs again only if one of them changed, and otherwise
//! its result is returned again without running. In a later build, the marked
//! record makes the step run again, with that stated as the reason.
//!
//! A missing or non-executable program fails the task and is recorded like any
//! other failure, with every path checked and the failure message, which a cache hit
//! fails with again. A bare program name without `PATH` in the step's environment,
//! a malformed pattern, and a tracer failure fail the task without a record.

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsString,
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant, SystemTime},
};

use kizz_store::{
    Capture, ContentHash, NotCachedReason, Observation, ProgramPath, RecordedOutput, RunReason,
    StepExit, StepKey, StepRecord, StepSpecRecord, Store, StoreError,
};
use kizz_trace::{
    AbsolutePath, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink, RunRequest,
    RunResult, Termination, TraceError, Tracer,
};

use crate::{
    check::{Checked, check},
    key::Occurrences,
    lookup::{ProgramLookup, find_program},
    observe::observe,
    patterns::PathPatterns,
    runtime::ProgramHash,
    states::FileStates,
    step::{StepError, StepExecutor, StepOutcome, StepSpec},
};

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
    /// whose directory is `task_dir`.
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
            occurrences: RefCell::new(Occurrences::new()),
            keys: RefCell::new(Vec::new()),
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
    /// Counts identical commands in the run.
    occurrences: RefCell<Occurrences>,
    /// The key of each step of the run, in order.
    keys: RefCell<Vec<StepKey>>,
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
}

/// Whether a step runs, as the cache check decided.
enum Decision {
    /// The step runs, for this reason.
    Run(RunReason),
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

    /// Store `record` under `key`, and forget any result of the build under `key`
    /// that was not cached.
    fn put(
        &self,
        key: &StepKey,
        record: &StepRecord,
    ) -> Result<(), StepError> {
        let mut txn = self
            .engine
            .store
            .write()
            .map_err(|error| store_error(&error))?;
        txn.put(key, record).map_err(|error| store_error(&error))?;
        txn.commit().map_err(|error| store_error(&error))?;
        lock(&self.engine.not_cached).remove(key);
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
            return Ok(Decision::Run(if known {
                RunReason::Always
            } else {
                RunReason::NeverRun
            }));
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
                Decision::Run(RunReason::NotCached {
                    why: run.why,
                    differences: checked.differences,
                })
            });
        }
        let Some(record) = self.record(&key)? else {
            return Ok(Decision::Run(RunReason::NeverRun));
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
            return Ok(Decision::Run(RunReason::NotCached {
                why,
                differences: checked.differences,
            }));
        }
        Ok(if checked.is_hit() {
            Decision::Hit {
                record: Box::new(record),
                checked,
                checked_at_nanos,
            }
        } else {
            Decision::Run(RunReason::InputsChanged {
                differences: checked.differences,
            })
        })
    }

    /// Return the result of the cache hit on `record`, stored under `key`, after
    /// storing the new metadata in `checked` when it has any.
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
            self.put(key, &record)?;
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
    fn not_cached_reason(
        &self,
        result: &RunResult,
    ) -> Option<NotCachedReason> {
        if let Termination::Signaled(signal) = result.termination {
            return Some(if self.engine.cancel.is_cancelled() {
                NotCachedReason::Interrupted { signal }
            } else {
                NotCachedReason::Signaled { signal }
            });
        }
        result.incomplete.as_ref().map(|reason| {
            NotCachedReason::Incomplete {
                detail: reason.to_string(),
            }
        })
    }

    /// Store the record of `step`, which ended as `finished` with a result that is
    /// not cached for the reason `why`, in place of any older record of its key, so
    /// no older result is replayed; and keep the result in memory for the rest of
    /// the build, with the states of the paths it touched.
    fn remember(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
        why: NotCachedReason,
        result: Result<StepOutcome, StepError>,
    ) -> Result<(), StepError> {
        let record = self.record_of(step, pending, finished, Some(why.clone()));
        self.put(&pending.key, &record)?;
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
    /// `step`.
    fn store(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
    ) -> Result<(), StepError> {
        let record = self.record_of(step, pending, finished, None);
        self.put(&pending.key, &record)
    }

    /// The record of `step`, which ended as `finished`, with the state of every path
    /// it touched; `not_cached` says why its result is not cached, if it is not.
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
            spec: StepSpecRecord {
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
                occurrence: pending.occurrence,
            },
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
        let (key, occurrence) = self
            .occurrences
            .borrow_mut()
            .next_key(self.program, step)
            .map_err(StepError::new)?;
        self.keys.borrow_mut().push(key);
        let ignore = self.patterns(&step.ignore)?;
        let track = self.patterns(&step.track)?;
        let reason = match self.decide(key, step)? {
            Decision::Run(reason) => reason,
            Decision::Hit {
                record,
                checked,
                checked_at_nanos,
            } => return self.hit(&key, *record, checked, checked_at_nanos),
            Decision::Replay(result) => return result,
        };
        let pending = Pending {
            key,
            occurrence,
            reason,
            ignore,
            track,
        };
        let result = self.look_up_and_run(step, name, &pending);
        if step.always {
            self.engine.finish_always(key, &result);
        }
        result
    }
}
