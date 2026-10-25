// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The engine side of the step executor, with a fake tracer and real files.
//!
//! Each test builds a project directory, a home directory, and a cache in temporary
//! directories, opens the store in that cache, and runs steps through an
//! [`Engine`] whose tracer is a fake: a script that performs file operations on real
//! files and reports them as the accesses of the run. Tests then read the step
//! records back from the store, and observe hashing through the engine's count of
//! hashed files, never through timing.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    os::unix::{
        fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _, symlink},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use kizz_core::{
    Engine, PrintSink, ProgramHash, StepError, StepExecutor, StepOutcome, StepSpec, TaskInfo,
    TaskProgram, find_program, parse_tasks, step_key,
};
use kizz_store::{
    AccessBits, Capture, ContentHash, EntryKind, FileMeta, Listing, MAX_LISTED_NAMES, Observation,
    Owner, PathState, ProgramPath, RecordedOutput, RunReason, SpecialKind, StepExit, StepKey,
    StepRecord, Store, Times,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    OutputStream, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The script of a fake tracer: what one run does and reports.
type Script = dyn Fn(&RunRequest) -> Result<RunResult, TraceError> + Send + Sync;

/// A tracer that runs nothing and follows a script that performs file operations
/// on real files and reports them.
struct FakeTracer {
    /// What each run does.
    script: Box<Script>,
    /// Every request received, in order.
    requests: Mutex<Vec<RunRequest>>,
}

impl FakeTracer {
    /// A tracer that follows `script`.
    fn new(
        script: impl Fn(&RunRequest) -> Result<RunResult, TraceError> + Send + Sync + 'static
    ) -> Self {
        Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// A tracer whose runs exit with status 0, write nothing, and report only the
    /// program as run.
    fn quiet() -> Self {
        Self::new(|request| finished(request, &[], Termination::Exited(0)))
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
    let mut reported = Accesses::new();
    reported.record(request.program.clone(), AccessKind::Exec);
    for (path, kind) in accesses {
        let path = AbsolutePath::new(path).map_err(|error| {
            TraceError::Tracer {
                message: error.to_string(),
                source: None,
            }
        })?;
        reported.record(path, *kind);
    }
    Ok(RunResult {
        termination,
        output: CapturedOutput::empty(request.capture),
        accesses: reported,
        incomplete: None,
    })
}

/// Echoes nothing.
struct NoEcho;

impl EchoSink for NoEcho {
    fn echo(
        &self,
        _stream: OutputStream,
        _bytes: &[u8],
    ) -> std::io::Result<()> {
        Ok(())
    }
}

/// Ignores printed lines.
struct NoPrint;

impl PrintSink for NoPrint {
    fn print_line(
        &self,
        _line: &str,
    ) {
    }
}

/// A project, a home directory, and a cache, each in a temporary directory, with an
/// executable `tool` in the project's `bin` directory.
struct Fixture {
    /// The project directory, the task directory of every step.
    project: TempDir,
    /// The project directory, as an absolute path.
    project_dir: AbsolutePath,
    /// The caller's home directory, kept for as long as the fixture lives.
    _home: TempDir,
    /// The caller's home directory, as an absolute path.
    home_dir: AbsolutePath,
    /// The cache directory.
    cache: TempDir,
}

impl Fixture {
    /// A new fixture.
    fn new() -> TestResult<Self> {
        let project = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let fixture = Self {
            project_dir: AbsolutePath::new(project.path())?,
            project,
            home_dir: AbsolutePath::new(home.path())?,
            _home: home,
            cache: tempfile::tempdir()?,
        };
        fixture.write("bin/tool", "#!/bin/sh\n")?;
        fixture.chmod("bin/tool", 0o755)?;
        Ok(fixture)
    }

    /// The project directory.
    fn dir(&self) -> AbsolutePath {
        self.project_dir.clone()
    }

    /// The project directory, as a plain path.
    fn root(&self) -> PathBuf {
        self.project.path().to_path_buf()
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

    /// Set the permission bits of `relative` in the project.
    fn chmod(
        &self,
        relative: &str,
        mode: u32,
    ) -> TestResult {
        fs::set_permissions(self.path(relative), fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    /// The caller's home directory.
    fn home(&self) -> AbsolutePath {
        self.home_dir.clone()
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
    ) -> StepSpec {
        StepSpec {
            argv: argv.iter().map(|&arg| arg.to_owned()).collect(),
            stdin: None,
            env: [(
                "PATH".to_owned(),
                self.path("bin").to_string_lossy().into_owned(),
            )]
            .into(),
            cwd: self.dir(),
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            expect: None,
        }
    }
}

/// The program hash every step of these tests uses, unless a test varies it.
const PROGRAM: ProgramHash = ProgramHash::from_bytes([7; 32]);

/// The record stored under `key`.
fn record(
    store: &Store,
    key: &StepKey,
) -> TestResult<StepRecord> {
    Ok(store
        .read()?
        .get::<StepRecord>(key)?
        .ok_or("no record for the step key")?)
}

/// Every stored step record.
fn records(store: &Store) -> TestResult<Vec<(StepKey, StepRecord)>> {
    Ok(store.read()?.scan::<StepRecord>(b"")?)
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

/// The listing recorded for the directory at `path` in `record`.
fn listing(
    record: &StepRecord,
    path: &Path,
) -> TestResult<Listing> {
    match &observation(record, path)?.state {
        PathState::Directory {
            listing: Some(listing),
            ..
        } => Ok(listing.clone()),
        other => Err(format!("{} has no listing: {other:?}", path.display()).into()),
    }
}

/// The BLAKE3 hash of the file at `path`.
fn content_hash(path: &Path) -> TestResult<ContentHash> {
    Ok(ContentHash(*blake3::hash(&fs::read(path)?).as_bytes()))
}

/// The modification and change times of the file or directory whose metadata is
/// `metadata`.
fn times(metadata: &fs::Metadata) -> TestResult<Times> {
    Ok(Times {
        mtime_nanos: metadata
            .mtime()
            .checked_mul(1_000_000_000)
            .and_then(|nanos| nanos.checked_add(metadata.mtime_nsec()))
            .ok_or("modification time out of range")?,
        ctime_nanos: metadata
            .ctime()
            .checked_mul(1_000_000_000)
            .and_then(|nanos| nanos.checked_add(metadata.ctime_nsec()))
            .ok_or("change time out of range")?,
    })
}

/// The state that a regular file at `path` should have, hashed or not.
fn file_state(
    path: &Path,
    hashed: bool,
) -> TestResult<PathState> {
    let metadata = fs::metadata(path)?;
    let Times {
        mtime_nanos,
        ctime_nanos,
    } = times(&metadata)?;
    Ok(PathState::File {
        meta: FileMeta {
            size: metadata.size(),
            mtime_nanos,
            ctime_nanos,
            inode: metadata.ino(),
            device: metadata.dev(),
            mode: metadata.mode(),
            owner: owner(&metadata),
        },
        hash: if hashed {
            Some(content_hash(path)?)
        } else {
            None
        },
    })
}

/// The owner of a file or directory whose metadata is `metadata`.
fn owner(metadata: &fs::Metadata) -> Owner {
    Owner {
        uid: metadata.uid(),
        gid: metadata.gid(),
    }
}

/// The state that the directory at `path` should have when it was not listed.
fn unlisted_dir_state(path: &Path) -> TestResult<PathState> {
    let metadata = fs::metadata(path)?;
    Ok(PathState::Directory {
        mode: metadata.mode(),
        owner: owner(&metadata),
        listing: None,
        times: times(&metadata)?,
    })
}

/// The same specification always gives the same key, each value that "Step keys"
/// lists changes it, `timestamps` included, and `always`, `stream`, and `expect` do
/// not.
#[test]
fn step_keys_cover_exactly_the_listed_values() -> TestResult {
    let fixture = Fixture::new()?;
    let base = fixture.step(&["tool", "build"]);
    let key = |program: ProgramHash, spec: &StepSpec, occurrence: u32| {
        step_key(program, spec, occurrence).unwrap()
    };
    let original = key(PROGRAM, &base, 0);
    assert_eq!(original, key(PROGRAM, &base.clone(), 0));

    let mut changed = BTreeMap::new();
    changed.insert(
        "program hash",
        key(ProgramHash::from_bytes([8; 32]), &base, 0),
    );
    let mut variant = base.clone();
    variant.cwd = fixture.dir().join("sub");
    changed.insert("working directory", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.argv.push("--release".to_owned());
    changed.insert("arguments", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.env.insert("MODE".to_owned(), "fast".to_owned());
    changed.insert("added variable", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant
        .env
        .insert("PATH".to_owned(), "/elsewhere".to_owned());
    changed.insert("changed variable", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.stdin = Some(String::new());
    changed.insert("empty stdin", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.stdin = Some("input".to_owned());
    changed.insert("stdin", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.capture = CaptureMode::Merged;
    changed.insert("capture mode", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.ignore.push("*.log".to_owned());
    changed.insert("ignore patterns", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.track.push("/proc/cpuinfo".to_owned());
    changed.insert("track patterns", key(PROGRAM, &variant, 0));
    let mut variant = base.clone();
    variant.timestamps = true;
    changed.insert("timestamps", key(PROGRAM, &variant, 0));
    changed.insert("occurrence", key(PROGRAM, &base, 1));

    let distinct: BTreeSet<StepKey> = changed.values().copied().collect();
    assert_eq!(distinct.len(), changed.len(), "{changed:?}");
    assert!(!distinct.contains(&original), "{changed:?}");

    let mut outside_the_key = base.clone();
    outside_the_key.always = true;
    outside_the_key.stream = true;
    outside_the_key.expect = Some(vec![0, 1]);
    assert_eq!(key(PROGRAM, &outside_the_key, 0), original);
    Ok(())
}

/// A command repeated in a task gets one key for each occurrence, and the count
/// starts again for each run of the task.
#[test]
fn a_repeated_command_gets_one_key_per_occurrence() -> TestResult {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    let tracer = FakeTracer::quiet();
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let repeated = fixture.step(&["tool", "again"]);
    let other = fixture.step(&["tool", "other"]);

    let steps = engine.task(PROGRAM, fixture.dir());
    for step in [&repeated, &other, &repeated] {
        steps.execute(step)?;
    }
    let keys = steps.keys();
    assert_eq!(keys, [
        step_key(PROGRAM, &repeated, 0)?,
        step_key(PROGRAM, &other, 0)?,
        step_key(PROGRAM, &repeated, 1)?,
    ]);
    assert_eq!(keys.iter().collect::<BTreeSet<_>>().len(), 3);
    assert_eq!(record(&store, &keys[0])?.spec.occurrence, 0);
    assert_eq!(record(&store, &keys[2])?.spec.occurrence, 1);

    let again = engine.task(PROGRAM, fixture.dir());
    again.execute(&repeated)?;
    assert_eq!(again.keys(), [keys[0]]);
    Ok(())
}

/// A task program run with the engine's executor gets one key per occurrence of a
/// repeated command, and each step's record holds the task program hash.
#[test]
fn task_programs_run_their_steps_through_the_engine() -> TestResult {
    let fixture = Fixture::new()?;
    let markdown = r#"# Build

```starlark ci
env.set("PATH", task.dir + "/bin")
for _ in range(2):
    run("tool", "x")
```
"#;
    let file = fixture.write("kizz.md", markdown)?;
    let parsed = parse_tasks(&file, Path::new("kizz.md"), markdown);
    let task = TaskInfo::new(parsed.tasks.first().ok_or("no task")?, &["ci".to_owned()])?;
    let program = TaskProgram::load(task, &NoPrint).program?;
    let hash = program.hash();

    let store = fixture.store()?;
    let tracer = FakeTracer::quiet();
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(hash, fixture.dir());
    program
        .run(&BTreeMap::new(), &steps, &NoPrint)
        .result
        .map_err(|failure| failure.to_string())?;

    let keys = steps.keys();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
    for (occurrence, key) in (0_u32..).zip(&keys) {
        let stored = record(&store, key)?;
        assert_eq!(stored.spec.occurrence, occurrence);
        assert_eq!(stored.spec.program_hash, ContentHash(*hash.as_bytes()));
        assert_eq!(stored.spec.argv, ["tool", "x"]);
    }
    Ok(())
}

/// The lookup goes through the step's `PATH` in order, records every location it
/// checks as a probe, stops at the first executable, and the request runs it.
#[test]
fn program_lookup_records_every_checked_location() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("first/tool", "not executable")?;
    fixture.chmod("first/tool", 0o644)?;
    fs::create_dir_all(fixture.path("second/tool"))?;
    fixture.write("later/tool", "#!/bin/sh\n")?;
    fixture.chmod("later/tool", 0o755)?;
    let store = fixture.store()?;
    let tracer = FakeTracer::quiet();
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);

    let mut step = fixture.step(&["tool"]);
    let search = ["first", "second", "missing", "bin", "later"]
        .map(|dir| fixture.path(dir).to_string_lossy().into_owned())
        .join(":");
    step.env.insert("PATH".to_owned(), search);
    let steps = engine.task(PROGRAM, fixture.dir());
    steps.execute(&step)?;

    let requests = tracer.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].program.as_path(), fixture.path("bin/tool"));
    assert_eq!(requests[0].args, ["tool"]);
    let stored = record(&store, &steps.keys()[0])?;
    assert_eq!(stored.program, Some(ProgramPath(fixture.path("bin/tool"))));
    let probe = AccessBits(AccessBits::PROBE);
    for (dir, state) in [
        ("first", file_state(&fixture.path("first/tool"), false)?),
        ("second", unlisted_dir_state(&fixture.path("second/tool"))?),
        ("missing", PathState::Absent),
    ] {
        let observed = observation(&stored, &fixture.path(&format!("{dir}/tool")))?;
        assert_eq!(observed.access, probe, "{dir}");
        assert_eq!(observed.state, state, "{dir}");
    }
    let program = observation(&stored, &fixture.path("bin/tool"))?;
    assert_eq!(
        program.access,
        AccessBits(AccessBits::PROBE | AccessBits::EXEC)
    );
    assert_eq!(program.state, file_state(&fixture.path("bin/tool"), true)?);
    assert!(observation(&stored, &fixture.path("later/tool")).is_err());

    let empty_and_relative = find_program(
        "tool",
        &[("PATH".to_owned(), ":bin".to_owned())].into(),
        &fixture.dir(),
    )?;
    let checked: Vec<PathBuf> = empty_and_relative
        .checked()
        .iter()
        .map(|(path, _)| path.as_path().to_path_buf())
        .collect();
    assert_eq!(checked, [fixture.path("bin/tool"), fixture.path("tool")]);
    Ok(())
}

/// A bare program name without `PATH` in the task environment is an error that
/// says to set or inherit `PATH`; nothing runs and nothing is recorded.
#[test]
fn a_bare_program_name_without_path_is_an_error() -> TestResult {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    let tracer = FakeTracer::quiet();
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let mut step = fixture.step(&["tool"]);
    step.env.clear();

    let error = engine
        .task(PROGRAM, fixture.dir())
        .execute(&step)
        .unwrap_err();
    assert!(error.message.contains("`tool`"), "{error}");
    assert!(error.message.contains("has no PATH"), "{error}");
    assert!(
        error.message.contains("env.set(\"PATH\", ...)")
            && error.message.contains("env.inherit(\"PATH\")"),
        "{error}"
    );
    assert_eq!(tracer.requests(), Vec::new());
    assert_eq!(records(&store)?, Vec::new());
    Ok(())
}

/// A missing program fails the task and is recorded like any other failure, with
/// every path checked; so is a program that is not executable.
#[test]
fn a_missing_or_non_executable_program_fails_and_is_recorded() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("plain/data", "not a program")?;
    fixture.chmod("plain/data", 0o644)?;
    let store = fixture.store()?;
    let tracer = FakeTracer::quiet();
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    let mut step = fixture.step(&["absent"]);
    let search = format!(
        "{}:{}",
        fixture.path("one").display(),
        fixture.path("two").display()
    );
    step.env.insert("PATH".to_owned(), search);

    let error = steps.execute(&step).unwrap_err();
    let one = fixture.path("one/absent");
    let two = fixture.path("two/absent");
    assert_eq!(
        error.message,
        format!(
            "program `absent` was not found in the task environment's PATH; checked {}, {}",
            one.display(),
            two.display()
        )
    );
    let stored = record(&store, &steps.keys()[0])?;
    assert_eq!(stored.exit, StepExit::ProgramNotFound);
    assert_eq!(stored.program, None);
    assert_eq!(stored.reason, RunReason::NeverRun);
    let observed: Vec<(&Path, &PathState)> = stored
        .observations
        .iter()
        .map(|observation| (observation.path.as_path(), &observation.state))
        .collect();
    assert_eq!(observed, [
        (one.as_path(), &PathState::Absent),
        (two.as_path(), &PathState::Absent)
    ]);

    let mut step = fixture.step(&["data"]);
    step.env.insert(
        "PATH".to_owned(),
        fixture.path("plain").to_string_lossy().into_owned(),
    );
    let error = steps.execute(&step).unwrap_err();
    assert!(error.message.contains("is not executable"), "{error}");
    let stored = record(&store, &steps.keys()[1])?;
    assert_eq!(stored.exit, StepExit::ProgramNotExecutable);
    assert_eq!(
        observation(&stored, &fixture.path("plain/data"))?.state,
        file_state(&fixture.path("plain/data"), false)?
    );
    assert_eq!(tracer.requests(), Vec::new());
    Ok(())
}

/// A program path with `/` resolves against the working directory, and when the
/// tracer reports it missing, the step is recorded with the tracer's accesses.
#[test]
fn a_program_path_the_tracer_cannot_find_is_recorded() -> TestResult {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    let tracer = FakeTracer::new(|request| {
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Probe);
        Err(TraceError::ProgramNotFound {
            program: request.program.clone(),
            accesses,
        })
    });
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    let error = steps.execute(&fixture.step(&["./out/./gone"])).unwrap_err();
    let gone = fixture.path("out/gone");
    assert_eq!(
        error.message,
        format!("program not found: {}", gone.display())
    );
    let stored = record(&store, &steps.keys()[0])?;
    assert_eq!(stored.exit, StepExit::ProgramNotFound);
    assert_eq!(stored.program, Some(ProgramPath(gone.clone())));
    assert_eq!(observation(&stored, &gone)?.state, PathState::Absent);
    Ok(())
}

/// Each kind of path gets the state "Path states" specifies, and only files that
/// were read or run are hashed.
#[test]
fn each_path_state_is_recorded_as_specified() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("read.txt", "read me")?;
    fixture.write("checked.txt", "only checked")?;
    fixture.write("listed/b.txt", "b")?;
    fs::create_dir_all(fixture.path("listed/a-dir"))?;
    symlink("b.txt", fixture.path("listed/c-link"))?;
    fs::create_dir_all(fixture.path("opened"))?;
    symlink("read.txt", fixture.path("link"))?;
    symlink("nowhere", fixture.path("dangling"))?;
    let _listener = UnixListener::bind(fixture.path("socket"))?;
    let project = fixture.root();
    let tracer = FakeTracer::new(move |request| {
        fs::write(project.join("written.txt"), "new content").unwrap();
        let accesses = [
            ("read.txt", AccessKind::Read),
            ("checked.txt", AccessKind::Probe),
            ("written.txt", AccessKind::Write),
            ("missing.txt", AccessKind::Probe),
            ("listed", AccessKind::List),
            ("opened", AccessKind::Probe),
            ("link", AccessKind::Read),
            ("dangling", AccessKind::Probe),
            ("socket", AccessKind::Probe),
        ]
        .map(|(relative, kind)| (project.join(relative), kind));
        let mut result = finished(request, &accesses, Termination::Exited(0))?;
        result
            .accesses
            .record(AbsolutePath::new("/dev/null").unwrap(), AccessKind::Write);
        Ok(result)
    });
    let store = fixture.store()?;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    steps.execute(&fixture.step(&["tool"]))?;
    let stored = record(&store, &steps.keys()[0])?;

    let state = |relative: &str| -> TestResult<PathState> {
        Ok(observation(&stored, &fixture.path(relative))?.state.clone())
    };
    assert_eq!(
        state("read.txt")?,
        file_state(&fixture.path("read.txt"), true)?
    );
    assert_eq!(
        state("checked.txt")?,
        file_state(&fixture.path("checked.txt"), false)?
    );
    assert_eq!(
        state("written.txt")?,
        file_state(&fixture.path("written.txt"), false)?
    );
    assert_eq!(state("missing.txt")?, PathState::Absent);
    assert_eq!(
        state("opened")?,
        unlisted_dir_state(&fixture.path("opened"))?
    );
    assert_eq!(state("link")?, PathState::Symlink {
        target: PathBuf::from("read.txt"),
        pointee: Box::new(file_state(&fixture.path("read.txt"), true)?),
    });
    assert_eq!(state("dangling")?, PathState::Symlink {
        target: PathBuf::from("nowhere"),
        pointee: Box::new(PathState::Absent),
    });
    assert_eq!(state("socket")?, PathState::Special {
        kind: SpecialKind::Socket,
    });
    assert_eq!(
        observation(&stored, Path::new("/dev/null"))?.state,
        PathState::Special {
            kind: SpecialKind::CharDevice,
        }
    );

    let entries: Vec<(Vec<u8>, EntryKind)> = listing(&stored, &fixture.path("listed"))?
        .entries
        .ok_or("the listed directory has no names")?
        .into_iter()
        .map(|entry| (entry.name, entry.kind))
        .collect();
    assert_eq!(entries, [
        (b"a-dir".to_vec(), EntryKind::Directory),
        (b"b.txt".to_vec(), EntryKind::File),
        (b"c-link".to_vec(), EntryKind::Symlink),
    ]);

    // The program, the read file, and the file behind the link: three files read
    // or run, so three hashed, and nothing else.
    assert_eq!(engine.files_hashed(), 3);
    let paths: Vec<&Path> = stored
        .observations
        .iter()
        .map(|observation| observation.path.as_path())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted);
    assert_eq!(
        observation(&stored, &fixture.path("read.txt"))?.access,
        AccessBits(AccessBits::READ)
    );
    Ok(())
}

/// A file that was only checked is never hashed, however often it is checked.
#[test]
fn a_file_that_was_only_checked_is_never_hashed() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("config.toml", "checked = true")?;
    let checked = fixture.path("config.toml");
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[
                (checked.clone(), AccessKind::Probe),
                (checked.clone(), AccessKind::Write),
            ],
            Termination::Exited(0),
        )
    });
    let store = fixture.store()?;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    for _ in 0..3 {
        steps.execute(&fixture.step(&["tool"]))?;
    }
    // Three runs, and only the program hashed, once in the build: the build's
    // cache of file states keeps its hash.
    assert_eq!(tracer.requests().len(), 3);
    assert_eq!(engine.files_hashed(), 1);
    for key in steps.keys() {
        let stored = record(&store, &key)?;
        assert_eq!(
            observation(&stored, &fixture.path("config.toml"))?.state,
            file_state(&fixture.path("config.toml"), false)?
        );
    }
    Ok(())
}

/// A large file that was read, which is hashed through a memory map with several
/// threads, gets the same hash as hashing its content directly.
#[test]
fn large_files_that_were_read_are_hashed() -> TestResult {
    let fixture = Fixture::new()?;
    let content = "0123456789".repeat(120_000);
    let large = fixture.write("large.bin", &content)?;
    let reported = large.clone();
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[(reported.clone(), AccessKind::Read)],
            Termination::Exited(0),
        )
    });
    let store = fixture.store()?;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    steps.execute(&fixture.step(&["tool"]))?;
    let stored = record(&store, &steps.keys()[0])?;
    assert!(fs::metadata(&large)?.len() > 1_000_000);
    assert_eq!(
        observation(&stored, &large)?.state,
        file_state(&large, true)?
    );
    Ok(())
}

