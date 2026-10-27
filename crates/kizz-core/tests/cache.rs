// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Cache checks, replay, `always` steps, results that are not cached, runs that
//! made system calls that the tracer does not know, and runs that made connections,
//! with a fake tracer and real files.
//!
//! Each test builds a project directory and a cache in temporary directories and
//! opens the store in that cache. A build is one [`Engine`]: a test runs a build,
//! changes files, and runs another build with a new engine on the same store. The
//! fake tracer performs scripted file operations on real files and reports them.
//! Tests observe hashing through the engine's count of hashed files, never through
//! timing, and observe whether a step ran through the requests the tracer received.
//!
//! Files that a test does not mean to be racily clean get a modification time an
//! hour in the past, so the two-second rule does not hash them.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs::{self, File, FileTimes},
    io,
    net::{Ipv6Addr, SocketAddr, SocketAddrV6},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    thread,
    time::{Duration, SystemTime},
};

use kizz_core::{
    Engine, PrintSink, ProgramHash, StepConnections, StepExecutor, StepOutcome, StepSpec, TaskInfo,
    TaskProgram, UnknownCalls, WorkerPool, observe::listing, parse_tasks, step_key,
};
use kizz_store::{
    AccessBits, Attribute, Attributes, Change, Connection as StoredConnection, ContentHash,
    Difference, DirEntry, EntryKind, FileMeta, Listing, NotCachedReason, Observation, Owner,
    PathState, RunReason, StepExit, StepKey, StepRecord, Store, Times,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, Connection,
    EchoSink, ExtendedAttributes, FirstReads, IncompleteReason, OutputStream, ProcessTree,
    RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The script of a fake tracer: what one run does and reports.
type Script = dyn Fn(&RunRequest) -> Result<RunResult, TraceError> + Send + Sync;

/// A tracer that runs nothing and follows a script that performs file operations
/// on real files and reports them, and whose attribute query answers with the
/// extended attributes that a test sets.
struct FakeTracer {
    /// What each run does.
    script: Box<Script>,
    /// Every request received, in order.
    requests: Mutex<Vec<RunRequest>>,
    /// The extended attributes of each path, by the path and whether a final link
    /// is followed; the query finds none for any other path that exists.
    attributes: Mutex<BTreeMap<(PathBuf, bool), ExtendedAttributes>>,
}

impl FakeTracer {
    /// A tracer that follows `script`.
    fn new(
        script: impl Fn(&RunRequest) -> Result<RunResult, TraceError> + Send + Sync + 'static
    ) -> Self {
        Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            attributes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Set the extended attributes of `path`, with or without following a final
    /// link, to `pairs` of a name and a value.
    fn set_attributes(
        &self,
        path: &Path,
        follow: bool,
        pairs: &[(&str, &str)],
    ) {
        let attributes = pairs
            .iter()
            .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
            .collect();
        self.attributes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((path.to_path_buf(), follow), attributes);
    }

    /// A tracer whose runs exit with status 0 and report reading `paths`.
    fn reading(paths: Vec<PathBuf>) -> Self {
        Self::new(move |request| {
            let accesses: Vec<_> = paths
                .iter()
                .map(|path| (path.clone(), AccessKind::Read))
                .collect();
            finished(request, &accesses, Termination::Exited(0))
        })
    }

    /// The number of requests received so far.
    fn runs(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The requests received so far.
    fn requests(&self) -> Vec<RunRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Tracer for FakeTracer {
    fn run(
        &self,
        request: &RunRequest,
        _echo: &dyn EchoSink,
        _cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        (self.script)(request)
    }

    fn extended_attributes(
        &self,
        path: &Path,
        follow: bool,
    ) -> io::Result<ExtendedAttributes> {
        let set = self
            .attributes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(path.to_path_buf(), follow))
            .cloned();
        match set {
            Some(attributes) => Ok(attributes),
            None => fs::symlink_metadata(path).map(|_| ExtendedAttributes::new()),
        }
    }

    /// These tests name no machine state, so the fake knows no part.
    fn machine_state(
        &self,
        _cwd: &Path,
    ) -> kizz_trace::MachineState {
        kizz_trace::MachineState::default()
    }
}

/// A tracer failure from `error`.
fn tracer_failure(error: &dyn Error) -> TraceError {
    TraceError::Tracer {
        message: error.to_string(),
        source: None,
    }
}

/// The accesses of a run of `request`: the program as run, then `accesses`.
///
/// # Errors
///
/// A tracer failure when a path in `accesses` is not absolute.
fn reported(
    request: &RunRequest,
    accesses: &[(PathBuf, AccessKind)],
) -> Result<Accesses, TraceError> {
    traced(request, accesses).map(|(reported, _)| reported)
}

/// The accesses of a run of `request`, the program as run, then `accesses`, with
/// the first reads of the run: the metadata of each file read or run before it was
/// written, noted now, when the script has done its file operations, under the
/// rules of the Linux tracer.
///
/// # Errors
///
/// A tracer failure when a path in `accesses` is not absolute.
fn traced(
    request: &RunRequest,
    accesses: &[(PathBuf, AccessKind)],
) -> Result<(Accesses, FirstReads), TraceError> {
    let mut reported = Accesses::new();
    let mut first_reads = FirstReads::new();
    first_reads.note_at(&request.program, &reported);
    reported.record(request.program.clone(), AccessKind::Exec);
    for (path, kind) in accesses {
        let path = AbsolutePath::new(path).map_err(|error| tracer_failure(&error))?;
        if matches!(kind, AccessKind::Read | AccessKind::Exec) {
            first_reads.note_at(&path, &reported);
        }
        reported.record(path, *kind);
    }
    Ok((reported, first_reads))
}

/// A result that reports the program as run, then `accesses`, with no output.
///
/// # Errors
///
/// A tracer failure when a path in `accesses` is not absolute.
fn finished(
    request: &RunRequest,
    accesses: &[(PathBuf, AccessKind)],
    termination: Termination,
) -> Result<RunResult, TraceError> {
    let (accesses, first_reads) = traced(request, accesses)?;
    Ok(RunResult {
        termination,
        output: CapturedOutput::empty(request.capture),
        tree: request
            .tree
            .then(|| ProcessTree::of_request(request, &accesses, Some(termination))),
        accesses,
        incomplete: None,
        first_reads,
        unknown_system_calls: BTreeSet::new(),
        connections: BTreeSet::new(),
    })
}

/// The second word of a request's argument vector, which these tests use to tell
/// steps apart.
fn verb(request: &RunRequest) -> &str {
    request
        .args
        .get(1)
        .and_then(|arg| arg.to_str())
        .unwrap_or_default()
}

/// Echoes nothing.
struct NoEcho;

impl EchoSink for NoEcho {
    fn echo(
        &self,
        _stream: OutputStream,
        _bytes: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }
}

/// Keeps printed lines.
#[derive(Default)]
struct Lines(Mutex<Vec<String>>);

impl Lines {
    /// The lines printed so far, and forget them.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl PrintSink for Lines {
    fn print_line(
        &self,
        line: &str,
    ) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line.to_owned());
    }
}

/// Set the modification time of `path` to `seconds` seconds ago.
fn age(
    path: &Path,
    seconds: u64,
) -> TestResult {
    let then = SystemTime::now()
        .checked_sub(Duration::from_secs(seconds))
        .ok_or("time out of range")?;
    File::options()
        .write(true)
        .open(path)?
        .set_times(FileTimes::new().set_modified(then))?;
    Ok(())
}

/// A project and a cache, each in a temporary directory, with an executable `tool`
/// in the project's `bin` directory whose modification time is an hour ago.
struct Fixture {
    /// The project directory, the task directory of every step.
    project: TempDir,
    /// The cache directory.
    cache: TempDir,
}

impl Fixture {
    /// A new fixture.
    fn new() -> TestResult<Self> {
        let fixture = Self {
            project: tempfile::tempdir()?,
            cache: tempfile::tempdir()?,
        };
        fixture.write_old("bin/tool", "#!/bin/sh\n")?;
        fs::set_permissions(fixture.path("bin/tool"), fs::Permissions::from_mode(0o755))?;
        Ok(fixture)
    }

    /// The project directory.
    fn dir(&self) -> TestResult<AbsolutePath> {
        Ok(AbsolutePath::new(self.project.path())?)
    }

    /// The absolute path of `relative` in the project.
    fn path(
        &self,
        relative: &str,
    ) -> PathBuf {
        self.project.path().join(relative)
    }

    /// Write `content` to `relative` in the project, creating parent directories.
    fn write(
        &self,
        relative: &str,
        content: &str,
    ) -> TestResult<PathBuf> {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(path)
    }

    /// Write `content` to `relative` in the project with a modification time an
    /// hour ago.
    fn write_old(
        &self,
        relative: &str,
        content: &str,
    ) -> TestResult<PathBuf> {
        let path = self.write(relative, content)?;
        age(&path, 3_600)?;
        Ok(path)
    }

    /// Open the store in the cache.
    fn store(&self) -> TestResult<Store> {
        Ok(Store::open(self.cache.path())?)
    }

    /// A step that runs `argv` in the project with `PATH` set to the project's
    /// `bin` directory.
    fn step(
        &self,
        argv: &[&str],
    ) -> TestResult<StepSpec> {
        Ok(StepSpec {
            argv: argv.iter().map(|&arg| arg.to_owned()).collect(),
            stdin: None,
            env: [(
                "PATH".to_owned(),
                self.path("bin").to_string_lossy().into_owned(),
            )]
            .into(),
            cwd: self.dir()?,
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            machine: Vec::new(),
            expect: None,
        })
    }

    /// Run `steps` in one build, a new engine with `tracer` and `cancel`, as one
    /// run of a task, and return each step's result and the files the build hashed.
    fn build_with(
        &self,
        tracer: &FakeTracer,
        cancel: Cancellation,
        steps: &[&StepSpec],
    ) -> TestResult<Built> {
        self.build_engine(tracer, cancel, false, steps)
    }

    /// Run `steps` in one build with `tracer`, whose engine caches the results of
    /// runs that made system calls that the tracer does not know, as
    /// `kizz build --cache-unknown-syscalls` asks.
    fn build_caching_unknown(
        &self,
        tracer: &FakeTracer,
        steps: &[&StepSpec],
    ) -> TestResult<Built> {
        self.build_engine(tracer, Cancellation::new(), true, steps)
    }

    /// Run `steps` in one build, a new engine with `tracer` and `cancel` that
    /// caches the results of runs with unknown system calls when `cache_unknown` is
    /// set, as one run of a task, and return what the build returned.
    fn build_engine(
        &self,
        tracer: &FakeTracer,
        cancel: Cancellation,
        cache_unknown: bool,
        steps: &[&StepSpec],
    ) -> TestResult<Built> {
        let store = self.store()?;
        let engine = Engine::new(tracer, &store, &NoEcho, cancel, None, &WorkerPool::new()?)
            .with_cache_unknown_system_calls(cache_unknown);
        let task = engine.task(PROGRAM, self.dir()?);
        let results = steps.iter().map(|step| task.execute(step)).collect();
        let built = Built {
            results,
            hashed: engine.files_hashed(),
            not_cached: engine.not_cached()?,
            unknown: engine.unknown_system_calls()?,
            connections: engine.connections()?,
        };
        drop(task);
        drop(engine);
        store.close()?;
        Ok(built)
    }

    /// Run `steps` in one build with `tracer`.
    fn build(
        &self,
        tracer: &FakeTracer,
        steps: &[&StepSpec],
    ) -> TestResult<Built> {
        self.build_with(tracer, Cancellation::new(), steps)
    }

    /// The record of `step`, the first occurrence in its task run.
    fn record(
        &self,
        step: &StepSpec,
    ) -> TestResult<Option<StepRecord>> {
        let store = self.store()?;
        let record = store
            .read()?
            .get::<StepRecord>(&step_key(PROGRAM, step, 0)?)?;
        store.close()?;
        Ok(record)
    }

    /// Every stored step record.
    fn records(&self) -> TestResult<Vec<(StepKey, StepRecord)>> {
        let store = self.store()?;
        let records = store.read()?.scan::<StepRecord>(b"")?;
        store.close()?;
        Ok(records)
    }
}

/// What one build returned.
struct Built {
    /// The result of each step, in order.
    results: Vec<Result<StepOutcome, kizz_core::StepError>>,
    /// The number of files the build hashed.
    hashed: u64,
    /// The steps whose results were not cached.
    not_cached: Vec<(StepKey, NotCachedReason)>,
    /// The runs that made system calls that the tracer does not know.
    unknown: Vec<UnknownCalls>,
    /// The runs that made connections.
    connections: Vec<StepConnections>,
}

impl Built {
    /// The outcome of step `index`.
    fn outcome(
        &self,
        index: usize,
    ) -> TestResult<StepOutcome> {
        match self.results.get(index) {
            Some(Ok(outcome)) => Ok(outcome.clone()),
            Some(Err(error)) => Err(error.message.clone().into()),
            None => Err(format!("no step {index}").into()),
        }
    }

    /// Whether step `index` came from the cache.
    fn cached(
        &self,
        index: usize,
    ) -> TestResult<bool> {
        Ok(self.outcome(index)?.cached)
    }
}

/// The program hash every step of these tests uses.
const PROGRAM: ProgramHash = ProgramHash::from_bytes([7; 32]);

/// The why of a record's reason, with its differences.
fn differences(record: &StepRecord) -> Vec<Difference> {
    match &record.reason {
        RunReason::InputsChanged { differences, .. } | RunReason::NotCached { differences, .. } => {
            differences.clone()
        },
        RunReason::NeverRun | RunReason::Always | RunReason::TaskCacheNever => Vec::new(),
    }
}

/// The difference of `path` with `change`.
fn difference(
    path: PathBuf,
    change: Change,
) -> Difference {
    Difference { path, change }
}

/// The observation of `path` in `record`.
fn observation<'r>(
    record: &'r StepRecord,
    path: &Path,
) -> TestResult<&'r Observation> {
    Ok(record
        .observations
        .iter()
        .find(|observation| observation.path == path)
        .ok_or_else(|| format!("no observation of {}", path.display()))?)
}

