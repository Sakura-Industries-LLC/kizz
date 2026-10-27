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
//!    recording every location checked, queries the machine state that the command starts
//!    with, through the tracer's machine query, and runs the command through the tracer;
//! 5. drops the cached hash of every path the step wrote, and records the state of every
//!    touched path, concurrently, applying the step's ignore and `track` patterns; and
//! 6. stores the step record, with the machine state, under the step key; a result that
//!    is not cached replaces any older record with a record marked as not cached.
//!
//! The cache check ("Cache check"): with no record for the key, the step runs, and
//! the reason is that it never ran. With a record, every recorded observation is
//! compared with the file system by [`check`]; the step is a
//! hit only when nothing changed, and otherwise it runs, with every difference as
//! the reason.
//!
//! Machine state ("Machine state"): a cache hit queries no machine state to record.
//! When the record names parts of the machine state, with `run(..., machine =
//! [...])`, the check queries the current state for the record's working directory
//! and compares each value of those parts with the recorded one; a difference makes
//! the step run, and the reason names each changed value, old and new. Each step key
//! is compared at most once per build, and not after its step ran in the build, so a
//! step that names a part that changes all the time, such as `memory`, runs at most
//! once per build and the build still settles.
//!
//! A step marked `always` runs at most once per build, when a pass first reaches it:
//! the first time its key comes up in the build, normally in the first pass, and it
//! is recorded like any other step. Every later time in the same build, whatever
//! changed, its first result is returned again without running ("Commands that
//! always run"). A key first reached in a later pass runs once then. Every step of a
//! task marked `cache=never` runs the same way, as if it were marked `always`, and
//! its reason names the task option. With [`Engine::with_no_cache`] (`kizz run
//! --no-cache`), every step runs the same way too, without a cache check, and is
//! still recorded; its reason names the option, unless the step is marked `always`,
//! its task is marked `cache=never`, or it never ran, whose reasons come first. The
//! option never makes a step cached.
//!
//! Step events ("Running one task with `kizz run`"): a run made with
//! [`TaskSteps::with_events`] tells its [`StepEvents`] when a step starts to run,
//! before its command starts and so before any of its output, and when a step comes
//! from the cache, with the result that the step returns. With
//! [`Engine::with_echo_every_step`], every command's output is echoed live, as if
//! every `run()` had `stream = True`; the option is in no step key or record.
//!
//! Results that are not cached ("Results that are not cached"): a command killed by
//! a signal, every run that returns once cancellation has been requested, whether a
//! signal ended it or it exited with a code after it handled the interrupt, a run
//! that the tracer reports as incompletely traced, a run in which a file changed
//! after the command first read it or ran it, and a command that could not start
//! for a reason other than a missing or non-executable program. A run that made a
//! system call that the tracer does not know is not cached either, unless the build
//! caches such results (`kizz build --cache-unknown-syscalls`, see
//! [`Engine::with_cache_unknown_system_calls`]); the tracer reports only the
//! numbers of such calls, and the record lists them either way. After it records
//! the states, the engine compares the first-read metadata that the tracer noted
//! with them (see [`changed_after_read`]); a change, when no other reason applies,
//! is the reason, and lists each changed file with whether the command wrote it.
//! Their record is marked with why, which makes it a record that is never a cache
//! hit and replaces any older record of the step key, so no older result is
//! replayed. The engine also keeps the paths such a command touched, with their
//! states, in memory for the rest of the build: when the step comes up again, it
//! runs again only if one of them changed, and otherwise its result is returned
//! again without running. In a later build, the marked record makes the step run
//! again, with that stated as the reason.
//!
//! A run's connections to IPv4 and IPv6 addresses and to Unix sockets, which the
//! tracer reports, never keep its result out of the cache: the record lists them,
//! and the engine keeps each run that made any for the build's warnings (see
//! [`Engine::connections`]). A cache hit runs nothing, so it adds no warning.
//!
//! Unchecked steps ("Exit codes and failure"): with [`Engine::with_fail_unchecked`],
//! a task run that leaves a non-zero exit unchecked fails when its program ends.
//! Which unchecked steps warn is the build's to say, from the results that stand
//! when it settles (see [`BuildReport::unchecked`](crate::BuildReport::unchecked)).
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
//!
//! Shared state ("Kizz never panics"): the engine's state that the threads of a
//! build share is locked with `kizz_threads::lock_within`, never with
//! `Mutex::lock`, which may panic when the calling thread holds the lock; a lock
//! that stays held for the engine's lock patience, `kizz_threads::LOCK_PATIENCE`
//! unless [`Engine::with_lock_patience`] sets another, is an error of the step, or
//! of the call that reads the state. The state of one task run lives in `Cell`s
//! that the private `cells` module changes, never in a `RefCell`, whose borrows
//! panic while the cell is borrowed.