/// A real FIFO that was read, checked, and written is recorded by type only, with
/// every reported access kind, and is never opened or hashed.
#[test]
fn a_fifo_is_recorded_by_type_only_and_never_hashed() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.path("pipes"))?;
    let fifo = fixture.path("pipes/fifo");
    // The standard library cannot make a FIFO, so the POSIX `mkfifo` command makes
    // it, with an explicit environment.
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .status()?;
    assert!(status.success(), "mkfifo failed: {status}");
    assert!(fs::symlink_metadata(&fifo)?.file_type().is_fifo());
    let reported = fifo.clone();
    let listed = fixture.path("pipes");
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[
                (reported.clone(), AccessKind::Read),
                (reported.clone(), AccessKind::Probe),
                (reported.clone(), AccessKind::Write),
                (listed.clone(), AccessKind::List),
            ],
            Termination::Exited(0),
        )
    });
    let store = fixture.store()?;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    steps.execute(&fixture.step(&["tool"]))?;
    // Only the program was hashed: the FIFO was read, but it is never opened or
    // hashed.
    assert_eq!(engine.files_hashed(), 1);

    let stored = record(&store, &steps.keys()[0])?;
    let observed = observation(&stored, &fifo)?;
    assert_eq!(observed.state, PathState::Special {
        kind: SpecialKind::Fifo,
    });
    assert_eq!(
        observed.access,
        AccessBits(AccessBits::READ | AccessBits::PROBE | AccessBits::WRITE)
    );
    assert!(!observed.tracked);
    let entries: Vec<(Vec<u8>, EntryKind)> = listing(&stored, &fixture.path("pipes"))?
        .entries
        .ok_or("the listed directory has no names")?
        .into_iter()
        .map(|entry| (entry.name, entry.kind))
        .collect();
    assert_eq!(entries, [(b"fifo".to_vec(), EntryKind::Fifo)]);
    Ok(())
}