/// The state a regular file at `path` that was read has now.
fn read_state(path: &Path) -> TestResult<PathState> {
    let metadata = fs::metadata(path)?;
    let nanos = |seconds: i64, nanoseconds: i64| {
        seconds
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_add(nanoseconds))
            .ok_or("time out of range")
    };
    Ok(PathState::File {
        meta: FileMeta {
            size: metadata.size(),
            mtime_nanos: nanos(metadata.mtime(), metadata.mtime_nsec())?,
            ctime_nanos: nanos(metadata.ctime(), metadata.ctime_nsec())?,
            inode: metadata.ino(),
            device: metadata.dev(),
            mode: metadata.mode(),
            owner: Owner {
                uid: metadata.uid(),
                gid: metadata.gid(),
            },
        },
        hash: Some(ContentHash(*blake3::hash(&fs::read(path)?).as_bytes())),
    })
}

/// An unchanged step is a cache hit: nothing runs, nothing is hashed when the
/// metadata proves every file unchanged, and the record stays as it was.
#[test]
fn an_unchanged_step_is_a_cache_hit() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write_old("input.txt", "input")?;
    let tracer = FakeTracer::reading(vec![input]);
    let step = fixture.step(&["tool", "build"])?;

    let first = fixture.build(&tracer, &[&step])?;
    assert!(!first.cached(0)?);
    assert_eq!(first.hashed, 2);
    let recorded = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(recorded.reason, RunReason::NeverRun);

    let second = fixture.build(&tracer, &[&step])?;
    assert!(second.cached(0)?);
    assert_eq!(second.outcome(0)?.termination, Termination::Exited(0));
    assert_eq!(tracer.runs(), 1);
    assert_eq!(second.hashed, 0);
    assert_eq!(fixture.record(&step)?, Some(recorded));
    Ok(())
}

/// A size change is caught without hashing, and a same-size content change is
/// caught by hashing.
#[test]
fn content_changes_are_caught_by_size_or_by_hash() -> TestResult {
    let fixture = Fixture::new()?;
    let data = fixture.write_old("data.txt", "1234567")?;
    let step = fixture.step(&["tool"])?;
    fixture.build(&FakeTracer::reading(vec![data.clone()]), &[&step])?;
    // Later runs only check the file, so recording them hashes only the program,
    // and every other hash comes from the cache check.
    let reported = data.clone();
    let checking = FakeTracer::new(move |request| {
        finished(
            request,
            &[(reported.clone(), AccessKind::Probe)],
            Termination::Exited(0),
        )
    });

    fs::write(&data, "12345678")?;
    let sized = fixture.build(&checking, &[&step])?;
    assert!(!sized.cached(0)?);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(
        data.clone(),
        Change::Size { old: 7, new: 8 }
    )]);
    // Only the program, when the run was recorded: the size change needed no hash.
    assert_eq!(sized.hashed, 1);

    // A second step, whose record holds the hash of the file it read.
    let second = fixture.step(&["tool", "second"])?;
    fixture.build(&FakeTracer::reading(vec![data.clone()]), &[&second])?;
    fs::write(&data, "ABCDEFGH")?;
    let hashed = fixture.build(&checking, &[&second])?;
    assert!(!hashed.cached(0)?);
    let record = fixture.record(&second)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(data, Change::Content)]);
    // The changed file, by the check, and the program, when the run was recorded.
    assert_eq!(hashed.hashed, 2);
    Ok(())
}

/// A file that was read and touched, with unchanged content, is a cache hit; its
/// new metadata is stored, so the next check takes the fast path without hashing.
#[test]
fn a_touched_file_with_unchanged_content_is_a_hit_with_new_metadata() -> TestResult {
    let fixture = Fixture::new()?;
    let data = fixture.write_old("data.txt", "same")?;
    let tracer = FakeTracer::reading(vec![data.clone()]);
    let step = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[&step])?;
    let before = fixture.record(&step)?.ok_or("no record")?;

    age(&data, 7_200)?;
    let touched = fixture.build(&tracer, &[&step])?;
    assert!(touched.cached(0)?);
    assert_eq!(tracer.runs(), 1);
    assert_eq!(touched.hashed, 1);
    let after = fixture.record(&step)?.ok_or("no record")?;
    assert_ne!(
        observation(&after, &data)?.state,
        observation(&before, &data)?.state
    );
    assert_eq!(observation(&after, &data)?.state, read_state(&data)?);
    assert!(after.recorded_at_nanos > before.recorded_at_nanos);
    assert_eq!(after.reason, before.reason);
    assert_eq!(after.ran_at_nanos, before.ran_at_nanos);

    let again = fixture.build(&tracer, &[&step])?;
    assert!(again.cached(0)?);
    assert_eq!(again.hashed, 0);
    assert_eq!(fixture.record(&step)?, Some(after));
    Ok(())
}

/// A file whose recorded modification time is within two seconds of when its
/// record was written is hashed even when its metadata matches.
#[test]
fn the_two_second_rule_hashes_files_whose_metadata_matches() -> TestResult {
    let fixture = Fixture::new()?;
    let fresh = fixture.write("fresh.txt", "just written")?;
    let tracer = FakeTracer::reading(vec![fresh.clone()]);
    let step = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[&step])?;
    let recorded = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(observation(&recorded, &fresh)?.state, read_state(&fresh)?);

    let second = fixture.build(&tracer, &[&step])?;
    assert!(second.cached(0)?);
    assert_eq!(tracer.runs(), 1);
    // The metadata matched, but the racily clean file was hashed anyway, and the
    // program, whose modification time is an hour old, was not.
    assert_eq!(second.hashed, 1);
    Ok(())
}

/// A new entry in a listed directory re-runs the step, and so does a program that
/// appears earlier in `PATH`, where the lookup found nothing.
#[test]
fn a_new_entry_or_a_program_appearing_on_path_reruns_the_step() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.path("src"))?;
    fixture.write_old("src/a.c", "a")?;
    let listed = fixture.path("src");
    let reported = listed.clone();
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[(reported.clone(), AccessKind::List)],
            Termination::Exited(0),
        )
    });
    let mut step = fixture.step(&["tool"])?;
    step.env.insert(
        "PATH".to_owned(),
        format!(
            "{}:{}",
            fixture.path("first").display(),
            fixture.path("bin").display()
        ),
    );
    fixture.build(&tracer, &[&step])?;
    assert!(fixture.build(&tracer, &[&step])?.cached(0)?);

    fixture.write_old("src/b.c", "b")?;
    assert!(!fixture.build(&tracer, &[&step])?.cached(0)?);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(
        listed,
        Change::Entries {
            appeared: vec![DirEntry {
                name: b"b.c".to_vec(),
                kind: EntryKind::File,
            }],
            disappeared: Vec::new(),
            retyped: Vec::new(),
        }
    )]);

    fixture.write_old("first/tool", "#!/bin/sh\n")?;
    fs::set_permissions(
        fixture.path("first/tool"),
        fs::Permissions::from_mode(0o755),
    )?;
    assert!(!fixture.build(&tracer, &[&step])?.cached(0)?);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(
        fixture.path("first/tool"),
        Change::Appeared
    )]);
    let requests = tracer.requests();
    let last = requests.last().ok_or("no request")?;
    assert_eq!(last.program.as_path(), fixture.path("first/tool"));
    assert_eq!(tracer.runs(), 3);
    Ok(())
}