use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap, HashSet},
    ffi::OsString,
    fmt, mem,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use kizz_store::{
    AccessBits, BuildId, Capture, Connection, ContentHash, DiskRecord, Explanation,
    FileSystemRecord, KernelRecord, LastWriter, LimitRecord, MachineChange, MachinePart,
    MachineStateRecord, MemoryRecord, NotCachedReason, Observation, PathKey, PositionKey,
    ProgramPath, Reason, RecordedOutput, RunReason, StepExit, StepKey, StepPosition, StepRecord,
    StepSpecRecord, Store, StoreError, TaskId, TaskRecord, Txn, Verdict, WrittenBy,
    machine_changes,
};
use kizz_threads::{LOCK_PATIENCE, LockError};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    FirstReads, MachineState, RunRequest, RunResult, Termination, TraceError, Tracer,
};

use crate::{
    cells,
    check::{Checked, check},
    explain::{
        Baseline, InputContext, RepeatedWarning, StepDecision, TaskIdentity, carried_from,
        changed_inputs, spec_reasons,
    },
    files::FileChanges,
    key::Occurrences,
    lookup::{ProgramLookup, find_program},
    observe::{changed_after_read, observe},
    patterns::PathPatterns,
    pool::WorkerPool,
    runtime::ProgramHash,
    states::FileStates,
    step::{StepError, StepExecutor, StepOutcome, StepSpec},
};

/// Follows the steps of a task run as they start or come from the cache, for the
/// live output of `kizz run`.
pub trait StepEvents {
    /// The step `decision` starts to run: its command has not started yet, so this
    /// comes before any of its output.
    fn step_started(
        &self,
        _decision: &StepDecision,
    ) {
    }

    /// The step `decision` came from the cache, or was returned again within the
    /// build, and returns `result`: its stored exit status and output, or its stored
    /// failure.
    fn step_cached(
        &self,
        _decision: &StepDecision,
        _result: &Result<StepOutcome, StepError>,
    ) {
    }
}

/// Follows no step.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoStepEvents;

impl StepEvents for NoStepEvents {}

/// Which commands' output the engine echoes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveOutput {
    /// Only the output of a `run()` with `stream = True`, as in a build.
    Streamed,
    /// Every command's output, as if every `run()` had `stream = True`, as `kizz
    /// run` asks.
    Every,
}

impl LiveOutput {
    /// Whether the output of `step` is echoed live.
    const fn echoes(
        self,
        step: &StepSpec,
    ) -> bool {
        step.stream || matches!(self, Self::Every)
    }
}

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

/// A run of a step that made system calls that the tracer does not know, as the
/// engine keeps it for the build's warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCalls {
    /// The step key.
    pub key: StepKey,
    /// The number of each such call, sorted and without repeats.
    pub numbers: Vec<u32>,
    /// Whether the result was cached: only when the build caches such results and
    /// no other reason kept it out of the cache.
    pub cached: bool,
}

/// A run of a step that connected to IPv4 or IPv6 addresses or to Unix sockets, as
/// the engine keeps it for the build's warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepConnections {
    /// The step key.
    pub key: StepKey,
    /// Each connection, sorted and without repeats.
    pub connections: Vec<Connection>,
    /// Whether the result was cached: connections never keep it out of the cache,
    /// but another reason may.
    pub cached: bool,
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
    /// Whether a run that made system calls that the tracer does not know is cached
    /// as if it had made none.
    cache_unknown_system_calls: bool,
    /// The runs of the build that made system calls that the tracer does not know,
    /// in the order they ended.
    unknown_calls: Mutex<Vec<UnknownCalls>>,
    /// The runs of the build that made connections, in the order they ended.
    connections: Mutex<Vec<StepConnections>>,
    /// Whether a task run that leaves a step unchecked fails when its program
    /// ends, as `kizz build --fail-unchecked` asks.
    fail_unchecked: bool,
    /// The command that runs the engine, as messages name it with one of its
    /// options: `kizz build` unless [`Engine::with_command_name`] sets another, such
    /// as `kizz run`.
    command_name: &'static str,
    /// The step keys whose recorded machine state the build already compared, or
    /// whose step ran in the build. The machine state is compared at most once per
    /// step key and build, so a step that names a part that changes all the time,
    /// such as `memory`, runs at most once per build, and the build still settles.
    machine_compared: Mutex<HashSet<StepKey>>,
    /// How long a lock of the shared state above may stay held by another holder
    /// before the step or the reader that waits for it fails: [`LOCK_PATIENCE`]
    /// unless [`Engine::with_lock_patience`] sets another.
    lock_patience: Duration,
    /// Whether every step runs, as if it were marked `always`, and is still
    /// recorded, as `kizz run --no-cache` asks.
    no_cache: bool,
    /// Which commands' output is echoed live: every command's, as if every `run()`
    /// had `stream = True`, as `kizz run` asks, or only that of a `run()` with
    /// `stream = True`.
    live_output: LiveOutput,
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
            .field(
                "cache_unknown_system_calls",
                &self.cache_unknown_system_calls,
            )
            .field("unknown_calls", &self.unknown_calls)
            .field("connections", &self.connections)
            .field("fail_unchecked", &self.fail_unchecked)
            .field("command_name", &self.command_name)
            .field("machine_compared", &self.machine_compared)
            .field("lock_patience", &self.lock_patience)
            .field("no_cache", &self.no_cache)
            .field("live_output", &self.live_output)
            .finish_non_exhaustive()
    }
}