/// A listed directory keeps its entry names up to 1,000 entries and leaves them out
/// above that, while its hash covers every entry.
#[test]
fn directory_entry_names_are_kept_up_to_the_limit() -> TestResult {
    let fixture = Fixture::new()?;
    for index in 0..MAX_LISTED_NAMES {
        fixture.write(&format!("at-limit/{index:04}"), "")?;
        fixture.write(&format!("over-limit/{index:04}"), "")?;
    }
    fixture.write("over-limit/extra", "")?;
    let project = fixture.root();
    let tracer = FakeTracer::new(move |request| {
        finished(
            request,
            &[
                (project.join("at-limit"), AccessKind::List),
                (project.join("over-limit"), AccessKind::List),
            ],
            Termination::Exited(0),
        )
    });
    let store = fixture.store()?;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    steps.execute(&fixture.step(&["tool"]))?;
    let stored = record(&store, &steps.keys()[0])?;

    let at_limit = listing(&stored, &fixture.path("at-limit"))?;
    let over_limit = listing(&stored, &fixture.path("over-limit"))?;
    let names = at_limit
        .entries
        .ok_or("the names at the limit were left out")?;
    assert_eq!(names.len(), MAX_LISTED_NAMES);
    assert_eq!(names[0].name, b"0000");
    assert_eq!(over_limit.entries, None);
    assert_ne!(at_limit.hash, over_limit.hash);
    Ok(())
}