/// Run the task in `kizz.md` once as one build, with the caller environment
/// `caller`, and return whether its step came from the cache.
fn build_task(
    fixture: &Fixture,
    tracer: &FakeTracer,
    caller: &BTreeMap<String, String>,
) -> TestResult<bool> {
    let file = fixture.path("kizz.md");
    let markdown = fs::read_to_string(&file)?;
    let parsed = parse_tasks(&file, Path::new("kizz.md"), &markdown);
    let task = TaskInfo::new(parsed.tasks.first().ok_or("no task")?, &["ci".to_owned()])?;
    let program = TaskProgram::load(task, &Lines::default()).program?;
    let store = fixture.store()?;
    let engine = Engine::new(
        tracer,
        &store,
        &NoEcho,
        Cancellation::new(),
        None,
        &WorkerPool::new()?,
    );
    let steps = engine.task(program.hash(), fixture.dir()?);
    let lines = Lines::default();
    program
        .run(caller, &steps, &lines)
        .result
        .map_err(|failure| failure.to_string())?;
    drop(steps);
    drop(engine);
    store.close()?;
    let printed = lines.take();
    match printed.last().map(String::as_str) {
        Some(line) if line.ends_with("True") => Ok(true),
        Some(line) if line.ends_with("False") => Ok(false),
        _ => Err(format!("unexpected output: {printed:?}").into()),
    }
}

/// Changes to the environment, the arguments, the task block, or a loaded module
/// re-run the step, and an unchanged task is a cache hit.
#[test]
fn environment_argument_block_and_module_changes_rerun_the_step() -> TestResult {
    let fixture = Fixture::new()?;
    let block = r#"load("lib.star", "flag")
env.inherit("PATH", "MODE")
r = run("tool", flag(), env.caller("ARG", "a"))
print(r.cached)
"#;
    let markdown = |block: &str| format!("# Build\n\n```starlark ci\n{block}```\n");
    fixture.write("kizz.md", &markdown(block))?;
    fixture.write("lib.star", "def flag():\n    return \"--fast\"\n")?;
    let tracer = FakeTracer::reading(Vec::new());
    let mut caller: BTreeMap<String, String> = [
        (
            "PATH".to_owned(),
            fixture.path("bin").to_string_lossy().into_owned(),
        ),
        ("MODE".to_owned(), "debug".to_owned()),
    ]
    .into();

    assert!(!build_task(&fixture, &tracer, &caller)?);
    assert!(build_task(&fixture, &tracer, &caller)?);

    caller.insert("MODE".to_owned(), "release".to_owned());
    assert!(!build_task(&fixture, &tracer, &caller)?);
    assert!(build_task(&fixture, &tracer, &caller)?);

    caller.insert("ARG".to_owned(), "b".to_owned());
    assert!(!build_task(&fixture, &tracer, &caller)?);
    assert!(build_task(&fixture, &tracer, &caller)?);

    fixture.write("kizz.md", &markdown(&format!("# A comment.\n{block}")))?;
    assert!(!build_task(&fixture, &tracer, &caller)?);
    assert!(build_task(&fixture, &tracer, &caller)?);

    fixture.write("lib.star", "def flag():\n    return \"--fast\" # same\n")?;
    assert!(!build_task(&fixture, &tracer, &caller)?);
    assert!(build_task(&fixture, &tracer, &caller)?);

    let requests = tracer.requests();
    assert_eq!(requests.len(), 5);
    let last = requests.last().ok_or("no request")?;
    assert_eq!(last.args, ["tool", "--fast", "b"]);
    assert_eq!(
        last.env.get(std::ffi::OsStr::new("MODE")),
        Some(&std::ffi::OsString::from("release"))
    );
    Ok(())
}

/// An `always` step runs once per build and is recorded; when it rewrites a file
/// with identical content, the step that reads the file stays cached, and when it
/// changes the content, that step runs again.
#[test]
fn always_steps_with_identical_output_leave_dependents_cached() -> TestResult {
    let fixture = Fixture::new()?;
    let generated = fixture.path("generated.txt");
    let content = std::sync::Arc::new(Mutex::new("same".to_owned()));
    let (path, written) = (generated.clone(), content.clone());
    let tracer = FakeTracer::new(move |request| {
        let kind = if verb(request) == "generate" {
            let text = written
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            fs::write(&path, text).map_err(|error| tracer_failure(&error))?;
            AccessKind::Write
        } else {
            AccessKind::Read
        };
        finished(request, &[(path.clone(), kind)], Termination::Exited(0))
    });
    let mut generate = fixture.step(&["tool", "generate"])?;
    generate.always = true;
    let consume = fixture.step(&["tool", "consume"])?;

    fixture.build(&tracer, &[&generate, &consume])?;
    assert_eq!(tracer.runs(), 2);

    // The second build runs the `always` step once, even when a later pass checks
    // the task again, and the step that reads its output stays cached.
    let store = fixture.store()?;
    let engine = Engine::new(
        &tracer,
        &store,
        &NoEcho,
        Cancellation::new(),
        None,
        &WorkerPool::new()?,
    );
    for pass in 0..2 {
        let task = engine.task(PROGRAM, fixture.dir()?);
        assert_eq!(task.execute(&generate)?.cached, pass == 1, "pass {pass}");
        assert!(task.execute(&consume)?.cached, "pass {pass}");
    }
    drop(engine);
    store.close()?;
    assert_eq!(tracer.runs(), 3);
    let record = fixture.record(&generate)?.ok_or("no record")?;
    assert_eq!(record.reason, RunReason::Always);
    assert!(record.spec.always);
    let consumer = fixture.record(&consume)?.ok_or("no record")?;
    assert_eq!(
        observation(&consumer, &generated)?.state,
        read_state(&generated)?
    );

    *content.lock().unwrap_or_else(PoisonError::into_inner) = "new!".to_owned();
    let third = fixture.build(&tracer, &[&generate, &consume])?;
    assert!(!third.cached(0)?);
    assert!(!third.cached(1)?);
    assert_eq!(tracer.runs(), 5);
    let consumer = fixture.record(&consume)?.ok_or("no record")?;
    assert_eq!(differences(&consumer), [difference(
        generated,
        Change::Content
    )]);
    Ok(())
}

/// A file that was only checked is never hashed, and a change to its metadata
/// re-runs the step; a change of its permission bits is named as such.
#[test]
fn a_checked_file_is_never_hashed_and_its_metadata_change_reruns() -> TestResult {
    let fixture = Fixture::new()?;
    let config = fixture.write_old("config.toml", "checked = true")?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644))?;
    let reported = config.clone();
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[(reported.clone(), AccessKind::Probe)],
            Termination::Exited(0),
        )
    });
    let step = fixture.step(&["tool"])?;
    // Only the program is hashed, when the run is recorded.
    assert_eq!(fixture.build(&tracer, &[&step])?.hashed, 1);
    let unchanged = fixture.build(&tracer, &[&step])?;
    assert!(unchanged.cached(0)?);
    assert_eq!(unchanged.hashed, 0);

    age(&config, 7_200)?;
    let changed = fixture.build(&tracer, &[&step])?;
    assert!(!changed.cached(0)?);
    assert_eq!(changed.hashed, 1);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(
        config.clone(),
        Change::Metadata
    )]);
    let PathState::File { hash, .. } = &observation(&record, &config)?.state else {
        return Err("the checked file is not recorded as a file".into());
    };
    assert_eq!(*hash, None);

    fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
    let changed = fixture.build(&tracer, &[&step])?;
    assert!(!changed.cached(0)?);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(
        config.clone(),
        Change::Permissions {
            old: 0o644,
            new: 0o600,
        }
    )]);
    Ok(())
}

/// An access under `/proc` or `/sys` is recorded and never re-runs a step unless
/// a `track` pattern matches it.
#[test]
fn kernel_paths_rerun_a_step_only_when_tracked() -> TestResult {
    let fixture = Fixture::new()?;
    let uptime = PathBuf::from("/proc/uptime");
    let tracer = FakeTracer::reading(vec![
        uptime.clone(),
        PathBuf::from("/sys/kernel/kizz-test-absent"),
    ]);
    let untracked = fixture.step(&["tool", "untracked"])?;
    let mut tracked = fixture.step(&["tool", "tracked"])?;
    tracked.track.push("/proc/uptime".to_owned());
    fixture.build(&tracer, &[&untracked, &tracked])?;
    let record = fixture.record(&untracked)?.ok_or("no record")?;
    let observed = observation(&record, &uptime)?;
    assert!(!observed.tracked);
    assert_eq!(observed.state, PathState::Untracked);
    assert!(observation(&fixture.record(&tracked)?.ok_or("no record")?, &uptime)?.tracked);

    // `/proc/uptime` counts in hundredths of a second, so it changes.
    thread::sleep(Duration::from_millis(100));
    let second = fixture.build(&tracer, &[&untracked, &tracked])?;
    assert!(second.cached(0)?);
    assert!(!second.cached(1)?);
    let record = fixture.record(&tracked)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(uptime, Change::Content)]);
    Ok(())
}

/// Build `step` with `tracer` twice: the first build must run it again, with
/// `expected` as its differences when given, and the second must be a cache hit.
fn reruns_then_hits(
    fixture: &Fixture,
    tracer: &FakeTracer,
    step: &StepSpec,
    expected: Option<&[Difference]>,
) -> TestResult {
    let rerun = fixture.build(tracer, &[step])?;
    assert!(!rerun.cached(0)?, "the step was a hit");
    if let Some(expected) = expected {
        let record = fixture.record(step)?.ok_or("no record")?;
        assert_eq!(differences(&record), expected);
    }
    assert!(fixture.build(tracer, &[step])?.cached(0)?, "the step ran");
    Ok(())
}