/// What the engine's field `always_ran` holds, as a [`LockError`] names it.
const ALWAYS_RAN: &str = "the results of the build's `always` steps";

/// What the engine's field `not_cached` holds, as a [`LockError`] names it.
const NOT_CACHED: &str = "the build's results that were not cached";

/// What the engine's field `warnings` holds, as a [`LockError`] names it.
const WARNINGS: &str = "the build's warnings for repeated reasons";

/// What the engine's field `unknown_calls` holds, as a [`LockError`] names it.
const UNKNOWN_CALLS: &str = "the build's runs that made unknown system calls";

/// What the engine's field `connections` holds, as a [`LockError`] names it.
const CONNECTIONS: &str = "the build's runs that made connections";

/// What the engine's field `machine_compared` holds, as a [`LockError`] names it.
const MACHINE_COMPARED: &str = "the build's compared machine states";

/// Lock `mutex`, which guards `what`, whether or not a thread panicked while
/// holding it, with `kizz_threads::lock_within`, which never panics, waiting at
/// most `patience` while another holder keeps it.
fn lock<'m, T>(
    mutex: &'m Mutex<T>,
    what: &'static str,
    patience: Duration,
) -> Result<MutexGuard<'m, T>, LockError> {
    kizz_threads::lock_within(mutex, what, patience)
}

/// [`lock`], with a lock that stays held as the error of the step.
fn step_lock<'m, T>(
    mutex: &'m Mutex<T>,
    what: &'static str,
    patience: Duration,
) -> Result<MutexGuard<'m, T>, StepError> {
    lock(mutex, what, patience).map_err(StepError::new)
}

impl<'a> Engine<'a> {
    /// An engine for one build that runs commands with `tracer`, echoes live output
    /// to `echo`, stops commands when `cancel` is set, stores records in `store`,
    /// expands `~/` in patterns to `home`, the caller's home directory, and runs its
    /// parallel work, hashing included, on `pool`.
    #[must_use]
    pub fn new(
        tracer: &'a dyn Tracer,
        store: &'a Store,
        echo: &'a dyn EchoSink,
        cancel: Cancellation,
        home: Option<AbsolutePath>,
        pool: &WorkerPool,
    ) -> Self {
        Self {
            tracer,
            store,
            echo,
            cancel,
            home,
            states: FileStates::new(pool.clone()),
            always_ran: Mutex::new(HashMap::new()),
            not_cached: Mutex::new(HashMap::new()),
            build: next_build(),
            warnings: Mutex::new(Vec::new()),
            cache_unknown_system_calls: false,
            unknown_calls: Mutex::new(Vec::new()),
            connections: Mutex::new(Vec::new()),
            fail_unchecked: false,
            command_name: "kizz build",
            machine_compared: Mutex::new(HashSet::new()),
            lock_patience: LOCK_PATIENCE,
            no_cache: false,
            live_output: LiveOutput::Streamed,
        }
    }

    /// The engine, whose readers and steps wait at most `patience` for its shared
    /// state while another holder keeps it locked, and then fail with a
    /// [`LockError`] or a step error, instead of [`LOCK_PATIENCE`]. Tests use a
    /// short patience to reach those errors without waiting for the default.
    #[must_use]
    pub const fn with_lock_patience(
        mut self,
        patience: Duration,
    ) -> Self {
        self.lock_patience = patience;
        self
    }