/// Every access under `/proc` or `/sys` is recorded; an untracked one has no state,
/// and one that a `track` pattern matches is hashed.
#[test]
fn proc_and_sys_accesses_are_recorded_and_tracked_ones_hashed() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::new(|request| {
        finished(
            request,
            &[
                (PathBuf::from("/proc/version"), AccessKind::Read),
                (PathBuf::from("/proc/self/status"), AccessKind::Read),
                (
                    PathBuf::from("/sys/devices/system/cpu/online"),
                    AccessKind::Read,
                ),
            ],
            Termination::Exited(0),
        )
    });
    let store = fixture.store()?;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    let mut step = fixture.step(&["tool"]);
    step.track.push("/proc/vers*".to_owned());
    steps.execute(&step)?;
    let stored = record(&store, &steps.keys()[0])?;

    let version = observation(&stored, Path::new("/proc/version"))?;
    assert!(version.tracked);
    let PathState::File { hash, .. } = &version.state else {
        return Err(format!("/proc/version: {:?}", version.state).into());
    };
    assert_eq!(*hash, Some(content_hash(Path::new("/proc/version"))?));
    for untracked in ["/proc/self/status", "/sys/devices/system/cpu/online"] {
        let observed = observation(&stored, Path::new(untracked))?;
        assert!(!observed.tracked, "{untracked}");
        assert_eq!(observed.state, PathState::Untracked, "{untracked}");
        assert_eq!(observed.access, AccessBits(AccessBits::READ), "{untracked}");
    }
    // The program and the tracked file.
    assert_eq!(engine.files_hashed(), 2);
    Ok(())
}