/// Remove `path`, a link, a file, or a directory with what it holds.
fn remove(path: &Path) -> TestResult {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Replace whatever is at `link` with a symbolic link to `target`.
fn relink(
    target: &Path,
    link: &Path,
) -> TestResult {
    if fs::symlink_metadata(link).is_ok() {
        remove(link)?;
    }
    symlink(target, link)?;
    Ok(())
}

/// The difference of a link at `link` retargeted from `old` to `new`.
fn link_target(
    link: &Path,
    old: &Path,
    new: &Path,
) -> [Difference; 1] {
    [difference(link.to_path_buf(), Change::LinkTarget {
        old: old.to_path_buf(),
        new: new.to_path_buf(),
    })]
}

/// A step that reads a final link to `/proc/version` and a file through a directory
/// link to `/proc`, built once and then found a hit while the links are unchanged.
struct LinksIntoProc {
    /// The project and cache.
    fixture: Fixture,
    /// The tracer, which reports reading `file_link` and `through_dir`.
    tracer: FakeTracer,
    /// The step.
    step: StepSpec,
    /// A regular directory of the project, holding `regular`.
    inputs: PathBuf,
    /// A regular input file, `inputs/version`.
    regular: PathBuf,
    /// The final link, at first to `/proc/version`.
    file_link: PathBuf,
    /// The directory link, at first to `/proc`.
    dir_link: PathBuf,
    /// The path read through `dir_link`.
    through_dir: PathBuf,
}

impl LinksIntoProc {
    /// Build the step, prove that only the `/proc` objects are untracked and the
    /// links keep their targets, and prove the next build a hit.
    fn new() -> TestResult<Self> {
        let fixture = Fixture::new()?;
        let root = fs::canonicalize(fixture.dir()?.as_path())?;
        fixture.write_old("inputs/version", "regular version")?;
        let file_link = root.join("version-link");
        let dir_link = root.join("proc-link");
        let through_dir = dir_link.join("version");
        symlink("/proc/version", &file_link)?;
        symlink("/proc", &dir_link)?;
        let links = Self {
            tracer: FakeTracer::reading(vec![file_link.clone(), through_dir.clone()]),
            step: fixture.step(&["tool"])?,
            fixture,
            inputs: root.join("inputs"),
            regular: root.join("inputs/version"),
            file_link,
            dir_link,
            through_dir,
        };
        links.fixture.build(&links.tracer, &[&links.step])?;
        let record = links.fixture.record(&links.step)?.ok_or("no record")?;
        let into_proc = |target: &str| {
            PathState::Symlink {
                target: PathBuf::from(target),
                pointee: Box::new(PathState::Untracked),
            }
        };
        assert_eq!(
            observation(&record, &links.file_link)?.state,
            into_proc("/proc/version")
        );
        assert_eq!(
            observation(&record, &links.through_dir)?.state,
            PathState::Untracked
        );
        let probed = observation(&record, &links.dir_link)?;
        assert_eq!(probed.state, into_proc("/proc"));
        assert_eq!(probed.access, AccessBits(AccessBits::PROBE));
        let unchanged = links.fixture.build(&links.tracer, &[&links.step])?;
        assert!(unchanged.cached(0)?);
        assert_eq!(unchanged.hashed, 0);
        Ok(links)
    }

    /// Build the step twice: the first build must run it again, with `expected` as
    /// its differences when given, and the second must be a hit.
    fn reruns_then_hits(
        &self,
        expected: Option<&[Difference]>,
    ) -> TestResult {
        reruns_then_hits(&self.fixture, &self.tracer, &self.step, expected)
    }
}

/// A final project link to `/proc/version` stays an ordinary dependency, and only
/// `/proc/version` is untracked: retargeting it to a regular input and back,
/// removing it, restoring it, and replacing it with a regular file each run the step
/// again.
#[test]
fn a_final_link_into_proc_stays_a_dependency() -> TestResult {
    let links = LinksIntoProc::new()?;
    let proc_version = Path::new("/proc/version");
    let file_link = &links.file_link;
    relink(&links.regular, file_link)?;
    links.reruns_then_hits(Some(&link_target(file_link, proc_version, &links.regular)))?;
    relink(proc_version, file_link)?;
    links.reruns_then_hits(Some(&link_target(file_link, &links.regular, proc_version)))?;
    remove(file_link)?;
    links.reruns_then_hits(Some(&[difference(file_link.clone(), Change::Disappeared)]))?;
    relink(proc_version, file_link)?;
    links.reruns_then_hits(Some(&[difference(file_link.clone(), Change::Appeared)]))?;
    let replacement = file_link.with_extension("new");
    fs::write(&replacement, "replaced")?;
    fs::rename(replacement, file_link)?;
    links.reruns_then_hits(Some(&[difference(file_link.clone(), Change::Type)]))?;
    Ok(())
}

/// A project directory link to `/proc` stays an ordinary dependency, and only the
/// `/proc` object read through it is untracked: retargeting it to a regular
/// directory and back, removing it, restoring it, and replacing it with a regular
/// directory each run the step again.
#[test]
fn a_directory_link_into_proc_stays_a_dependency() -> TestResult {
    let links = LinksIntoProc::new()?;
    let proc = Path::new("/proc");
    let dir_link = &links.dir_link;
    relink(&links.inputs, dir_link)?;
    links.reruns_then_hits(Some(&link_target(dir_link, proc, &links.inputs)))?;
    // The recorded state of the file read through the link changes too.
    relink(proc, dir_link)?;
    links.reruns_then_hits(None)?;
    remove(dir_link)?;
    links.reruns_then_hits(Some(&[difference(dir_link.clone(), Change::Disappeared)]))?;
    relink(proc, dir_link)?;
    links.reruns_then_hits(None)?;
    remove(dir_link)?;
    fs::create_dir(dir_link)?;
    fs::write(&links.through_dir, "regular version")?;
    links.reruns_then_hits(Some(&[difference(dir_link.clone(), Change::Type)]))?;
    Ok(())
}

/// A hit returns the recorded exit status and raw output exactly, with `cached`
/// set, to the engine's caller and to a task program; a recorded missing program
/// fails again from the cache without running.
#[test]
fn a_hit_replays_the_exit_status_and_output_exactly() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::new(|request| {
        let mut result = finished(request, &[], Termination::Exited(3))?;
        result.output = match request.capture {
            CaptureMode::Merged => {
                CapturedOutput::Merged {
                    output: b"out \xff err\n".to_vec(),
                }
            },
            CaptureMode::Split => {
                CapturedOutput::Split {
                    stdout: b"\x00out".to_vec(),
                    stderr: b"err\xfe".to_vec(),
                }
            },
        };
        Ok(result)
    });
    let split = fixture.step(&["tool", "split"])?;
    let mut merged = fixture.step(&["tool", "merged"])?;
    merged.capture = CaptureMode::Merged;
    let mut missing = fixture.step(&["absent"])?;
    missing.env.insert(
        "PATH".to_owned(),
        fixture.path("nowhere").to_string_lossy().into_owned(),
    );

    let first = fixture.build(&tracer, &[&split, &merged, &missing])?;
    let second = fixture.build(&tracer, &[&split, &merged, &missing])?;
    assert_eq!(tracer.runs(), 2);
    for index in 0..2 {
        let (ran, replayed) = (first.outcome(index)?, second.outcome(index)?);
        assert!(!ran.cached && replayed.cached);
        assert_eq!(replayed.termination, Termination::Exited(3));
        assert_eq!(replayed.output, ran.output);
    }
    let failures: Vec<String> = [&first, &second]
        .iter()
        .map(|built| {
            built
                .outcome(2)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default()
        })
        .collect();
    assert!(
        failures[0].starts_with("program `absent` was not found"),
        "{failures:?}"
    );
    assert_eq!(failures[0], failures[1]);
    let record = fixture.record(&missing)?.ok_or("no record")?;
    assert_eq!(record.exit, StepExit::ProgramNotFound);
    assert_eq!(record.failure.as_deref(), Some(failures[0].as_str()));

    let block = "env.set(\"PATH\", task.dir + \"/bin\")\nr = run(\"tool\", \"merged\", capture = \
                 \"merged\")\nprint(r.rc, r.signal, r.cached)\nprint(r.output)\n";
    fixture.write(
        "kizz.md",
        &format!("# Show\n\n```starlark ci\n{block}```\n"),
    )?;
    let shown: Vec<Vec<String>> = (0..2)
        .map(|_| show(&fixture, &tracer))
        .collect::<TestResult<_>>()?;
    assert_eq!(tracer.runs(), 3);
    let replaced = format!("Show: out {} err", char::REPLACEMENT_CHARACTER);
    assert_eq!(shown[0], [
        "Show: 3 None False",
        replaced.as_str(),
        "Show: "
    ]);
    assert_eq!(shown[1], ["Show: 3 None True", replaced.as_str(), "Show: "]);
    Ok(())
}

/// Run the task in `kizz.md` once as one build and return what it printed.
fn show(
    fixture: &Fixture,
    tracer: &FakeTracer,
) -> TestResult<Vec<String>> {
    let file = fixture.path("kizz.md");
    let markdown = fs::read_to_string(&file)?;
    let parsed = parse_tasks(&file, Path::new("kizz.md"), &markdown);
    let task = TaskInfo::new(parsed.tasks.first().ok_or("no task")?, &["ci".to_owned()])?;
    let lines = Lines::default();
    let program = TaskProgram::load(task, &lines).program?;
    let store = fixture.store()?;
    let engine = Engine::new(
        tracer,
        &store,
        &NoEcho,
        Cancellation::new(),
        None,
        &WorkerPool::new()?,
    );
    let steps = engine.task(program.hash(), fixture.dir()?);
    program
        .run(&BTreeMap::new(), &steps, &lines)
        .result
        .map_err(|failure| failure.to_string())?;
    drop(steps);
    drop(engine);
    store.close()?;
    Ok(lines.take())
}

/// A tracer whose runs read `outcome` in the project and end as it says: `signal`,
/// `interrupt` (a signal stops the command), `handled` (the command handles the
/// interrupt and exits with code 130), `incomplete`, `start`, `unknown` (the command
/// makes the system calls 1,000,000 and 400, which the tracer does not know, and
/// 400 again), `incomplete-unknown` (both), `connect` (the command makes the
/// connections of [`connections_made`], one of them twice), `incomplete-connect`
/// (both), or anything else for exit status 0.
fn outcome_tracer(outcome: PathBuf) -> FakeTracer {
    FakeTracer::new(move |request| {
        let text = fs::read_to_string(&outcome).map_err(|error| tracer_failure(&error))?;
        let accesses = [(outcome.clone(), AccessKind::Read)];
        let termination = match text.as_str() {
            "signal" => Termination::Signaled(9),
            "interrupt" => Termination::Signaled(2),
            "handled" => Termination::Exited(130),
            _ => Termination::Exited(0),
        };
        if text == "start" {
            let touched = reported(request, &accesses)?;
            return Err(TraceError::StartFailed {
                program: request.program.clone(),
                source: io::Error::from(io::ErrorKind::OutOfMemory),
                tree: request
                    .tree
                    .then(|| ProcessTree::of_request(request, &touched, None)),
                accesses: touched,
            });
        }
        let mut result = finished(request, &accesses, termination)?;
        if text.starts_with("incomplete") {
            result.incomplete = Some(IncompleteReason::NonNativeSystemCall {
                detail: "int 0x80".to_owned(),
            });
        }
        if text.ends_with("unknown") {
            result.unknown_system_calls = [1_000_000, 400, 400].into_iter().collect();
        }
        if text.ends_with("connect") {
            result.connections = connections_made(&request.cwd);
        }
        Ok(result)
    })
}

