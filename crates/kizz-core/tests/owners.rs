// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The owners of step records that the engine stores, and the path resets that
//! use them, with a fake tracer, real files, and a cache in temporary directories.
//!
//! Every build is one [`Engine`] on a store opened in the cache. Each task runs
//! through a [`TaskSteps`] made with [`Engine::task_for`] and ends with
//! [`TaskSteps::finish`], as the build loop runs it, so the tests check the owners
//! that the engine stores. After every build, every step position's task must own
//! the record that its key names. The fake tracer performs what a step's arguments
//! name and reports it:
//!
//! * `read:PATH` reads a file, relative to the step's working directory;
//! * `signal` makes a signal kill the command, so its result is not cached;
//!
//! and any other argument does nothing.

use std::{
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use kizz_core::{
    Engine, ProgramHash, ProgramParts, StepExecutor as _, StepSpec, TaskIdentity, TaskSteps,
    plan_reset,
};
use kizz_store::{
    EntryCounts, LearnedOrder, StepKey, StepPosition, StepRecord, Store, TaskId, TaskRecord,
    Verdict,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    FirstReads, OutputStream, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The program hash of every task: tasks with the same block share step keys.
const PROGRAM: [u8; 32] = [7; 32];

/// A tracer that runs nothing, reads the files a step's arguments name, noting their
/// metadata as first reads as the Linux tracer does, and counts its runs.
#[derive(Default)]
struct FakeTracer {
    /// The number of runs so far.
    runs: Mutex<usize>,
}

impl FakeTracer {
    /// The number of runs so far.
    fn runs(&self) -> usize {
        *self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Tracer for FakeTracer {
    fn run(
        &self,
        request: &RunRequest,
        _echo: &dyn EchoSink,
        _cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        {
            let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            *runs = runs.saturating_add(1);
        }
        let mut accesses = Accesses::new();
        let mut first_reads = FirstReads::new();
        first_reads.note_at(&request.program, &accesses);
        accesses.record(request.program.clone(), AccessKind::Exec);
        let mut termination = Termination::Exited(0);
        for arg in request.args.iter().skip(1) {
            let arg = arg.to_str().unwrap_or_default();
            if arg == "signal" {
                termination = Termination::Signaled(9);
            } else if let Some(path) = arg.strip_prefix("read:") {
                let path = request.cwd.as_path().join(path);
                let _content: Vec<u8> = fs::read(&path).unwrap_or_default();
                let path = AbsolutePath::new(path).map_err(|error| {
                    TraceError::Tracer {
                        message: error.to_string(),
                        source: None,
                    }
                })?;
                first_reads.note_at(&path, &accesses);
                accesses.record(path, AccessKind::Read);
            }
        }
        Ok(RunResult {
            termination,
            output: CapturedOutput::empty(request.capture),
            accesses,
            incomplete: None,
            first_reads,
        })
    }
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

/// What one build did, task by task, in the order the tasks ran.
struct Built {
    /// Whether each step of each task ran, rather than being cached.
    ran: Vec<Vec<bool>>,
    /// The key of each step of each task.
    keys: Vec<Vec<StepKey>>,
}

impl Built {
    /// Whether step `step` of task `task` ran.
    fn ran(
        &self,
        task: usize,
        step: usize,
    ) -> TestResult<bool> {
        Ok(*self
            .ran
            .get(task)
            .and_then(|steps| steps.get(step))
            .ok_or("no such step")?)
    }

    /// The key of step `step` of task `task`.
    fn key(
        &self,
        task: usize,
        step: usize,
    ) -> TestResult<StepKey> {
        Ok(*self
            .keys
            .get(task)
            .and_then(|keys| keys.get(step))
            .ok_or("no such step")?)
    }
}

/// Projects and a cache, each in a temporary directory, with an executable `tool`
/// in the `bin` directory.
struct Fixture {
    /// The directory of the projects.
    _dir: TempDir,
    /// The directory of the projects, with every symbolic link resolved.
    root: PathBuf,
    /// The cache.
    cache: TempDir,
}

impl Fixture {
    /// A new fixture.
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let fixture = Self {
            _dir: dir,
            root,
            cache: tempfile::tempdir()?,
        };
        fixture.write("bin/tool", "#!/bin/sh\n")?;
        fs::set_permissions(fixture.path("bin/tool"), fs::Permissions::from_mode(0o755))?;
        Ok(fixture)
    }

    /// The absolute path of `relative`.
    fn path(
        &self,
        relative: &str,
    ) -> PathBuf {
        self.root.join(relative)
    }

    /// Write `content` to `relative`, creating parent directories.
    fn write(
        &self,
        relative: &str,
        content: &str,
    ) -> TestResult {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(())
    }

    /// A step that runs `argv` in the absolute directory `cwd`, with `PATH` set to
    /// the `bin` directory.
    fn step(
        &self,
        argv: &[&str],
        cwd: &Path,
    ) -> TestResult<StepSpec> {
        Ok(StepSpec {
            argv: argv.iter().map(|&arg| arg.to_owned()).collect(),
            stdin: None,
            env: [(
                "PATH".to_owned(),
                self.path("bin").to_string_lossy().into_owned(),
            )]
            .into(),
            cwd: AbsolutePath::new(cwd)?,
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            expect: None,
        })
    }

    /// The task with index `index` in `<dir>/kizz.md`.
    fn task(
        &self,
        dir: &str,
        index: u32,
    ) -> TestResult<TaskIdentity> {
        Ok(TaskIdentity {
            id: TaskId {
                file: self.path(dir).join("kizz.md"),
                index,
            },
            line: 3,
            heading: None,
            targets: vec!["ci".to_owned()],
            program: ProgramHash::from_bytes(PROGRAM),
            parts: ProgramParts::default(),
            dir: AbsolutePath::new(self.path(dir))?,
        })
    }

    /// Open the store in the cache.
    fn store(&self) -> TestResult<Store> {
        Ok(Store::open(self.cache.path())?)
    }

    /// Run one build with `tracer`: each task with its steps, in order, finishing
    /// each task run as succeeded; then check that every step position's task owns
    /// the record that its key names.
    fn build(
        &self,
        tracer: &FakeTracer,
        tasks: &[(&TaskIdentity, &[&StepSpec])],
    ) -> TestResult<Built> {
        let store = self.store()?;
        let engine = Engine::new(tracer, &store, &NoEcho, Cancellation::new(), None);
        let mut built = Built {
            ran: Vec::new(),
            keys: Vec::new(),
        };
        for (identity, steps) in tasks {
            let run: TaskSteps<'_> = engine.task_for((*identity).clone());
            for step in *steps {
                let _result: Result<_, _> = run.execute(step);
            }
            run.finish(true)?;
            built.ran.push(
                run.decisions()
                    .iter()
                    .map(|decision| matches!(decision.verdict, Verdict::Ran(_)))
                    .collect(),
            );
            built.keys.push(run.keys());
        }
        drop(engine);
        positions_own_their_records(&store)?;
        store.close()?;
        Ok(built)
    }

    /// Run `step` once without a task identity, as only tests do, and return its
    /// key.
    fn run_without_identity(
        &self,
        tracer: &FakeTracer,
        step: &StepSpec,
    ) -> TestResult<StepKey> {
        let store = self.store()?;
        let engine = Engine::new(tracer, &store, &NoEcho, Cancellation::new(), None);
        let run = engine.task(
            ProgramHash::from_bytes(PROGRAM),
            AbsolutePath::new(&self.root)?,
        );
        let _result: Result<_, _> = run.execute(step);
        let key = *run.keys().first().ok_or("no step")?;
        drop(run);
        drop(engine);
        store.close()?;
        Ok(key)
    }

    /// The step record under `key`, if there is one.
    fn record(
        &self,
        key: &StepKey,
    ) -> TestResult<Option<StepRecord>> {
        let store = self.store()?;
        let record = store.read()?.get::<StepRecord>(key)?;
        store.close()?;
        Ok(record)
    }

    /// The owners of the step record under `key`, in order.
    fn owners(
        &self,
        key: &StepKey,
    ) -> TestResult<Vec<TaskId>> {
        let record = self.record(key)?.ok_or("no step record")?;
        Ok(record.owners.into_iter().collect())
    }
}

/// Fails unless every step position's task owns the record that its key names.
fn positions_own_their_records(store: &Store) -> TestResult {
    let txn = store.read()?;
    for (key, position) in txn.scan::<StepPosition>(&[])? {
        let record = txn
            .get::<StepRecord>(&position.step)?
            .ok_or_else(|| format!("no record for the position {key:?}"))?;
        assert!(
            record.owners.contains(&key.task),
            "{key:?} does not own its record: {:?}",
            record.owners
        );
    }
    Ok(())
}

/// The task identities of `identities`, in order.
fn ids(identities: &[&TaskIdentity]) -> Vec<TaskId> {
    identities
        .iter()
        .map(|identity| identity.id.clone())
        .collect()
}

/// A run makes its task an owner of the record it stores; a second task whose step
/// has the same key and is a hit becomes an owner too; a hit that refreshes the
/// record keeps its owners; a run that replaces the record because an input changed
/// keeps the earlier owners and adds its task; and a run without a task identity
/// adds no owner and keeps the others.
#[test]
fn runs_and_cache_hits_make_their_tasks_owners() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("in.txt", "one")?;
    let tracer = FakeTracer::default();
    let first = fixture.task("a", 0)?;
    let second = fixture.task("b", 0)?;
    let third = fixture.task("c", 0)?;
    let step = fixture.step(&["tool", "read:in.txt"], &fixture.root)?;

    let built = fixture.build(&tracer, &[(&first, &[&step]), (&second, &[&step])])?;
    assert!(built.ran(0, 0)?);
    assert!(!built.ran(1, 0)?);
    let key = built.key(0, 0)?;
    assert_eq!(built.key(1, 0)?, key);
    assert_eq!(fixture.owners(&key)?, ids(&[&first, &second]));

    // The same content written again is a hit that may refresh the observations,
    // and the owners stay.
    fixture.write("in.txt", "one")?;
    let built = fixture.build(&tracer, &[(&first, &[&step])])?;
    assert!(!built.ran(0, 0)?);
    assert_eq!(fixture.owners(&key)?, ids(&[&first, &second]));

    // A changed input: the third task's run replaces the record and keeps the
    // earlier owners.
    fixture.write("in.txt", "two")?;
    let built = fixture.build(&tracer, &[(&third, &[&step])])?;
    assert!(built.ran(0, 0)?);
    assert_eq!(built.key(0, 0)?, key);
    assert_eq!(fixture.owners(&key)?, ids(&[&first, &second, &third]));

    // A run without a task identity replaces the record and adds no owner.
    fixture.write("in.txt", "three")?;
    let runs = tracer.runs();
    assert_eq!(fixture.run_without_identity(&tracer, &step)?, key);
    assert_eq!(tracer.runs(), runs.saturating_add(1));
    assert_eq!(fixture.owners(&key)?, ids(&[&first, &second, &third]));
    Ok(())
}

/// The marker of a result that is not cached replaces the record of its key and
/// keeps the earlier owners; a second task that reuses that result within the build
/// becomes an owner; and so does a second task that reuses an `always` result.
#[test]
fn results_reused_within_a_build_make_their_tasks_owners() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let first = fixture.task("a", 0)?;
    let second = fixture.task("b", 0)?;
    let third = fixture.task("c", 0)?;
    let killed = fixture.step(&["tool", "signal"], &fixture.root)?;

    let built = fixture.build(&tracer, &[(&first, &[&killed])])?;
    let key = built.key(0, 0)?;
    assert!(built.ran(0, 0)?);
    assert_eq!(fixture.owners(&key)?, ids(&[&first]));

    // In the next build the marked record runs again, and the new marker keeps the
    // first owner; the third task reuses the result within the build.
    let built = fixture.build(&tracer, &[(&second, &[&killed]), (&third, &[&killed])])?;
    assert!(built.ran(0, 0)?);
    assert!(!built.ran(1, 0)?);
    let record = fixture.record(&key)?.ok_or("no step record")?;
    assert!(record.not_cached.is_some(), "{record:?}");
    assert_eq!(fixture.owners(&key)?, ids(&[&first, &second, &third]));

    let mut always = fixture.step(&["tool", "always"], &fixture.root)?;
    always.always = true;
    let built = fixture.build(&tracer, &[(&first, &[&always]), (&second, &[&always])])?;
    assert!(built.ran(0, 0)?);
    assert!(!built.ran(1, 0)?);
    let key = built.key(0, 0)?;
    assert_eq!(built.key(1, 0)?, key);
    assert_eq!(fixture.owners(&key)?, ids(&[&first, &second]));
    Ok(())
}

/// The tasks, steps, and step records of the reset test, after its history: a task
/// under `inside` whose step runs with an absolute `cwd` outside it, a task outside
/// it whose step runs with an absolute `cwd` inside it, each built in a first and
/// then a second configuration, a record shared by a task inside and a task outside,
/// and a record with no owner.
struct Scene {
    /// The directory that the test resets.
    inside: PathBuf,
    /// The directory under `inside` where the outside task's step runs.
    work: PathBuf,
    /// The task under `inside` whose step runs outside it.
    mine: TaskIdentity,
    /// The task under `inside` that shares a record.
    inside_shared: TaskIdentity,
    /// The task outside `inside` whose step runs inside it.
    theirs: TaskIdentity,
    /// The task outside `inside` that shares a record.
    outside_shared: TaskIdentity,
    /// The first configuration of the step of `mine`.
    mine_v1: StepSpec,
    /// The first configuration of the step of `theirs`.
    theirs_v1: StepSpec,
    /// The shared step.
    shared: StepSpec,
    /// The keys of the first and second configurations of `mine`.
    mine_keys: [StepKey; 2],
    /// The keys of the first and second configurations of `theirs`.
    theirs_keys: [StepKey; 2],
    /// The key of the shared record.
    shared_key: StepKey,
    /// The key of the record with no owner.
    unowned_key: StepKey,
}

impl Scene {
    /// Builds the history of the reset test with `tracer`, and gives the shared task
    /// under `inside` learned order.
    fn new(
        fixture: &Fixture,
        tracer: &FakeTracer,
    ) -> TestResult<Self> {
        let elsewhere = fixture.path("elsewhere");
        let work = fixture.path("inside/work");
        fixture.write("elsewhere/in.txt", "mine")?;
        fixture.write("inside/work/in.txt", "theirs")?;
        fixture.write("common/shared.txt", "shared")?;
        let mine = fixture.task("inside", 0)?;
        let inside_shared = fixture.task("inside", 1)?;
        let theirs = fixture.task("outside", 0)?;
        let outside_shared = fixture.task("outside", 1)?;
        let mine_v1 = fixture.step(&["tool", "read:in.txt", "v1"], &elsewhere)?;
        let mine_v2 = fixture.step(&["tool", "read:in.txt", "v2"], &elsewhere)?;
        let theirs_v1 = fixture.step(&["tool", "read:in.txt", "v1"], &work)?;
        let theirs_v2 = fixture.step(&["tool", "read:in.txt", "v2"], &work)?;
        let shared = fixture.step(&["tool", "read:shared.txt"], &fixture.path("common"))?;

        let first = fixture.build(tracer, &[
            (&mine, &[&mine_v1]),
            (&inside_shared, &[&shared]),
            (&theirs, &[&theirs_v1]),
            (&outside_shared, &[&shared]),
        ])?;
        assert_eq!(first.key(3, 0)?, first.key(1, 0)?);
        assert!(!first.ran(3, 0)?);
        let second = fixture.build(tracer, &[(&mine, &[&mine_v2]), (&theirs, &[&theirs_v2])])?;
        // Before the reset, the first configuration of the task under `inside` is a
        // hit on its historical record.
        let restored = fixture.build(tracer, &[(&mine, &[&mine_v1])])?;
        assert!(!restored.ran(0, 0)?);
        fixture.build(tracer, &[(&mine, &[&mine_v2])])?;
        let unowned = fixture.step(&["tool", "read:in.txt"], &work)?;
        let unowned_key = fixture.run_without_identity(tracer, &unowned)?;
        let store = fixture.store()?;
        let mut txn = store.write()?;
        txn.put(&inside_shared.id, &LearnedOrder {
            producers: [outside_shared.id.clone()].into(),
        })?;
        txn.commit()?;
        store.close()?;
        Ok(Self {
            inside: fixture.path("inside"),
            work,
            mine,
            inside_shared,
            theirs,
            outside_shared,
            mine_v1,
            theirs_v1,
            shared,
            mine_keys: [first.key(0, 0)?, second.key(0, 0)?],
            theirs_keys: [first.key(2, 0)?, second.key(1, 0)?],
            shared_key: first.key(1, 0)?,
            unowned_key,
        })
    }

    /// Fails unless the cache holds the task records and step positions of the
    /// tasks outside `inside` alone, and no learned order.
    fn assert_only_outside_tasks_remain(
        &self,
        store: &Store,
    ) -> TestResult {
        let txn = store.read()?;
        let outside = ids(&[&self.theirs, &self.outside_shared]);
        let tasks: Vec<TaskId> = txn
            .scan::<TaskRecord>(&[])?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(tasks, outside);
        let positioned: Vec<TaskId> = txn
            .scan::<StepPosition>(&[])?
            .into_iter()
            .map(|(key, _)| key.task)
            .collect();
        assert_eq!(positioned, outside);
        assert_eq!(txn.scan::<LearnedOrder>(&[])?, Vec::new());
        Ok(())
    }
}

/// A path reset with records that the engine stored follows their owners, never
/// their working directories:
///
/// * a task under the path whose step runs with an absolute `cwd` outside it, built in a
///   first configuration and then a second: restoring the first is a hit before the
///   reset, and after the reset both records are gone and restoring the first runs the
///   step again;
/// * a task outside the path whose step runs with an absolute `cwd` inside it keeps its
///   historical and current records, its step positions, and its task record, and
///   restoring its first configuration afterwards is a hit;
/// * a record shared by a task under the path and a task outside it survives, owned by
///   the outside task alone, while the task under the path loses its task record, step
///   positions, and learned order, and owns the record again when it reaches it;
/// * a record with no owner survives every path reset, including one of the directory it
///   ran in.
#[test]
fn a_reset_follows_the_owners_that_the_engine_stored() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let scene = Scene::new(&fixture, &tracer)?;
    let [mine_first, mine_second] = scene.mine_keys;
    for key in scene.mine_keys {
        assert_eq!(fixture.owners(&key)?, ids(&[&scene.mine]));
    }
    for key in scene.theirs_keys {
        assert_eq!(fixture.owners(&key)?, ids(&[&scene.theirs]));
    }
    let both = ids(&[&scene.inside_shared, &scene.outside_shared]);
    assert_eq!(fixture.owners(&scene.shared_key)?, both);
    assert_eq!(fixture.owners(&scene.unowned_key)?, Vec::<TaskId>::new());

    let store = fixture.store()?;
    let plan = plan_reset(&store, &scene.inside)?;
    assert_eq!(plan.counts(), EntryCounts {
        tasks: 2,
        steps: 2,
        shared_steps: 1,
        positions: 2,
        task_records: 2,
        orders: 1,
        writers: 0,
    });
    plan.apply(&store)?;
    scene.assert_only_outside_tasks_remain(&store)?;
    // Nothing more is the entry of a task under the path, and a reset of the
    // directory that the record with no owner ran in leaves it alone.
    for dir in [&scene.inside, &scene.work] {
        assert_eq!(plan_reset(&store, dir)?.counts(), EntryCounts::default());
    }
    store.close()?;
    assert_eq!(fixture.record(&mine_first)?, None);
    assert_eq!(fixture.record(&mine_second)?, None);
    for key in scene.theirs_keys {
        assert_eq!(fixture.owners(&key)?, ids(&[&scene.theirs]));
    }
    assert_eq!(
        fixture.owners(&scene.shared_key)?,
        ids(&[&scene.outside_shared])
    );
    assert_eq!(fixture.owners(&scene.unowned_key)?, Vec::<TaskId>::new());

    // The task outside the path finds its first configuration's record; the task
    // under the path runs its first configuration again and reaches the shared
    // record by its key, which makes it an owner again.
    let runs = tracer.runs();
    let built = fixture.build(&tracer, &[
        (&scene.theirs, &[&scene.theirs_v1]),
        (&scene.mine, &[&scene.mine_v1]),
        (&scene.inside_shared, &[&scene.shared]),
    ])?;
    assert_eq!(
        (built.ran(0, 0)?, built.ran(1, 0)?, built.ran(2, 0)?),
        (false, true, false)
    );
    assert_eq!(tracer.runs(), runs.saturating_add(1));
    assert_eq!(built.key(1, 0)?, mine_first);
    assert_eq!(fixture.owners(&mine_first)?, ids(&[&scene.mine]));
    assert_eq!(fixture.owners(&scene.shared_key)?, both);

    // A reset of every project removes every owned record and keeps the one with
    // no owner.
    let store = fixture.store()?;
    plan_reset(&store, &fixture.root)?.apply(&store)?;
    let steps: Vec<StepKey> = store
        .read()?
        .scan::<StepRecord>(&[])?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(steps, [scene.unowned_key]);
    store.close()?;
    Ok(())
}