/// A path that symbolic links lead into `/proc` or `/sys`, such as `/dev/fd/0`,
/// `/dev/stdin`, or a project link to `/proc/self/fd`, leaves only the `/proc` object
/// untracked, with no state read through kizz's own descriptors and nothing hashed:
/// a final link keeps its target with an untracked pointee, and each directory link
/// on the way is recorded as a probed link with its target. A regular file beside
/// them keeps its state.
#[test]
fn paths_that_links_lead_into_proc_or_sys_keep_only_their_links() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("plain.txt", "plain")?;
    symlink("/proc/self/fd", fixture.path("fd"))?;
    symlink("/proc/self/fd/0", fixture.path("stdin"))?;
    let linked = [
        PathBuf::from("/dev/fd/0"),
        PathBuf::from("/dev/stdin"),
        fixture.path("fd/0"),
        fixture.path("stdin"),
    ];
    let mut accesses: Vec<(PathBuf, AccessKind)> = linked
        .iter()
        .map(|path| (path.clone(), AccessKind::Read))
        .collect();
    accesses.push((fixture.path("plain.txt"), AccessKind::Read));
    let tracer =
        FakeTracer::new(move |request| finished(request, &accesses, Termination::Exited(0)));
    let store = fixture.store()?;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let steps = engine.task(PROGRAM, fixture.dir());
    steps.execute(&fixture.step(&["tool"]))?;
    let stored = record(&store, &steps.keys()[0])?;

    let link_into_proc = |link: &Path| -> TestResult<PathState> {
        Ok(PathState::Symlink {
            target: fs::read_link(link)?,
            pointee: Box::new(PathState::Untracked),
        })
    };
    let read = AccessBits(AccessBits::READ);
    let probe = AccessBits(AccessBits::PROBE);
    let expected = [
        (PathBuf::from("/dev/fd/0"), read, PathState::Untracked),
        (fixture.path("fd/0"), read, PathState::Untracked),
        (
            PathBuf::from("/dev/stdin"),
            read,
            link_into_proc(Path::new("/dev/stdin"))?,
        ),
        (
            fixture.path("stdin"),
            read,
            link_into_proc(&fixture.path("stdin"))?,
        ),
        (
            PathBuf::from("/dev/fd"),
            probe,
            link_into_proc(Path::new("/dev/fd"))?,
        ),
        (
            fixture.path("fd"),
            probe,
            link_into_proc(&fixture.path("fd"))?,
        ),
    ];
    for (path, access, state) in expected {
        let observed = observation(&stored, &path)?;
        assert!(!observed.tracked, "{}", path.display());
        assert_eq!(observed.access, access, "{}", path.display());
        assert_eq!(observed.state, state, "{}", path.display());
    }
    let plain = observation(&stored, &fixture.path("plain.txt"))?;
    assert!(
        matches!(plain.state, PathState::File { hash: Some(_), .. }),
        "{plain:?}"
    );
    assert!(
        stored
            .observations
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path),
        "the observations are not sorted by path: {:?}",
        stored.observations
    );
    // The program and the plain file.
    assert_eq!(engine.files_hashed(), 2);
    Ok(())
}

