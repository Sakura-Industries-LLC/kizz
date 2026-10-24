// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The engine side of the step executor.
//!
//! An [`Engine`] holds what every task of a build shares: the tracer, the store,
//! where live output goes, the cancellation flag, and the caller's home directory.
//! For each run of a task program, [`Engine::task`] makes a [`TaskSteps`], the
//! [`StepExecutor`] that the runtime hands each `run()` call to. For each step it:
//!
//! 1. derives the step key, counting identical commands earlier in the run;
//! 2. looks the program up through the `PATH` of the step's environment, recording every
//!    location checked;
//! 3. runs the command through the tracer;
//! 4. records the state of every touched path, concurrently, applying the step's ignore
//!    and `track` patterns; and
//! 5. stores the step record under the step key.
//!
//! A missing or non-executable program fails the task and is recorded like any
//! other failure, with every path checked. A bare program name without `PATH` in
//! the step's environment, a malformed pattern, a command that could not start for
//! another reason, and a tracer failure fail the task without a record.
//!
//! Every step runs: comparing a stored record with the file system, the cache
//! check, is not part of this executor, so the reason it records is that the step
//! never ran, that it is marked `always`, or that its record was not checked.

use std::{
    cell::RefCell,
    ffi::OsString,
    fmt,
    time::{Duration, Instant, SystemTime},
};

use kizz_store::{
    Capture, ContentHash, ProgramPath, RecordedOutput, RunReason, StepExit, StepKey, StepRecord,
    StepSpecRecord, Store, StoreError,
};
use kizz_trace::{
    AbsolutePath, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink, RunRequest,
    Termination, TraceError, Tracer,
};

use crate::{
    key::Occurrences,
    lookup::{ProgramLookup, find_program},
    observe::{HashCounter, observe},
    patterns::PathPatterns,
    runtime::ProgramHash,
    step::{StepError, StepExecutor, StepOutcome, StepSpec},
};

/// What every task of a build shares to run and record its steps.
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
    /// Counts the files whose content was hashed.
    hashed: HashCounter,
}

impl fmt::Debug for Engine<'_> {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.debug_struct("Engine")
            .field("store", &self.store)
            .field("home", &self.home)
            .field("hashed", &self.hashed)
            .finish_non_exhaustive()
    }
}

impl<'a> Engine<'a> {
    /// An engine that runs commands with `tracer`, echoes live output to `echo`,
    /// stops commands when `cancel` is set, stores records in `store`, and expands
    /// `~/` in patterns to `home`, the caller's home directory.
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
            hashed: HashCounter::new(),
        }
    }

    /// The number of files whose content this engine has hashed.
    #[must_use]
    pub fn files_hashed(&self) -> u64 {
        self.hashed.files()
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

/// A duration in nanoseconds.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// How a command that ran ended, as the store records it.
const fn step_exit(termination: Termination) -> StepExit {
    match termination {
        Termination::Exited(code) => StepExit::Exited { code },
        Termination::Signaled(signal) => StepExit::Signaled { signal },
    }
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

    /// Why the step `step`, whose key is `key`, runs.
    fn reason(
        &self,
        key: &StepKey,
        step: &StepSpec,
    ) -> Result<RunReason, StepError> {
        let txn = self
            .engine
            .store
            .read()
            .map_err(|error| store_error(&error))?;
        let recorded = txn
            .get::<StepRecord>(key)
            .map_err(|error| store_error(&error))?;
        Ok(if recorded.is_none() {
            RunReason::NeverRun
        } else if step.always {
            RunReason::Always
        } else {
            RunReason::Unchecked
        })
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
                accesses.merge(result.accesses);
                let outcome = StepOutcome {
                    termination: result.termination,
                    output: result.output.clone(),
                    cached: false,
                };
                self.store(step, pending, Finished {
                    program: Some(program),
                    accesses,
                    exit: step_exit(result.termination),
                    output: result.output,
                    duration,
                    ran_at,
                })?;
                Ok(outcome)
            },
            Err(error) if error.is_missing_program() => {
                if let Some(touched) = error.accesses() {
                    accesses.merge(touched.clone());
                }
                let exit = if matches!(error, TraceError::ProgramNotFound { .. }) {
                    StepExit::ProgramNotFound
                } else {
                    StepExit::ProgramNotExecutable
                };
                self.store(step, pending, Finished {
                    program: Some(program),
                    accesses,
                    exit,
                    output: CapturedOutput::empty(step.capture),
                    duration,
                    ran_at,
                })?;
                Err(match lookup_checked {
                    None => StepError::new(&error),
                    Some(list) => StepError::new(format_args!("{error}; checked {list}")),
                })
            },
            Err(error) => Err(StepError::new(&error)),
        }
    }

    /// Record the state of every path `finished` touched and store the record of
    /// `step`.
    fn store(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
    ) -> Result<(), StepError> {
        let observed = observe(
            &finished.accesses,
            &pending.ignore,
            &pending.track,
            &self.engine.hashed,
        );
        let record = StepRecord {
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
            ran_at_nanos: nanos(
                finished
                    .ran_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default(),
            ),
            reason: pending.reason.clone(),
        };
        let mut txn = self
            .engine
            .store
            .write()
            .map_err(|error| store_error(&error))?;
        txn.put(&pending.key, &record)
            .map_err(|error| store_error(&error))?;
        txn.commit().map_err(|error| store_error(&error))
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
        let pending = Pending {
            key,
            occurrence,
            reason: self.reason(&key, step)?,
            ignore: self.patterns(&step.ignore)?,
            track: self.patterns(&step.track)?,
        };
        let lookup = find_program(name, &step.env, &step.cwd).map_err(StepError::new)?;
        let exit = match lookup {
            ProgramLookup::Found { program, checked } => {
                return self.run(step, &pending, program, checked);
            },
            ProgramLookup::NotFound { .. } => StepExit::ProgramNotFound,
            ProgramLookup::NotExecutable { .. } => StepExit::ProgramNotExecutable,
        };
        let message = missing_program_message(name, &lookup);
        self.store(step, &pending, Finished {
            program: None,
            accesses: lookup.checked().clone(),
            exit,
            output: CapturedOutput::empty(step.capture),
            duration: Duration::ZERO,
            ran_at: SystemTime::now(),
        })?;
        Err(StepError::new(message))
    }
}