/// Whether a run that ends as `kind` says, for the outcome tracer, runs after
/// cancellation was requested, as an interrupted command does.
fn cancelled(kind: &str) -> bool {
    matches!(kind, "interrupt" | "handled")
}

/// Why a run that ends as `kind` says, for the outcome tracer, is not cached, for
/// the program `program`.
fn not_cached_reason(
    kind: &str,
    program: &Path,
) -> NotCachedReason {
    match kind {
        "signal" => NotCachedReason::Signaled { signal: 9 },
        "interrupt" => NotCachedReason::Interrupted { signal: 2 },
        "handled" => NotCachedReason::InterruptedExited { code: 130 },
        "incomplete" | "incomplete-unknown" | "incomplete-connect" => {
            NotCachedReason::Incomplete {
                detail: "a system call used a non-native interface: int 0x80".to_owned(),
            }
        },
        "unknown" => {
            NotCachedReason::UnknownSystemCalls {
                numbers: UNKNOWN.to_vec(),
            }
        },
        _ => {
            NotCachedReason::StartFailed {
                message: format!(
                    "could not start {}: {}",
                    program.display(),
                    io::Error::from(io::ErrorKind::OutOfMemory)
                ),
            }
        },
    }
}

/// Each kind of result that is not cached leaves only a record marked with why,
/// which is never a cache hit. Within the build, the step is not run again while
/// nothing it touched changed, and runs again when something did; each new build
/// runs it again, with that stated as the reason. With cancellation requested, a
/// run that exits with a code after it handled the interrupt is such a result, like
/// one that a signal stopped.
#[test]
fn results_that_are_not_cached_leave_only_a_marker() -> TestResult {
    for kind in [
        "signal",
        "interrupt",
        "handled",
        "incomplete",
        "start",
        "unknown",
    ] {
        let fixture = Fixture::new()?;
        let why = not_cached_reason(kind, &fixture.path("bin/tool"));
        let outcome = fixture.write_old("outcome", kind)?;
        let tracer = outcome_tracer(outcome.clone());
        let step = fixture.step(&["tool"])?;
        let cancel = Cancellation::new();
        if cancelled(kind) {
            cancel.cancel();
        }
        let store = fixture.store()?;
        let engine = Engine::new(&tracer, &store, &NoEcho, cancel, None, &WorkerPool::new()?);
        let first = engine.task(PROGRAM, fixture.dir()?).execute(&step);
        let again = engine.task(PROGRAM, fixture.dir()?).execute(&step);
        assert_eq!(tracer.runs(), 1, "{kind}");
        let key = step_key(PROGRAM, &step, 0)?;
        assert_eq!(engine.not_cached()?, [(key, why.clone())], "{kind}");
        let stored = store.read()?.scan::<StepRecord>(b"")?;
        assert_eq!(stored.len(), 1, "{kind}");
        let (stored_key, marker) = stored.first().ok_or("no marker")?;
        assert_eq!(*stored_key, key, "{kind}");
        assert_eq!(marker.not_cached, Some(why.clone()), "{kind}");
        assert_eq!(marker.reason, RunReason::NeverRun, "{kind}");
        match (first, again) {
            (Ok(first), Ok(again)) => {
                assert!(!first.cached && again.cached, "{kind}");
                assert_eq!(first.termination, again.termination, "{kind}");
            },
            (Err(first), Err(again)) => {
                assert_eq!(first, again, "{kind}");
                assert_eq!(marker.exit, StepExit::NotStarted, "{kind}");
                assert_eq!(marker.failure.as_deref(), Some(first.message.as_str()));
            },
            other => return Err(format!("{kind}: {other:?}").into()),
        }

        // With cancellation still requested, the rerun returns once cancellation has
        // been requested, so it is interrupted too, even though it exits with 0.
        let rerun_why = cancelled(kind).then_some(NotCachedReason::InterruptedExited { code: 0 });
        fs::write(&outcome, "ok")?;
        let rerun = engine.task(PROGRAM, fixture.dir()?).execute(&step)?;
        assert!(!rerun.cached, "{kind}");
        assert_eq!(tracer.runs(), 2, "{kind}");
        let rerun_not_cached: Vec<_> = rerun_why.iter().map(|why| (key, why.clone())).collect();
        assert_eq!(engine.not_cached()?, rerun_not_cached, "{kind}");
        drop(engine);
        store.close()?;
        let record = fixture.record(&step)?.ok_or("no record")?;
        assert_eq!(record.not_cached, rerun_why, "{kind}");
        assert_eq!(record.reason, RunReason::NotCached {
            why: why.clone(),
            differences: vec![difference(outcome.clone(), Change::Size {
                old: u64::try_from(kind.len())?,
                new: 2,
            })],
        });

        // In each new build, the step runs again, and its result, which is not
        // cached, replaces the record of the cached run with a marker; the build
        // after that runs it again because of the marker.
        fs::write(&outcome, kind)?;
        for _ in 0..2 {
            let cancel = Cancellation::new();
            if cancelled(kind) {
                cancel.cancel();
            }
            let built = fixture.build_with(&tracer, cancel, &[&step])?;
            assert_eq!(built.not_cached, [(key, why.clone())], "{kind}");
        }
        assert_eq!(tracer.runs(), 4, "{kind}");
        let records = fixture.records()?;
        assert_eq!(records.len(), 1, "{kind}");
        let (_, marker) = records.first().ok_or("no marker")?;
        assert_eq!(marker.not_cached, Some(why.clone()), "{kind}");
        assert_eq!(marker.reason, RunReason::NotCached {
            why,
            differences: Vec::new(),
        });
    }
    Ok(())
}

/// After a run whose result is not cached, the older record of a cached run is
/// never replayed: with the input that cached run read restored, the next build
/// runs the step again, saying that the previous run could not be cached and why,
/// and only the build after that is a cache hit.
#[test]
fn an_older_record_is_never_replayed_after_a_result_that_is_not_cached() -> TestResult {
    for kind in [
        "signal",
        "interrupt",
        "handled",
        "incomplete",
        "start",
        "unknown",
    ] {
        let fixture = Fixture::new()?;
        let why = not_cached_reason(kind, &fixture.path("bin/tool"));
        let outcome = fixture.write_old("outcome", "ok")?;
        let tracer = outcome_tracer(outcome.clone());
        let step = fixture.step(&["tool"])?;

        let cached = fixture.build(&tracer, &[&step])?;
        assert!(!cached.cached(0)?, "{kind}");
        let success = fixture.record(&step)?.ok_or("no record")?;
        assert_eq!(success.not_cached, None, "{kind}");

        fs::write(&outcome, kind)?;
        let cancel = Cancellation::new();
        if cancelled(kind) {
            cancel.cancel();
        }
        let failed = fixture.build_with(&tracer, cancel, &[&step])?;
        assert_eq!(tracer.runs(), 2, "{kind}");
        assert_eq!(failed.not_cached.len(), 1, "{kind}");
        let marker = fixture.record(&step)?.ok_or("no marker")?;
        assert_eq!(marker.not_cached, Some(why.clone()), "{kind}");
        assert_ne!(marker, success, "{kind}");

        // The input the cached run read is back, with the same content, so the old
        // record's observations would all match: it must not be replayed.
        fs::write(&outcome, "ok")?;
        let restored = fixture.build(&tracer, &[&step])?;
        assert_eq!(tracer.runs(), 3, "{kind}");
        assert!(!restored.cached(0)?, "{kind}");
        assert_eq!(restored.not_cached, Vec::new(), "{kind}");
        let record = fixture.record(&step)?.ok_or("no record")?;
        assert_eq!(record.not_cached, None, "{kind}");
        assert_eq!(record.reason, RunReason::NotCached {
            why,
            differences: vec![difference(outcome.clone(), Change::Size {
                old: u64::try_from(kind.len())?,
                new: 2,
            })],
        });

        let hit = fixture.build(&tracer, &[&step])?;
        assert!(hit.cached(0)?, "{kind}");
        assert_eq!(tracer.runs(), 3, "{kind}");
    }
    Ok(())
}

/// The system calls that the outcome tracer's `unknown` runs make, sorted and
/// without repeats, as the tracer reports them.
const UNKNOWN: [u32; 2] = [400, 1_000_000];

/// Without the option, a run that made system calls that the tracer does not know
/// is not cached: its marker gives the reason that names each number and lists
/// them, the engine keeps the run for the warning as not cached, and the next build
/// runs the step again with that reason. A cache hit keeps nothing for a warning.
#[test]
fn a_run_with_unknown_system_calls_is_not_cached_without_the_option() -> TestResult {
    let fixture = Fixture::new()?;
    let outcome = fixture.write_old("outcome", "unknown")?;
    let tracer = outcome_tracer(outcome);
    let step = fixture.step(&["tool"])?;
    let key = step_key(PROGRAM, &step, 0)?;
    let why = NotCachedReason::UnknownSystemCalls {
        numbers: UNKNOWN.to_vec(),
    };

    let first = fixture.build(&tracer, &[&step])?;
    assert!(!first.cached(0)?);
    assert_eq!(first.not_cached, [(key, why.clone())]);
    assert_eq!(first.unknown, [UnknownCalls {
        key,
        numbers: UNKNOWN.to_vec(),
        cached: false,
    }]);
    let marker = fixture.record(&step)?.ok_or("no marker")?;
    assert_eq!(marker.not_cached, Some(why.clone()));
    assert_eq!(marker.unknown_system_calls, UNKNOWN);

    let second = fixture.build(&tracer, &[&step])?;
    assert!(!second.cached(0)?);
    assert_eq!(tracer.runs(), 2);
    let marker = fixture.record(&step)?.ok_or("no marker")?;
    assert_eq!(marker.reason, RunReason::NotCached {
        why: why.clone(),
        differences: Vec::new(),
    });
    assert_eq!(
        why.to_string(),
        "the command made system calls 400 and 1000000, which the tracer does not know"
    );
    Ok(())
}