/// Each ignore pattern form leaves its matches out of the record and counts them:
/// a relative path, a relative glob, a `~/` glob, an absolute path, and a regular
/// expression.
#[test]
fn each_ignore_pattern_form_leaves_out_and_counts_its_matches() -> TestResult {
    let fixture = Fixture::new()?;
    let home = fixture.home();
    let project = fixture.root();
    let home_dir = home.as_path().to_path_buf();
    fixture.write("kept.txt", "kept")?;
    let tracer = FakeTracer::new(move |request| {
        let mut accesses: Vec<(PathBuf, AccessKind)> = [
            "kept.txt",
            "notes/log.txt",
            "build/a.o",
            "build/b.o",
            "build/deep/c.o",
            "cache.pyc",
        ]
        .iter()
        .map(|relative| (project.join(relative), AccessKind::Read))
        .collect();
        accesses.push((home_dir.join(".cache/go-build/ab/cd"), AccessKind::Read));
        accesses.push((PathBuf::from("/etc/kizz-test-absent"), AccessKind::Probe));
        finished(request, &accesses, Termination::Exited(0))
    });
    let store = fixture.store()?;
    let engine = Engine::new(
        &tracer,
        &store,
        &NoEcho,
        Cancellation::new(),
        Some(home.clone()),
    );
    let steps = engine.task(PROGRAM, fixture.dir());
    let mut step = fixture.step(&["tool"]);
    step.ignore = [
        "notes/./log.txt",
        "build/*.o",
        "~/.cache/go-build/**",
        "/etc/kizz-test-absent",
        r"re:\.pyc$",
    ]
    .map(str::to_owned)
    .to_vec();
    steps.execute(&step)?;
    let stored = record(&store, &steps.keys()[0])?;

    let observed: Vec<PathBuf> = stored
        .observations
        .iter()
        .map(|observation| observation.path.clone())
        .collect();
    assert_eq!(observed, [
        fixture.path("bin/tool"),
        fixture.path("build/deep/c.o"),
        fixture.path("kept.txt"),
    ]);
    assert_eq!(stored.ignored, 6);
    assert_eq!(stored.spec.ignore, step.ignore);

    let mut no_home = fixture.step(&["tool"]);
    no_home.ignore.push("~/x".to_owned());
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let error = engine
        .task(PROGRAM, fixture.dir())
        .execute(&no_home)
        .unwrap_err();
    assert!(error.message.contains("no HOME"), "{error}");
    Ok(())
}