    /// Hold the build's warnings for repeated reasons locked until the returned
    /// guard is dropped, so a test of a caller can reach what a reader does when
    /// the engine's state stays locked; only builds with debug assertions, such as
    /// tests, have it.
    ///
    /// # Errors
    ///
    /// A [`LockError`] when the warnings stay locked for the engine's patience.
    #[cfg(debug_assertions)]
    pub fn hold_warnings(&self) -> Result<impl Sized + '_, LockError> {
        lock(&self.warnings, WARNINGS, self.lock_patience)
    }

    /// The engine, which caches the result of a run that made system calls that
    /// the tracer does not know, as if it had made none, when `cache` is set, as
    /// `kizz build --cache-unknown-syscalls` asks. Without it, such a result is not
    /// cached unless no other reason applies. The option is in no step key or
    /// record.
    #[must_use]
    pub const fn with_cache_unknown_system_calls(
        mut self,
        cache: bool,
    ) -> Self {
        self.cache_unknown_system_calls = cache;
        self
    }

    /// Whether the engine caches the results of runs that made system calls that
    /// the tracer does not know.
    #[must_use]
    pub const fn caches_unknown_system_calls(&self) -> bool {
        self.cache_unknown_system_calls
    }

    /// The engine, which runs every step as if it were marked `always` when
    /// `no_cache` is set, as `kizz run --no-cache` asks: no cache check decides
    /// whether a step runs, every result is still recorded as usual, and the reason
    /// of a step that has a record names the option, unless the step is marked
    /// `always` or its task `cache=never`. Nothing makes a step cached that would
    /// run without it. The option is in no step key or record.
    #[must_use]
    pub const fn with_no_cache(
        mut self,
        no_cache: bool,
    ) -> Self {
        self.no_cache = no_cache;
        self
    }

    /// The engine, which echoes every command's output live when `echo` is set, as
    /// if every `run()` had `stream = True`, as `kizz run` asks. Without it, only
    /// the output of a `run()` with `stream = True` is echoed. The option is in no
    /// step key or record.
    #[must_use]
    pub const fn with_echo_every_step(
        mut self,
        echo: bool,
    ) -> Self {
        self.live_output = if echo {
            LiveOutput::Every
        } else {
            LiveOutput::Streamed
        };
        self
    }

    /// The runs of the build that made system calls that the tracer does not know,
    /// in the order they ended, each with whether its result was cached.
    ///
    /// # Errors
    ///
    /// A [`LockError`] when the runs stay locked.
    pub fn unknown_system_calls(&self) -> Result<Vec<UnknownCalls>, LockError> {
        Ok(lock(&self.unknown_calls, UNKNOWN_CALLS, self.lock_patience)?.clone())
    }

    /// The runs of the build that connected to IPv4 or IPv6 addresses or to Unix
    /// sockets, in the order they ended, each with its connections and whether its
    /// result was cached. A cache hit is no run, so it adds nothing.
    ///
    /// # Errors
    ///
    /// A [`LockError`] when the runs stay locked.
    pub fn connections(&self) -> Result<Vec<StepConnections>, LockError> {
        Ok(lock(&self.connections, CONNECTIONS, self.lock_patience)?.clone())
    }

    /// The engine, which makes a task run that leaves a step unchecked fail when
    /// its program ends, when `fail` is set, as `kizz build --fail-unchecked` asks.
    /// Without it, an unchecked step only warns. The option is in no step key or
    /// record.
    #[must_use]
    pub const fn with_fail_unchecked(
        mut self,
        fail: bool,
    ) -> Self {
        self.fail_unchecked = fail;
        self
    }

    /// Whether a task run that leaves a step unchecked fails.
    #[must_use]
    pub const fn fails_unchecked(&self) -> bool {
        self.fail_unchecked
    }

    /// The engine, whose messages name the command that runs it as `name`, such as
    /// `kizz run`, with the option they mention, as in the failure of a task that
    /// `--fail-unchecked` fails. Without it, they name `kizz build`. The name is in
    /// no step key or record.
    #[must_use]
    pub const fn with_command_name(
        mut self,
        name: &'static str,
    ) -> Self {
        self.command_name = name;
        self
    }

    /// The command that runs the engine, as messages name it: `kizz build` unless
    /// [`Engine::with_command_name`] set another.
    #[must_use]
    pub const fn command_name(&self) -> &'static str {
        self.command_name
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
    ///
    /// # Errors
    ///
    /// A [`LockError`] when the warnings stay locked.
    pub fn warnings(&self) -> Result<Vec<RepeatedWarning>, LockError> {
        Ok(lock(&self.warnings, WARNINGS, self.lock_patience)?.clone())
    }

    /// Keep `warning`, unless the same step already has one in this build.
    fn warn(
        &self,
        warning: RepeatedWarning,
    ) -> Result<(), StepError> {
        let mut warnings = step_lock(&self.warnings, WARNINGS, self.lock_patience)?;
        let known = warnings.iter().any(|known| {
            known.task.as_ref().map(|task| &task.task)
                == warning.task.as_ref().map(|task| &task.task)
                && known.position == warning.position
                && known.command == warning.command
        });
        if !known {
            warnings.push(warning);
        }
        Ok(())
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
    ///
    /// # Errors
    ///
    /// A [`LockError`] when the results stay locked.
    pub fn not_cached(&self) -> Result<Vec<(StepKey, NotCachedReason)>, LockError> {
        let mut steps: Vec<_> = lock(&self.not_cached, NOT_CACHED, self.lock_patience)?
            .iter()
            .map(|(key, run)| (*key, run.why.clone()))
            .collect();
        steps.sort_by_key(|(key, _)| *key);
        Ok(steps)
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
            occurrences: Cell::new(Occurrences::new()),
            keys: Cell::new(Vec::new()),
            decisions: Cell::new(Vec::new()),
            cached_positions: Cell::new(Vec::new()),
            cache_never: false,
            events: &NoStepEvents,
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
            occurrences: Cell::new(Occurrences::new()),
            keys: Cell::new(Vec::new()),
            decisions: Cell::new(Vec::new()),
            cached_positions: Cell::new(Vec::new()),
            cache_never: false,
            events: &NoStepEvents,
        }
    }

    /// The result of the `always` step `key` when it already ran in this build, as
    /// a result that did not run; otherwise mark it as running and return `None`.
    fn first_always_result(
        &self,
        key: StepKey,
    ) -> Result<Option<Result<StepOutcome, StepError>>, StepError> {
        let mut ran = step_lock(&self.always_ran, ALWAYS_RAN, self.lock_patience)?;
        Ok(match ran.get(&key) {
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
        })
    }

    /// Keep `result`, the result of the one run of the `always` step `key` in this
    /// build, to return every later time the step comes up in the build.
    fn finish_always(
        &self,
        key: StepKey,
        result: &Result<StepOutcome, StepError>,
    ) -> Result<(), StepError> {
        let result = result.clone().map(|outcome| {
            StepOutcome {
                cached: true,
                ..outcome
            }
        });
        step_lock(&self.always_ran, ALWAYS_RAN, self.lock_patience)?.insert(key, Some(result));
        Ok(())
    }
}