/// With the option, a run that made system calls that the tracer does not know is
/// cached as if it had made none: its record lists the numbers and has no marker,
/// and the engine keeps the run for the warning as cached. A later build without
/// the option finds the record a cache hit, and keeps nothing for a warning.
#[test]
fn with_the_option_a_run_with_unknown_system_calls_is_cached() -> TestResult {
    let fixture = Fixture::new()?;
    let outcome = fixture.write_old("outcome", "unknown")?;
    let tracer = outcome_tracer(outcome);
    let step = fixture.step(&["tool"])?;
    let key = step_key(PROGRAM, &step, 0)?;

    let first = fixture.build_caching_unknown(&tracer, &[&step])?;
    assert!(!first.cached(0)?);
    assert_eq!(first.not_cached, Vec::new());
    assert_eq!(first.unknown, [UnknownCalls {
        key,
        numbers: UNKNOWN.to_vec(),
        cached: true,
    }]);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(record.not_cached, None);
    assert_eq!(record.unknown_system_calls, UNKNOWN);

    for later in [
        fixture.build(&tracer, &[&step])?,
        fixture.build_caching_unknown(&tracer, &[&step])?,
    ] {
        assert!(later.cached(0)?);
        assert_eq!(later.unknown, Vec::new());
        assert_eq!(later.not_cached, Vec::new());
    }
    assert_eq!(tracer.runs(), 1);
    Ok(())
}

/// A run that is also incompletely traced keeps that reason, with or without the
/// option, and its marker still lists the numbers; the engine keeps the run for the
/// warning as not cached.
#[test]
fn an_incomplete_run_with_unknown_system_calls_keeps_its_reason() -> TestResult {
    for cache_unknown in [false, true] {
        let fixture = Fixture::new()?;
        let outcome = fixture.write_old("outcome", "incomplete-unknown")?;
        let tracer = outcome_tracer(outcome);
        let step = fixture.step(&["tool"])?;
        let key = step_key(PROGRAM, &step, 0)?;
        let why = not_cached_reason("incomplete-unknown", &fixture.path("bin/tool"));

        let built = if cache_unknown {
            fixture.build_caching_unknown(&tracer, &[&step])?
        } else {
            fixture.build(&tracer, &[&step])?
        };
        assert_eq!(built.not_cached, [(key, why.clone())], "{cache_unknown}");
        assert_eq!(
            built.unknown,
            [UnknownCalls {
                key,
                numbers: UNKNOWN.to_vec(),
                cached: false,
            }],
            "{cache_unknown}"
        );
        let marker = fixture.record(&step)?.ok_or("no marker")?;
        assert_eq!(marker.not_cached, Some(why), "{cache_unknown}");
        assert_eq!(marker.unknown_system_calls, UNKNOWN, "{cache_unknown}");
    }
    Ok(())
}

/// The connections that the outcome tracer's `connect` runs make in the task
/// directory `dir`: the IPv4 address 127.0.0.1 on port 8080, twice, the IPv6
/// address `::1` on port 443, the Unix socket `run/s.sock` in `dir`, and the
/// abstract Unix socket `kizz` followed by a zero byte.
fn connections_made(dir: &AbsolutePath) -> BTreeSet<Connection> {
    [
        Connection::Ip(SocketAddr::from(([127, 0, 0, 1], 8080))),
        Connection::UnixAbstract(b"kizz\0".to_vec()),
        Connection::UnixPath(dir.join("run/s.sock")),
        Connection::Ip(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            443,
            0,
            0,
        ))),
        Connection::Ip(SocketAddr::from(([127, 0, 0, 1], 8080))),
    ]
    .into_iter()
    .collect()
}

/// The connections of [`connections_made`] in the project of `fixture`, as a record
/// keeps them: sorted and without repeats.
fn connections_recorded(fixture: &Fixture) -> Vec<StoredConnection> {
    vec![
        StoredConnection::Ipv4 {
            address: [127, 0, 0, 1],
            port: 8080,
        },
        StoredConnection::Ipv6 {
            address: Ipv6Addr::LOCALHOST.octets(),
            port: 443,
            scope: 0,
        },
        StoredConnection::UnixPath {
            path: fixture.path("run/s.sock"),
        },
        StoredConnection::UnixAbstract {
            name: b"kizz\0".to_vec(),
        },
    ]
}

/// A run that made connections is cached: its record lists each connection once,
/// sorted, and has no marker, and the engine keeps the run for the warning as
/// cached. The next build finds the record a cache hit and keeps nothing for a
/// warning, so a cache hit does not warn.
#[test]
fn a_run_with_connections_is_cached_and_warns_only_when_it_runs() -> TestResult {
    let fixture = Fixture::new()?;
    let outcome = fixture.write_old("outcome", "connect")?;
    let tracer = outcome_tracer(outcome);
    let step = fixture.step(&["tool"])?;
    let key = step_key(PROGRAM, &step, 0)?;

    let first = fixture.build(&tracer, &[&step])?;
    assert!(!first.cached(0)?);
    assert_eq!(first.not_cached, Vec::new());
    assert_eq!(first.unknown, Vec::new());
    assert_eq!(first.connections, [StepConnections {
        key,
        connections: connections_recorded(&fixture),
        cached: true,
    }]);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(record.not_cached, None);
    assert_eq!(record.connections, connections_recorded(&fixture));

    let second = fixture.build(&tracer, &[&step])?;
    assert!(second.cached(0)?);
    assert_eq!(second.connections, Vec::new());
    assert_eq!(tracer.runs(), 1);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(record.connections, connections_recorded(&fixture));
    Ok(())
}

/// A run that made connections and is also incompletely traced keeps that reason,
/// and its marker still lists the connections; the engine keeps the run for the
/// warning as not cached, and the next build runs it again and warns again.
#[test]
fn an_incomplete_run_with_connections_keeps_its_reason() -> TestResult {
    let fixture = Fixture::new()?;
    let outcome = fixture.write_old("outcome", "incomplete-connect")?;
    let tracer = outcome_tracer(outcome);
    let step = fixture.step(&["tool"])?;
    let key = step_key(PROGRAM, &step, 0)?;
    let why = not_cached_reason("incomplete-connect", &fixture.path("bin/tool"));

    for build in 1..=2 {
        let built = fixture.build(&tracer, &[&step])?;
        assert!(!built.cached(0)?, "{build}");
        assert_eq!(built.not_cached, [(key, why.clone())], "{build}");
        assert_eq!(
            built.connections,
            [StepConnections {
                key,
                connections: connections_recorded(&fixture),
                cached: false,
            }],
            "{build}"
        );
        let marker = fixture.record(&step)?.ok_or("no marker")?;
        assert_eq!(marker.not_cached, Some(why.clone()), "{build}");
        assert_eq!(
            marker.connections,
            connections_recorded(&fixture),
            "{build}"
        );
    }
    assert_eq!(tracer.runs(), 2);
    Ok(())
}

/// An `always` step runs at most once per build, when a pass first reaches it: every
/// later pass of the same build reuses its result without running it, even after a
/// path it touched changed, whether that result was cached or not; the next build
/// runs it again.
#[test]
fn an_always_step_runs_once_per_build_whatever_changes() -> TestResult {
    for kind in ["ok", "signal"] {
        let fixture = Fixture::new()?;
        let outcome = fixture.write_old("outcome", kind)?;
        let tracer = outcome_tracer(outcome.clone());
        let mut step = fixture.step(&["tool", "fetch"])?;
        step.always = true;
        let expected = if kind == "ok" {
            Termination::Exited(0)
        } else {
            Termination::Signaled(9)
        };

        for build in 1..=2 {
            let store = fixture.store()?;
            let engine = Engine::new(
                &tracer,
                &store,
                &NoEcho,
                Cancellation::new(),
                None,
                &WorkerPool::new()?,
            );
            for pass in 0..3 {
                let result = engine.task(PROGRAM, fixture.dir()?).execute(&step)?;
                assert_eq!(
                    result.cached,
                    pass > 0,
                    "{kind}, build {build}, pass {pass}"
                );
                assert_eq!(result.termination, expected, "{kind}, pass {pass}");
                // A path the step touched changes between passes.
                fs::write(&outcome, format!("{kind}{}", "!".repeat(pass + 1)))?;
                assert_eq!(tracer.runs(), build, "{kind}, build {build}, pass {pass}");
            }
            assert_eq!(
                engine.not_cached()?.len(),
                usize::from(kind == "signal"),
                "{kind}"
            );
            drop(engine);
            store.close()?;
            fs::write(&outcome, kind)?;
            let record = fixture.record(&step)?.ok_or("no record")?;
            let reason = if build == 1 {
                RunReason::NeverRun
            } else {
                RunReason::Always
            };
            assert_eq!(record.reason, reason, "{kind}, build {build}");
        }
    }
    Ok(())
}

/// A file that many steps read is hashed once per build, and hashed again after a
/// step records a write to it.
#[test]
fn a_shared_file_is_hashed_once_per_build_and_again_after_a_write() -> TestResult {
    let fixture = Fixture::new()?;
    let shared = fixture.write_old("shared.h", "int x;")?;
    let path = shared.clone();
    let tracer = FakeTracer::new(move |request| {
        let kind = if verb(request) == "write" {
            fs::write(&path, "int x;").map_err(|error| tracer_failure(&error))?;
            AccessKind::Write
        } else {
            AccessKind::Read
        };
        finished(request, &[(path.clone(), kind)], Termination::Exited(0))
    });
    let readers: Vec<StepSpec> = ["a", "b", "c"]
        .iter()
        .map(|name| fixture.step(&["tool", name]))
        .collect::<TestResult<_>>()?;
    let writer = fixture.step(&["tool", "write"])?;
    let after = fixture.step(&["tool", "d"])?;

    let store = fixture.store()?;
    let engine = Engine::new(
        &tracer,
        &store,
        &NoEcho,
        Cancellation::new(),
        None,
        &WorkerPool::new()?,
    );
    let steps = engine.task(PROGRAM, fixture.dir()?);
    for reader in &readers {
        steps.execute(reader)?;
    }
    // The program and the shared file, once each, for three steps.
    assert_eq!(engine.files_hashed(), 2);
    steps.execute(&writer)?;
    assert_eq!(engine.files_hashed(), 2);
    steps.execute(&after)?;
    assert_eq!(engine.files_hashed(), 3);
    drop(steps);
    drop(engine);
    store.close()?;

    // In a new build, the three readers find the shared file's metadata changed,
    // and it is hashed once for all three checks.
    let refs: Vec<&StepSpec> = readers.iter().collect();
    let checked = fixture.build(&tracer, &refs)?;
    for index in 0..3 {
        assert!(checked.cached(index)?);
    }
    assert_eq!(checked.hashed, 1);
    assert_eq!(tracer.runs(), 5);
    assert_eq!(checked.not_cached, Vec::new());
    Ok(())
}