/// A step record read back from the store, after the store is closed and opened
/// again, holds the full specification, the observations, the exit status, the raw
/// output, the duration, when the step ran, and why it ran.
#[test]
fn a_step_record_read_back_holds_every_field() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("input.txt", "input")?;
    let input = fixture.path("input.txt");
    let tracer = FakeTracer::new(move |request| {
        let mut result = finished(
            request,
            &[(input.clone(), AccessKind::Read)],
            Termination::Exited(3),
        )?;
        result.output = CapturedOutput::Merged {
            output: b"out \xff err\n".to_vec(),
        };
        Ok(result)
    });
    let mut step = fixture.step(&["tool", "--flag"]);
    step.stdin = Some("feed".to_owned());
    step.env.insert("MODE".to_owned(), "x".to_owned());
    step.capture = CaptureMode::Merged;
    step.stream = true;
    step.expect = Some(vec![0, 3]);
    step.ignore.push("*.tmp".to_owned());
    step.track.push("/proc/cpuinfo".to_owned());
    step.timestamps = true;

    let before = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let key = {
        let store = fixture.store()?;
        let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
        let steps = engine.task(PROGRAM, fixture.dir());
        let outcome: StepOutcome = steps.execute(&step)?;
        assert_eq!(outcome.termination, Termination::Exited(3));
        assert!(!outcome.cached);
        let key = steps.keys()[0];
        drop(steps);
        drop(engine);
        store.close()?;
        key
    };
    let after = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let request = tracer.requests().pop().ok_or("no request")?;
    assert_eq!(request.stdin.as_deref(), Some(&b"feed"[..]));
    assert!(request.echo);

    let store = fixture.store()?;
    let stored = record(&store, &key)?;
    let spec = &stored.spec;
    assert_eq!(spec.program_hash, ContentHash([7; 32]));
    assert_eq!(spec.cwd, fixture.root());
    assert_eq!(spec.argv, ["tool", "--flag"]);
    assert_eq!(spec.env, step.env);
    assert_eq!(spec.stdin.as_deref(), Some("feed"));
    assert_eq!(spec.capture, Capture::Merged);
    assert!(spec.stream && !spec.always);
    assert_eq!(spec.ignore, ["*.tmp"]);
    assert_eq!(spec.track, ["/proc/cpuinfo"]);
    assert!(spec.timestamps);
    assert_eq!(spec.expect, Some(vec![0, 3]));
    assert_eq!(spec.occurrence, 0);
    assert_eq!(stored.program, Some(ProgramPath(fixture.path("bin/tool"))));
    assert_eq!(
        observation(&stored, &fixture.path("input.txt"))?.state,
        file_state(&fixture.path("input.txt"), true)?
    );
    assert_eq!(stored.ignored, 0);
    assert_eq!(stored.exit, StepExit::Exited { code: 3 });
    assert_eq!(stored.output, RecordedOutput::Merged {
        output: b"out \xff err\n".to_vec(),
    });
    let ran_at = u128::from(stored.ran_at_nanos);
    assert!(before.as_nanos() <= ran_at && ran_at <= after.as_nanos());
    assert!(
        u128::from(stored.duration_nanos) <= after.as_nanos().saturating_sub(before.as_nanos())
    );
    assert_eq!(stored.reason, RunReason::NeverRun);

    // A new build with the same specification and unchanged inputs is a cache hit:
    // nothing runs, and the record keeps why the step last ran.
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let replayed = engine.task(PROGRAM, fixture.dir()).execute(&step)?;
    assert!(replayed.cached);
    assert_eq!(replayed.termination, Termination::Exited(3));
    assert_eq!(tracer.requests().len(), 1);
    assert_eq!(record(&store, &key)?.reason, RunReason::NeverRun);
    // `always` is not in the key, so the same record is found, and the step runs
    // in a new build for that reason.
    step.always = true;
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    engine.task(PROGRAM, fixture.dir()).execute(&step)?;
    assert_eq!(tracer.requests().len(), 2);
    assert_eq!(record(&store, &key)?.reason, RunReason::Always);
    Ok(())
}

/// A step the executor cannot run because its patterns are malformed fails without
/// a record.
#[test]
fn malformed_patterns_fail_the_step_without_a_record() -> TestResult {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    let tracer = FakeTracer::quiet();
    let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
    let mut step = fixture.step(&["tool"]);
    step.track.push("re:(".to_owned());
    let error: StepError = engine
        .task(PROGRAM, fixture.dir())
        .execute(&step)
        .unwrap_err();
    assert!(
        error
            .message
            .starts_with("run(): invalid regular expression"),
        "{error}"
    );
    assert_eq!(tracer.requests(), Vec::new());
    assert_eq!(records(&store)?, Vec::new());
    Ok(())
}