/// The step executor of one run of a task program.
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
    occurrences: Cell<Occurrences>,
    /// The key of each step of the run, in order.
    keys: Cell<Vec<StepKey>>,
    /// What the build decided about each step of the run, in order.
    decisions: Cell<Vec<StepDecision>>,
    /// The step positions of the cached steps, stored by [`TaskSteps::finish`],
    /// which also makes the task an owner of the record each of them names.
    cached_positions: Cell<Vec<(PositionKey, StepPosition)>>,
    /// Whether the task is marked `cache=never`, so every step runs as if it were
    /// marked `always`.
    cache_never: bool,
    /// Follows the steps as they start or come from the cache.
    events: &'a dyn StepEvents,
}

impl fmt::Debug for TaskSteps<'_> {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.debug_struct("TaskSteps")
            .field("engine", &self.engine)
            .field("program", &self.program)
            .field("task_dir", &self.task_dir)
            .field("identity", &self.identity)
            .field("keys", &cells::cloned(&self.keys))
            .field("decisions", &cells::cloned(&self.decisions))
            .field("cache_never", &self.cache_never)
            .finish_non_exhaustive()
    }
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
    /// The first-read metadata the tracer noted, empty for a command that did not
    /// run.
    first_reads: FirstReads,
    /// The numbers of the system calls that the command made and that the tracer
    /// does not know, sorted and without repeats; empty for a command that did not
    /// run.
    unknown_system_calls: Vec<u32>,
    /// The connections that the command made, sorted and without repeats; empty
    /// for a command that did not run.
    connections: Vec<Connection>,
    /// The machine state that the command started with, queried right before the
    /// lookup of its program.
    machine: MachineStateRecord,
}

/// The machine state `state` that the tracer reported, as a record keeps it.
fn machine_record(state: &MachineState) -> MachineStateRecord {
    MachineStateRecord {
        kernel: state.kernel.as_ref().map(|kernel| {
            KernelRecord {
                release: kernel.release.clone(),
                version: kernel.version.clone(),
            }
        }),
        limits: state.limits.as_ref().map(|limits| {
            limits
                .iter()
                .map(|limit| {
                    LimitRecord {
                        resource: limit.resource.clone(),
                        soft: limit.soft,
                        hard: limit.hard,
                    }
                })
                .collect()
        }),
        affinity: state.affinity.clone(),
        memory: state.memory.map(|memory| {
            MemoryRecord {
                total: memory.total,
                available: memory.available,
            }
        }),
        filesystem: state.filesystem.map(|filesystem| {
            FileSystemRecord {
                kind: filesystem.kind,
                flags: filesystem.flags,
            }
        }),
        disk: state.disk.map(|disk| {
            DiskRecord {
                size: disk.size,
                free: disk.free,
            }
        }),
    }
}

/// The connection `connection` that the tracer reported, as a record keeps it.
fn stored_connection(connection: &kizz_trace::Connection) -> Connection {
    match connection {
        kizz_trace::Connection::Ip(SocketAddr::V4(address)) => {
            Connection::Ipv4 {
                address: address.ip().octets(),
                port: address.port(),
            }
        },
        kizz_trace::Connection::Ip(SocketAddr::V6(address)) => {
            Connection::Ipv6 {
                address: address.ip().octets(),
                port: address.port(),
                scope: address.scope_id(),
            }
        },
        kizz_trace::Connection::UnixPath(path) => {
            Connection::UnixPath {
                path: path.as_path().to_path_buf(),
            }
        },
        kizz_trace::Connection::UnixAbstract(name) => {
            Connection::UnixAbstract { name: name.clone() }
        },
    }
}