/// A step that read a file is a cache hit until `chmod` changes the file's mode
/// from 0644 to 0600 with identical bytes: the step runs again, and its reason
/// names the permission change. When the times change with the mode, the mode is
/// still what is reported, and nothing refreshes it away.
#[test]
fn a_permission_change_with_identical_bytes_runs_a_step_that_read_the_file() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write_old("input.txt", "same bytes")?;
    fs::set_permissions(&input, fs::Permissions::from_mode(0o644))?;
    let tracer = FakeTracer::reading(vec![input.clone()]);
    let step = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[&step])?;
    assert!(fixture.build(&tracer, &[&step])?.cached(0)?);

    fs::set_permissions(&input, fs::Permissions::from_mode(0o600))?;
    reruns_then_hits(
        &fixture,
        &tracer,
        &step,
        Some(&[difference(input.clone(), Change::Permissions {
            old: 0o644,
            new: 0o600,
        })]),
    )?;

    // A new modification time and a new mode: the mode is reported, not refreshed.
    age(&input, 7_200)?;
    fs::set_permissions(&input, fs::Permissions::from_mode(0o640))?;
    reruns_then_hits(
        &fixture,
        &tracer,
        &step,
        Some(&[difference(input.clone(), Change::Permissions {
            old: 0o600,
            new: 0o640,
        })]),
    )?;
    assert_eq!(tracer.runs(), 3);
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(observation(&record, &input)?.state, read_state(&input)?);
    Ok(())
}

/// A step that ran a program runs again after the program loses its execute bits,
/// the review's first example: its reason names the permission change of the
/// program, and the step fails because the program is not executable.
#[test]
fn a_program_that_loses_its_execute_bits_runs_its_step_again() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::reading(Vec::new());
    let step = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[&step])?;
    assert!(fixture.build(&tracer, &[&step])?.cached(0)?);

    let program = fixture.path("bin/tool");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o644))?;
    let built = fixture.build(&tracer, &[&step])?;
    let Some(Err(error)) = built.results.first() else {
        return Err("the step did not fail".into());
    };
    assert!(error.message.contains("not executable"), "{error:?}");
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(record.exit, StepExit::ProgramNotExecutable);
    assert_eq!(differences(&record), [difference(
        program,
        Change::Permissions {
            old: 0o755,
            new: 0o644,
        }
    )]);
    // The lookup found no executable program, so nothing was traced.
    assert_eq!(tracer.runs(), 1);
    Ok(())
}

/// Replace the owner that `state` records, a regular file's or a directory's, with
/// `owner`.
fn set_owner(
    state: &mut PathState,
    owner: Owner,
) -> TestResult {
    match state {
        PathState::File { meta, .. } => meta.owner = owner,
        PathState::Directory {
            owner: recorded, ..
        } => *recorded = owner,
        other => return Err(format!("no owner in {other:?}").into()),
    }
    Ok(())
}

/// A recorded owner that differs from the file's or the directory's current one is
/// a change, for a file that was read and for a directory that was only checked.
/// Tests cannot change owners, so the test writes the other owner into the stored
/// record.
#[test]
fn a_recorded_owner_that_differs_runs_the_step_again() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write_old("input.txt", "content")?;
    let dir = fixture.path("inputs");
    fs::create_dir(&dir)?;
    let (read, probed) = (input.clone(), dir.clone());
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[
                (read.clone(), AccessKind::Read),
                (probed.clone(), AccessKind::Probe),
            ],
            Termination::Exited(0),
        )
    });
    let step = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[&step])?;
    assert!(fixture.build(&tracer, &[&step])?.cached(0)?);

    let metadata = fs::metadata(&input)?;
    let current = Owner {
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    let other = Owner {
        uid: current.uid.wrapping_add(1),
        gid: current.gid.wrapping_add(1),
    };
    let key = step_key(PROGRAM, &step, 0)?;
    let mut record = fixture.record(&step)?.ok_or("no record")?;
    for observation in &mut record.observations {
        if observation.path == input || observation.path == dir {
            set_owner(&mut observation.state, other)?;
        }
    }
    let store = fixture.store()?;
    let mut txn = store.write()?;
    txn.put(&key, &record)?;
    txn.commit()?;
    store.close()?;

    let owner = Change::Owner {
        old: other,
        new: current,
    };
    reruns_then_hits(
        &fixture,
        &tracer,
        &step,
        Some(&[
            difference(input.clone(), owner.clone()),
            difference(dir.clone(), owner),
        ]),
    )?;
    let record = fixture.record(&step)?.ok_or("no record")?;
    assert_eq!(observation(&record, &input)?.state, read_state(&input)?);
    let PathState::Directory { owner, mode, .. } = observation(&record, &dir)?.state else {
        return Err("the directory is not recorded as a directory".into());
    };
    assert_eq!(owner, current);
    assert_eq!(mode, fs::metadata(&dir)?.mode());
    Ok(())
}

/// Replace the listing that `state`, a listed directory, records with `new`.
fn set_listing(
    state: &mut PathState,
    new: Listing,
) -> TestResult {
    match state {
        PathState::Directory {
            listing: Some(recorded),
            ..
        } => {
            *recorded = new;
            Ok(())
        },
        other => Err(format!("no listing in {other:?}").into()),
    }
}

/// Replace the listing of `dir` in the stored record of `step` with the listing of
/// `entries`, in their order, with the hashes of that order.
fn store_listing(
    fixture: &Fixture,
    step: &StepSpec,
    dir: &Path,
    entries: &[DirEntry],
) -> TestResult {
    let key = step_key(PROGRAM, step, 0)?;
    let mut record = fixture.record(step)?.ok_or("no record")?;
    for observation in &mut record.observations {
        if observation.path == dir {
            set_listing(&mut observation.state, listing(entries.to_vec()))?;
        }
    }
    let store = fixture.store()?;
    let mut txn = store.write()?;
    txn.put(&key, &record)?;
    txn.commit()?;
    store.close()?;
    Ok(())
}

/// A recorded listing with the directory's current entries in reverse order runs
/// the step with an order change, and one that lacks a current entry runs it with
/// an entries change that names the entry. The test writes each listing, with the
/// hashes of its order, into the stored record.
#[test]
fn a_listing_in_another_order_or_without_an_entry_runs_the_step_again() -> TestResult {
    let fixture = Fixture::new()?;
    let listed = fixture.path("listed");
    for name in ["one", "two", "three", "four"] {
        fixture.write_old(&format!("listed/{name}"), name)?;
    }
    let reported = listed.clone();
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[(reported.clone(), AccessKind::List)],
            Termination::Exited(0),
        )
    });
    let step = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[&step])?;
    assert!(fixture.build(&tracer, &[&step])?.cached(0)?);
    let record = fixture.record(&step)?.ok_or("no record")?;
    let PathState::Directory {
        listing: Some(recorded),
        ..
    } = &observation(&record, &listed)?.state
    else {
        return Err("the directory is not recorded as listed".into());
    };
    let current = recorded.entries.clone().ok_or("no entries")?;
    assert_eq!(current.len(), 4);

    let reversed: Vec<DirEntry> = current.iter().rev().cloned().collect();
    store_listing(&fixture, &step, &listed, &reversed)?;
    reruns_then_hits(
        &fixture,
        &tracer,
        &step,
        Some(&[difference(listed.clone(), Change::Order)]),
    )?;

    let without_two: Vec<DirEntry> = current
        .iter()
        .filter(|entry| entry.name != b"two")
        .cloned()
        .collect();
    store_listing(&fixture, &step, &listed, &without_two)?;
    reruns_then_hits(
        &fixture,
        &tracer,
        &step,
        Some(&[difference(listed, Change::Entries {
            appeared: vec![DirEntry {
                name: b"two".to_vec(),
                kind: EntryKind::File,
            }],
            disappeared: Vec::new(),
            retyped: Vec::new(),
        })]),
    )?;
    assert_eq!(tracer.runs(), 3);
    Ok(())
}

/// A listed directory and a directory that was only checked each run their step
/// again after `chmod`, with the permission change as the reason.
#[test]
fn a_listed_or_checked_directory_runs_its_step_again_after_chmod() -> TestResult {
    let fixture = Fixture::new()?;
    let listed = fixture.path("listed");
    let checked = fixture.path("checked");
    for dir in [&listed, &checked] {
        fs::create_dir(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    }
    let (list, probe) = (listed.clone(), checked.clone());
    let tracer = FakeTracer::new(move |request| {
        let accesses = match verb(request) {
            "list" => [(list.clone(), AccessKind::List)],
            _ => [(probe.clone(), AccessKind::Probe)],
        };
        finished(request, &accesses, Termination::Exited(0))
    });
    let lists = fixture.step(&["tool", "list"])?;
    let checks = fixture.step(&["tool", "check"])?;
    fixture.build(&tracer, &[&lists, &checks])?;
    let hit = fixture.build(&tracer, &[&lists, &checks])?;
    assert!(hit.cached(0)? && hit.cached(1)?);

    let change = Change::Permissions {
        old: 0o755,
        new: 0o700,
    };
    for (dir, step, other) in [(&listed, &lists, &checks), (&checked, &checks, &lists)] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let built = fixture.build(&tracer, &[step, other])?;
        assert!(!built.cached(0)?, "{}", dir.display());
        assert!(built.cached(1)?, "{}", dir.display());
        let record = fixture.record(step)?.ok_or("no record")?;
        assert_eq!(differences(&record), [difference(
            dir.clone(),
            change.clone()
        )]);
        let again = fixture.build(&tracer, &[step, other])?;
        assert!(again.cached(0)? && again.cached(1)?);
    }
    assert_eq!(tracer.runs(), 4);
    Ok(())
}

/// Set the modification time of `path`, a file or a directory, to `seconds` seconds
/// ago, which also gives it a new change time.
fn set_modified(
    path: &Path,
    seconds: u64,
) -> TestResult {
    let then = SystemTime::now()
        .checked_sub(Duration::from_secs(seconds))
        .ok_or("time out of range")?;
    File::open(path)?.set_times(FileTimes::new().set_modified(then))?;
    Ok(())
}

/// The modification and change times that `state`, a regular file's or a
/// directory's, records.
fn recorded_times(state: &PathState) -> TestResult<Times> {
    match state {
        PathState::File { meta, .. } => Ok(meta.times()),
        PathState::Directory { times, .. } => Ok(*times),
        other => Err(format!("no times in {other:?}").into()),
    }
}