/// The connections `connections` that the tracer reported, as a record keeps them:
/// sorted and without repeats.
fn stored_connections(connections: &BTreeSet<kizz_trace::Connection>) -> Vec<Connection> {
    connections
        .iter()
        .map(stored_connection)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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

/// The request that runs `step` with the found program `program`: its exact
/// arguments, environment, working directory, `stdin`, capture mode and echo,
/// which `live_output` decides, without the process tree, which a build never asks
/// for.
fn run_request(
    step: &StepSpec,
    program: AbsolutePath,
    live_output: LiveOutput,
) -> RunRequest {
    RunRequest {
        program,
        args: step.argv.iter().map(OsString::from).collect(),
        env: step
            .env
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        cwd: step.cwd.clone(),
        stdin: step.stdin.as_ref().map(|text| text.as_bytes().to_vec()),
        capture: step.capture,
        echo: live_output.echoes(step),
        tree: false,
        terminal: false,
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

impl<'a> TaskSteps<'a> {
    /// The step executor, which tells `events` when each step starts to run and
    /// when each comes from the cache, as `kizz run` asks for its live output.
    #[must_use]
    pub const fn with_events(
        mut self,
        events: &'a dyn StepEvents,
    ) -> Self {
        self.events = events;
        self
    }
}

impl TaskSteps<'_> {
    /// The step executor, which runs every step as if it were marked `always`, with
    /// the reason that names the task option, when `never` says that the task is
    /// marked `cache=never`. The option is in no step key.
    #[must_use]
    pub const fn with_cache_never(
        mut self,
        never: bool,
    ) -> Self {
        self.cache_never = never;
        self
    }

    /// The key of each step so far, in order.
    #[must_use]
    pub fn keys(&self) -> Vec<StepKey> {
        cells::cloned(&self.keys)
    }

    /// What the build decided about each step so far, in order: why each one ran,
    /// or that it was cached.
    #[must_use]
    pub fn decisions(&self) -> Vec<StepDecision> {
        cells::cloned(&self.decisions)
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
        let steps = u32::try_from(cells::update(&self.keys, |keys| keys.len())).unwrap_or(u32::MAX);
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
        step_lock(
            &self.engine.not_cached,
            NOT_CACHED,
            self.engine.lock_patience,
        )?
        .remove(key);
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
        step_lock(
            &self.engine.not_cached,
            NOT_CACHED,
            self.engine.lock_patience,
        )?
        .remove(&pending.key);
        Ok(())
    }

    /// Whether the step `step` runs as if it were marked `always`: it is, its task
    /// is marked `cache=never`, or the engine runs every step (`kizz run
    /// --no-cache`).
    const fn runs_always(
        &self,
        step: &StepSpec,
    ) -> bool {
        step.always || self.cache_never || self.engine.no_cache
    }

    /// Decide whether the step `step`, whose key is `key`, runs.
    fn decide(
        &self,
        key: StepKey,
        step: &StepSpec,
    ) -> Result<Decision, StepError> {
        let remembered = step_lock(
            &self.engine.not_cached,
            NOT_CACHED,
            self.engine.lock_patience,
        )?
        .get(&key)
        .cloned();
        if self.runs_always(step) {
            let known = remembered.is_some() || self.record(&key)?.is_some();
            if let Some(first) = self.engine.first_always_result(key)? {
                return Ok(Decision::Replay(first));
            }
            let reason = if !known {
                RunReason::NeverRun
            } else if self.cache_never {
                RunReason::TaskCacheNever
            } else if step.always {
                RunReason::Always
            } else {
                RunReason::RunNoCache
            };
            return Ok(Decision::Run(reason, None));
        }
        if let Some(run) = remembered {
            let checked = check(
                &run.observations,
                run.recorded_at_nanos,
                step.timestamps,
                &self.engine.states,
                self.engine.tracer,
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
            record.spec.timestamps,
            &self.engine.states,
            self.engine.tracer,
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
        let machine = self.changed_machine(key, &record)?;
        Ok(if checked.is_hit() && machine.is_empty() {
            Decision::Hit {
                record: Box::new(record),
                checked,
                checked_at_nanos,
            }
        } else {
            Decision::Run(
                RunReason::InputsChanged {
                    differences: checked.differences,
                    machine,
                },
                Some(Baseline::new(
                    &record.observations,
                    record.recorded_at_nanos,
                )),
            )
        })
    }

    /// The values of the machine state that the record `record`, stored under
    /// `key`, names and that changed since it was made, which the machine query of
    /// the tracer reads for the record's working directory.
    ///
    /// A record that names no part compares nothing and queries nothing. Nor does a
    /// record whose machine state the build already compared, or whose step already
    /// ran in the build: each step key is compared at most once per build.
    fn changed_machine(
        &self,
        key: StepKey,
        record: &StepRecord,
    ) -> Result<Vec<MachineChange>, StepError> {
        let parts = MachinePart::parts(&record.spec.machine);
        if parts.is_empty()
            || !step_lock(
                &self.engine.machine_compared,
                MACHINE_COMPARED,
                self.engine.lock_patience,
            )?
            .insert(key)
        {
            return Ok(Vec::new());
        }
        let current = machine_record(&self.engine.tracer.machine_state(&record.spec.cwd));
        Ok(machine_changes(&parts, &record.machine, &current))
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
            RunReason::TaskCacheNever => vec![Reason::TaskCacheNever],
            RunReason::RunNoCache => vec![Reason::RunNoCache],
            RunReason::InputsChanged {
                differences,
                machine,
            } => {
                let mut reasons = Vec::new();
                if !differences.is_empty() || machine.is_empty() {
                    reasons.push(Reason::InputsChanged {
                        inputs: changed_inputs(&txn, &context, differences)?,
                    });
                }
                if !machine.is_empty() {
                    reasons.push(Reason::MachineChanged {
                        changes: machine.clone(),
                    });
                }
                reasons
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

    /// Note that the step `step` at `position`, whose key is `key`, was cached and
    /// returns `result`, and tell the run's step events; its position is stored,
    /// and its task becomes an owner of the record under `key`, when the run
    /// finishes, unless it already ran in this build with the same key, whose
    /// reasons it keeps and whose record its run made it an owner of.
    fn note_cached(
        &self,
        position: u32,
        key: StepKey,
        step: &StepSpec,
        result: &Result<StepOutcome, StepError>,
    ) -> Result<(), StepError> {
        let decision = StepDecision {
            position,
            command: step.command(),
            verdict: Verdict::Cached,
        };
        self.events.step_cached(&decision, result);
        cells::update(&self.decisions, |decisions| decisions.push(decision));
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
        let cached = (
            PositionKey {
                task: identity.id.clone(),
                position,
            },
            stored,
        );
        cells::update(&self.cached_positions, |positions| positions.push(cached));
        Ok(())
    }

    /// Note that the step `step` of `pending` ran, warn when it re-ran in
    /// consecutive builds for the same self-inflicted reason, and tell the run's
    /// step events that it starts, before its command starts.
    fn note_ran(
        &self,
        step: &StepSpec,
        pending: &Pending,
    ) -> Result<(), StepError> {
        let command = step.command();
        if !pending.explanation.repeated.is_empty() {
            self.engine.warn(RepeatedWarning {
                task: self.identity.as_ref().map(TaskIdentity::label),
                position: pending.position,
                command: command.clone(),
                paths: pending.explanation.repeated.clone(),
            })?;
        }
        let decision = StepDecision {
            position: pending.position,
            command,
            verdict: Verdict::Ran(pending.explanation.clone()),
        };
        self.events.step_started(&decision);
        cells::update(&self.decisions, |decisions| decisions.push(decision));
        Ok(())
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
    /// the lookup checked, and `machine` the machine state the command starts with.
    fn run(
        &self,
        step: &StepSpec,
        pending: &Pending,
        program: AbsolutePath,
        checked: Accesses,
        machine: MachineStateRecord,
    ) -> Result<StepOutcome, StepError> {
        let request = run_request(step, program.clone(), self.engine.live_output);
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
                    first_reads: result.first_reads,
                    unknown_system_calls: result.unknown_system_calls.into_iter().collect(),
                    connections: stored_connections(&result.connections),
                    machine,
                };
                self.finish_run(step, pending, finished, why, Ok(outcome.clone()))?;
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
                    first_reads: FirstReads::new(),
                    unknown_system_calls: Vec::new(),
                    connections: Vec::new(),
                    machine,
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
                    first_reads: FirstReads::new(),
                    unknown_system_calls: Vec::new(),
                    connections: Vec::new(),
                    machine,
                };
                let why = NotCachedReason::StartFailed { message };
                self.finish_run(step, pending, finished, Some(why), Err(failure.clone()))?;
                Err(failure)
            },
            Err(error) => Err(StepError::new(&error)),
        }
    }

    /// Why the result `result` of a run is not cached, or `None` when it is.
    ///
    /// Every run that returns once cancellation has been requested is interrupted,
    /// however it ended: by a signal, or with an exit code that the command chose
    /// after it handled the interrupt. A run that the tracer reports as incompletely
    /// traced is not cached for that reason; otherwise a run that made system calls
    /// that the tracer does not know is not cached for them, unless the build caches
    /// such results.
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
        if let Some(reason) = &result.incomplete {
            return Some(NotCachedReason::Incomplete {
                detail: reason.to_string(),
            });
        }
        (!result.unknown_system_calls.is_empty() && !self.engine.cache_unknown_system_calls).then(
            || {
                NotCachedReason::UnknownSystemCalls {
                    numbers: result.unknown_system_calls.iter().copied().collect(),
                }
            },
        )
    }

    /// Record the state of every path `finished` touched, and store the record of
    /// `step`, whose result is `result`, with the owners of the record it replaces
    /// and its task.
    ///
    /// The result is not cached for the reason `why`, or, when there is none, when
    /// a file changed after the command first read it or ran it, as the comparison
    /// of the first-read metadata with the recorded states finds. Such a record is
    /// marked with the reason and replaces any older record of its key, so no older
    /// result is replayed, and the result is kept in memory for the rest of the
    /// build, with the states of the paths it touched. A run that made system calls
    /// that the tracer does not know, and a run that made connections, is kept for
    /// the build's warnings, with whether its result was cached.
    fn finish_run(
        &self,
        step: &StepSpec,
        pending: &Pending,
        mut finished: Finished,
        why: Option<NotCachedReason>,
        result: Result<StepOutcome, StepError>,
    ) -> Result<(), StepError> {
        let first_reads = mem::take(&mut finished.first_reads);
        let mut record = self.record_of(step, pending, finished);
        let why = why.or_else(|| {
            let files = changed_after_read(&record.observations, &first_reads);
            (!files.is_empty()).then_some(NotCachedReason::ChangedAfterRead { files })
        });
        if !record.unknown_system_calls.is_empty() {
            step_lock(
                &self.engine.unknown_calls,
                UNKNOWN_CALLS,
                self.engine.lock_patience,
            )?
            .push(UnknownCalls {
                key: pending.key,
                numbers: record.unknown_system_calls.clone(),
                cached: why.is_none(),
            });
        }
        if !record.connections.is_empty() {
            step_lock(
                &self.engine.connections,
                CONNECTIONS,
                self.engine.lock_patience,
            )?
            .push(StepConnections {
                key: pending.key,
                connections: record.connections.clone(),
                cached: why.is_none(),
            });
        }
        let Some(why) = why else {
            return self.put_run(pending, &mut record);
        };
        record.not_cached = Some(why.clone());
        self.put_run(pending, &mut record)?;
        let result = result.map(|outcome| {
            StepOutcome {
                cached: true,
                ..outcome
            }
        });
        step_lock(
            &self.engine.not_cached,
            NOT_CACHED,
            self.engine.lock_patience,
        )?
        .insert(pending.key, NotCachedRun {
            why,
            observations: record.observations,
            recorded_at_nanos: record.recorded_at_nanos,
            result,
        });
        Ok(())
    }

    /// Record the state of every path `finished` touched and store the record of
    /// `step`, a command that did not run, with the owners of the record it
    /// replaces and its task.
    fn store(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
    ) -> Result<(), StepError> {
        let mut record = self.record_of(step, pending, finished);
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
            timestamps: step.timestamps,
            expect: step.expect.clone(),
            occurrence,
            machine: step.machine.clone(),
        }
    }

    /// The record of `step`, which ended as `finished`, with the state of every path
    /// it touched, as a cached result until [`TaskSteps::finish_run`] marks it. Its
    /// owners are empty until [`TaskSteps::put_run`] stores it.
    fn record_of(
        &self,
        step: &StepSpec,
        pending: &Pending,
        finished: Finished,
    ) -> StepRecord {
        let recorded_at_nanos = now_nanos();
        let observed = observe(
            &finished.accesses,
            &pending.ignore,
            &pending.track,
            &self.engine.states,
            self.engine.tracer,
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
            not_cached: None,
            owners: BTreeSet::new(),
            unknown_system_calls: finished.unknown_system_calls,
            connections: finished.connections,
            machine: finished.machine,
        }
    }

    /// Look the program of `step`, whose first argument is `name`, up and run it,
    /// or record that it is missing or not executable.
    ///
    /// Right before it runs the command, or records that the program is missing,
    /// it queries the machine state that the command starts with, which the record
    /// keeps whether or not the step names any part of it; the build compares the
    /// state of this step key no more in this build.
    fn look_up_and_run(
        &self,
        step: &StepSpec,
        name: &str,
        pending: &Pending,
    ) -> Result<StepOutcome, StepError> {
        let lookup = find_program(name, &step.env, &step.cwd).map_err(StepError::new)?;
        step_lock(
            &self.engine.machine_compared,
            MACHINE_COMPARED,
            self.engine.lock_patience,
        )?
        .insert(pending.key);
        let machine = machine_record(&self.engine.tracer.machine_state(step.cwd.as_path()));
        let exit = match lookup {
            ProgramLookup::Found { program, checked } => {
                return self.run(step, pending, program, checked, machine);
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
            first_reads: FirstReads::new(),
            unknown_system_calls: Vec::new(),
            connections: Vec::new(),
            machine,
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
        let position =
            u32::try_from(cells::update(&self.keys, |keys| keys.len())).unwrap_or(u32::MAX);
        let (key, occurrence) = cells::update(&self.occurrences, |occurrences| {
            occurrences.next_key(self.program, step)
        })
        .map_err(StepError::new)?;
        cells::update(&self.keys, |keys| keys.push(key));
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
                self.note_cached(position, key, step, &result)?;
                return result;
            },
            Decision::Replay(result) => {
                self.note_cached(position, key, step, &result)?;
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
        self.note_ran(step, &pending)?;
        let result = self.look_up_and_run(step, name, &pending);
        if self.runs_always(step) {
            self.engine.finish_always(key, &result)?;
        }
        result
    }
}

/// Unit tests of the locks of the engine's shared state.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::{
        sync::Mutex,
        time::{Duration, Instant},
    };

    use kizz_threads::LOCK_PATIENCE;

    use super::{WARNINGS, lock, step_lock};

    /// The short patience of the tests, in place of the engine's default,
    /// [`LOCK_PATIENCE`], so a lock that stays held fails at once.
    const TEST_PATIENCE: Duration = Duration::from_millis(50);

    /// State that stays locked, here by the calling thread itself, where
    /// `Mutex::lock` may panic, is an error that names the state, for a reader and
    /// for a step, once the patience has passed; free state locks. The patience is
    /// the short one of the tests, which the engine's lock path takes as a
    /// parameter, so the two waits take a fraction of a second, not twice
    /// [`LOCK_PATIENCE`].
    #[test]
    fn state_that_stays_locked_is_an_error_of_the_reader_and_the_step() {
        let started = Instant::now();
        let state = Mutex::new(vec![1]);
        let held = lock(&state, WARNINGS, TEST_PATIENCE).unwrap();
        let read = lock(&state, WARNINGS, TEST_PATIENCE).unwrap_err();
        assert_eq!(read.what(), WARNINGS);
        assert!(read.waited() >= TEST_PATIENCE, "{read:?}");
        let step = step_lock(&state, WARNINGS, TEST_PATIENCE).unwrap_err();
        assert!(
            step.message
                .starts_with("could not lock the build's warnings for repeated reasons"),
            "{step}"
        );
        drop(held);
        assert_eq!(*step_lock(&state, WARNINGS, TEST_PATIENCE).unwrap(), [1]);
        assert!(
            started.elapsed() < LOCK_PATIENCE,
            "the test waited {:?}",
            started.elapsed()
        );
    }
}