/// The current modification and change times of `path`.
fn current_times(path: &Path) -> TestResult<Times> {
    let metadata = fs::metadata(path)?;
    let nanos = |seconds: i64, nanoseconds: i64| {
        seconds
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_add(nanoseconds))
            .ok_or("time out of range")
    };
    Ok(Times {
        mtime_nanos: nanos(metadata.mtime(), metadata.mtime_nsec())?,
        ctime_nanos: nanos(metadata.ctime(), metadata.ctime_nsec())?,
    })
}

/// `step` with the `timestamps` option set.
fn stamped(step: &StepSpec) -> StepSpec {
    StepSpec {
        timestamps: true,
        ..step.clone()
    }
}

/// With `timestamps = True`, a read file whose modification time changes while its
/// bytes stay the same runs the step, without hashing, with a reason that names the
/// old and new times. The same step without the option is a hit that stores the new
/// metadata, and the two have different step keys.
#[test]
fn with_timestamps_a_new_modification_time_with_the_same_bytes_runs_the_step() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write_old("input.txt", "same bytes")?;
    let tracer = FakeTracer::reading(vec![input.clone()]);
    let plain = fixture.step(&["tool"])?;
    let stamped = stamped(&plain);
    assert_ne!(
        step_key(PROGRAM, &plain, 0)?,
        step_key(PROGRAM, &stamped, 0)?
    );
    fixture.build(&tracer, &[&plain, &stamped])?;
    let hit = fixture.build(&tracer, &[&plain, &stamped])?;
    assert!(hit.cached(0)? && hit.cached(1)?);
    assert_eq!(tracer.runs(), 2);

    let record = fixture.record(&stamped)?.ok_or("no record")?;
    let old = recorded_times(&observation(&record, &input)?.state)?;
    set_modified(&input, 7_200)?;
    let new = current_times(&input)?;
    assert_ne!(old.mtime_nanos, new.mtime_nanos);

    // The step with the option checks first, finds the times changed without
    // hashing, and runs; the step without it hashes the file and is a hit.
    let built = fixture.build(&tracer, &[&stamped, &plain])?;
    assert!(!built.cached(0)?);
    assert!(built.cached(1)?);
    assert_eq!(tracer.runs(), 3);
    let record = fixture.record(&stamped)?.ok_or("no record")?;
    assert_eq!(differences(&record), [difference(
        input.clone(),
        Change::Timestamps { old, new }
    )]);
    assert_eq!(observation(&record, &input)?.state, read_state(&input)?);
    let record = fixture.record(&plain)?.ok_or("no record")?;
    assert_eq!(observation(&record, &input)?.state, read_state(&input)?);
    assert_eq!(record.reason, RunReason::NeverRun);

    let again = fixture.build(&tracer, &[&plain, &stamped])?;
    assert!(again.cached(0)? && again.cached(1)?);
    assert_eq!(again.hashed, 0);
    assert_eq!(tracer.runs(), 3);
    Ok(())
}

/// With `timestamps = True`, a listed directory and a directory that was only
/// checked each run their step when their modification time changes, with the
/// times as the reason; without the option, both are hits.
#[test]
fn with_timestamps_a_listed_or_checked_directory_runs_when_its_time_changes() -> TestResult {
    let fixture = Fixture::new()?;
    let listed = fixture.path("listed");
    let checked = fixture.path("checked");
    for dir in [&listed, &checked] {
        fs::create_dir(dir)?;
    }
    let (list, probe) = (listed.clone(), checked.clone());
    let tracer = FakeTracer::new(move |request| {
        let accesses = match verb(request) {
            "list" => [(list.clone(), AccessKind::List)],
            _ => [(probe.clone(), AccessKind::Probe)],
        };
        finished(request, &accesses, Termination::Exited(0))
    });
    let lists = fixture.step(&["tool", "list"])?;
    let checks = fixture.step(&["tool", "check"])?;
    let (stamped_lists, stamped_checks) = (stamped(&lists), stamped(&checks));
    let all = [&lists, &checks, &stamped_lists, &stamped_checks];
    fixture.build(&tracer, &all)?;
    let hit = fixture.build(&tracer, &all)?;
    for index in 0..all.len() {
        assert!(hit.cached(index)?, "step {index}");
    }
    assert_eq!(tracer.runs(), 4);

    for (dir, step, index) in [(&listed, &stamped_lists, 2), (&checked, &stamped_checks, 3)] {
        let record = fixture.record(step)?.ok_or("no record")?;
        let old = recorded_times(&observation(&record, dir)?.state)?;
        set_modified(dir, 7_200)?;
        let new = current_times(dir)?;
        assert_ne!(old.mtime_nanos, new.mtime_nanos);
        let built = fixture.build(&tracer, &all)?;
        for other in 0..all.len() {
            assert_eq!(
                built.cached(other)?,
                other != index,
                "{} step {other}",
                dir.display()
            );
        }
        let record = fixture.record(step)?.ok_or("no record")?;
        assert_eq!(differences(&record), [difference(
            dir.clone(),
            Change::Timestamps { old, new }
        )]);
        assert_eq!(recorded_times(&observation(&record, dir)?.state)?, new);
        let again = fixture.build(&tracer, &all)?;
        for other in 0..all.len() {
            assert!(again.cached(other)?, "{} step {other}", dir.display());
        }
    }
    assert_eq!(tracer.runs(), 6);
    Ok(())
}

/// A step without the option refreshes a touched file's metadata on a hit; the same
/// step switched to the option does not reuse that record, under its own key, and
/// runs, and switching back is a hit on the first record.
#[test]
fn switching_the_timestamps_option_does_not_reuse_a_refreshed_record() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write_old("input.txt", "same bytes")?;
    let tracer = FakeTracer::reading(vec![input.clone()]);
    let plain = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[&plain])?;
    set_modified(&input, 7_200)?;
    let touched = fixture.build(&tracer, &[&plain])?;
    assert!(touched.cached(0)?);
    assert_eq!(touched.hashed, 1);
    let refreshed = fixture.record(&plain)?.ok_or("no record")?;
    assert_eq!(observation(&refreshed, &input)?.state, read_state(&input)?);

    let stamped = stamped(&plain);
    let switched = fixture.build(&tracer, &[&stamped])?;
    assert!(!switched.cached(0)?);
    assert_eq!(tracer.runs(), 2);
    let record = fixture.record(&stamped)?.ok_or("no record")?;
    assert!(record.spec.timestamps);
    assert_eq!(record.reason, RunReason::NeverRun);
    assert_eq!(fixture.records()?.len(), 2);
    assert!(!refreshed.spec.timestamps);

    let back = fixture.build(&tracer, &[&plain])?;
    assert!(back.cached(0)?);
    assert_eq!(tracer.runs(), 2);
    Ok(())
}

/// The attribute `name` with the hash of `value`, as a record holds it.
fn attribute(
    name: &str,
    value: &str,
) -> Attribute {
    Attribute {
        name: name.as_bytes().to_vec(),
        hash: ContentHash(*blake3::hash(value.as_bytes()).as_bytes()),
    }
}

/// A step whose command read the extended attributes of a file, of a directory, or
/// of what a link points to records them, through the fake attribute query, and
/// stays cached while they do not change; when one appears, disappears, or
/// changes value, the step runs again, and the reason names the attribute.
#[test]
fn a_step_runs_again_when_an_extended_attribute_it_read_changes() -> TestResult {
    let fixture = Fixture::new()?;
    let file = fixture.write_old("file.txt", "content")?;
    let dir = fixture.path("dir");
    fs::create_dir(&dir)?;
    fixture.write_old("target.txt", "pointee")?;
    let link = fixture.path("link");
    symlink("target.txt", &link)?;
    let (read_file, read_dir, read_link) = (file.clone(), dir.clone(), link.clone());
    let tracer = FakeTracer::new(move |request| {
        let path = match verb(request) {
            "file" => read_file.clone(),
            "dir" => read_dir.clone(),
            _ => read_link.clone(),
        };
        finished(
            request,
            &[(path, AccessKind::Xattr)],
            Termination::Exited(0),
        )
    });
    tracer.set_attributes(&file, false, &[("user.a", "1")]);
    tracer.set_attributes(&dir, false, &[("user.kept", "k"), ("user.gone", "g")]);
    tracer.set_attributes(&link, false, &[]);
    tracer.set_attributes(&link, true, &[("user.origin", "x")]);
    let steps = [
        fixture.step(&["tool", "file"])?,
        fixture.step(&["tool", "dir"])?,
        fixture.step(&["tool", "link"])?,
    ];
    let all: Vec<&StepSpec> = steps.iter().collect();
    fixture.build(&tracer, &all)?;
    let record = fixture.record(&steps[2])?.ok_or("no record")?;
    let observed = observation(&record, &link)?;
    assert!(observed.access.contains(AccessBits::XATTR));
    assert_eq!(
        observed.attributes,
        Some(Attributes {
            own: Some(Vec::new()),
            pointee: Some(vec![attribute("user.origin", "x")]),
        })
    );
    let hit = fixture.build(&tracer, &all)?;
    for index in 0..all.len() {
        assert!(hit.cached(index)?, "step {index}");
    }
    assert_eq!(tracer.runs(), 3);

    tracer.set_attributes(&file, false, &[("user.a", "1"), ("user.new", "n")]);
    tracer.set_attributes(&dir, false, &[("user.kept", "k")]);
    tracer.set_attributes(&link, true, &[("user.origin", "y")]);
    let built = fixture.build(&tracer, &all)?;
    for index in 0..all.len() {
        assert!(!built.cached(index)?, "step {index}");
    }
    assert_eq!(tracer.runs(), 6);
    let expected = [
        (&file, Change::Attributes {
            appeared: vec![attribute("user.new", "n")],
            disappeared: Vec::new(),
            changed: Vec::new(),
        }),
        (&dir, Change::Attributes {
            appeared: Vec::new(),
            disappeared: vec![attribute("user.gone", "g")],
            changed: Vec::new(),
        }),
        (&link, Change::Attributes {
            appeared: Vec::new(),
            disappeared: Vec::new(),
            changed: vec![attribute("user.origin", "y")],
        }),
    ];
    for (step, (path, change)) in steps.iter().zip(expected) {
        let record = fixture.record(step)?.ok_or("no record")?;
        assert_eq!(differences(&record), [difference(path.clone(), change)]);
    }
    let again = fixture.build(&tracer, &all)?;
    for index in 0..all.len() {
        assert!(again.cached(index)?, "step {index}");
    }
    assert_eq!(tracer.runs(), 6);
    Ok(())
}
