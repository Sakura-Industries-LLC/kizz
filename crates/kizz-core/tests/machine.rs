// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tests of the machine state of steps ("Machine state"): every step that runs
//! records the state that the tracer's machine query reports for its working
//! directory, a cache hit queries none to record, and only the parts that a step
//! names with `run(..., machine = [...])` are compared, value by value, with a
//! reason that names each changed value, old and new. The names are in the step key,
//! and `kizz why` shows the recorded values of the parts that a step names, and of
//! every part with `--verbose`.
//!
//! Each test builds a project directory and a cache in temporary directories. A
//! build is one [`Engine`] on a store opened in the cache, and its one task runs
//! through a [`TaskSteps`] made with a task identity. The fake tracer reports only
//! that the step ran its program, and its machine query answers with the state that
//! the test scripts, which it can change between builds.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{
    collections::BTreeSet,
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use kizz_core::{
    Engine, ProgramHash, ProgramParts, Render, StepDecision, StepExecutor as _, StepSpec,
    TaskIdentity, TaskSteps, WorkerPool, why,
};
use kizz_store::{
    DiskRecord, Explanation, FileSystemRecord, KernelRecord, LimitRecord, MachineChange,
    MachinePart, MachineStateRecord, MemoryRecord, Reason, StepKey, StepRecord, Store, TaskId,
    Verdict,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, Disk, EchoSink,
    ExtendedAttributes, FileSystem, FirstReads, Kernel, MachineState, Memory, OutputStream,
    ProcessTree, ResourceLimit, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The state that the fake's machine query starts with: every part.
fn state() -> MachineState {
    MachineState {
        kernel: Some(Kernel {
            release: "6.12.0".to_owned(),
            version: "#1 SMP".to_owned(),
        }),
        limits: Some(vec![ResourceLimit {
            resource: "nofile".to_owned(),
            soft: Some(1_024),
            hard: None,
        }]),
        affinity: Some(vec![0, 1, 2, 3]),
        memory: Some(Memory {
            total: 8_000,
            available: 4_000,
        }),
        filesystem: Some(FileSystem {
            kind: 0xEF53,
            flags: 0x1020,
        }),
        disk: Some(Disk {
            size: 100_000,
            free: 40_000,
        }),
    }
}

/// [`state`] as a step record keeps it.
fn recorded_state() -> MachineStateRecord {
    MachineStateRecord {
        kernel: Some(KernelRecord {
            release: "6.12.0".to_owned(),
            version: "#1 SMP".to_owned(),
        }),
        limits: Some(vec![LimitRecord {
            resource: "nofile".to_owned(),
            soft: Some(1_024),
            hard: None,
        }]),
        affinity: Some(vec![0, 1, 2, 3]),
        memory: Some(MemoryRecord {
            total: 8_000,
            available: 4_000,
        }),
        filesystem: Some(FileSystemRecord {
            kind: 0xEF53,
            flags: 0x1020,
        }),
        disk: Some(DiskRecord {
            size: 100_000,
            free: 40_000,
        }),
    }
}

/// A tracer that runs nothing and reports that the step ran its program, and whose
/// machine query answers with a scripted state and notes each query.
struct FakeTracer {
    /// The state that the machine query answers with.
    state: Mutex<MachineState>,
    /// The working directory of every machine query so far, in order.
    queries: Mutex<Vec<PathBuf>>,
    /// The number of runs so far.
    runs: Mutex<usize>,
}

impl FakeTracer {
    /// A fake whose machine query answers with [`state`].
    fn new() -> Self {
        Self {
            state: Mutex::new(state()),
            queries: Mutex::new(Vec::new()),
            runs: Mutex::new(0),
        }
    }

    /// Change the state that the machine query answers with.
    fn change(
        &self,
        change: impl FnOnce(&mut MachineState),
    ) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut state);
    }

    /// The working directory of every machine query so far.
    fn queries(&self) -> Vec<PathBuf> {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

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
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        *runs = runs.saturating_add(1);
        let mut accesses = Accesses::new();
        let mut first_reads = FirstReads::new();
        first_reads.note_at(&request.program, &accesses);
        accesses.record(request.program.clone(), AccessKind::Exec);
        Ok(RunResult {
            termination: Termination::Exited(0),
            output: CapturedOutput::empty(request.capture),
            tree: request
                .tree
                .then(|| ProcessTree::of_request(request, &accesses, Some(Termination::Exited(0)))),
            accesses,
            incomplete: None,
            first_reads,
            unknown_system_calls: BTreeSet::new(),
            connections: BTreeSet::new(),
        })
    }

    /// These tests read no extended attributes, so there are none.
    fn extended_attributes(
        &self,
        _path: &Path,
        _follow: bool,
    ) -> io::Result<ExtendedAttributes> {
        Ok(ExtendedAttributes::new())
    }

    fn machine_state(
        &self,
        cwd: &Path,
    ) -> MachineState {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(cwd.to_path_buf());
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

/// What one build decided and keyed, step by step.
struct Built {
    /// The decision on each step, in order.
    decisions: Vec<StepDecision>,
    /// The key of each step, in order.
    keys: Vec<StepKey>,
}

impl Built {
    /// The verdict on step `index`.
    fn verdict(
        &self,
        index: usize,
    ) -> TestResult<&Verdict> {
        Ok(&self.decisions.get(index).ok_or("no such step")?.verdict)
    }

    /// The key of step `index`.
    fn key(
        &self,
        index: usize,
    ) -> TestResult<StepKey> {
        Ok(*self.keys.get(index).ok_or("no such step")?)
    }
}

/// A project and a cache, each in a temporary directory, with an executable `tool`
/// in the project's `bin` directory.
struct Fixture {
    /// The project directory.
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
        let tool = fixture.project.path().join("bin/tool");
        fs::create_dir_all(fixture.project.path().join("bin"))?;
        fs::write(&tool, "#!/bin/sh\n")?;
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755))?;
        Ok(fixture)
    }

    /// A step that runs `tool` with `argument` in the project, with `PATH` set to
    /// `bin`, naming the parts `machine`.
    fn step(
        &self,
        argument: &str,
        machine: &[&str],
    ) -> TestResult<StepSpec> {
        Ok(StepSpec {
            argv: vec!["tool".to_owned(), argument.to_owned()],
            stdin: None,
            env: [(
                "PATH".to_owned(),
                self.project
                    .path()
                    .join("bin")
                    .to_string_lossy()
                    .into_owned(),
            )]
            .into(),
            cwd: AbsolutePath::new(self.project.path())?,
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            untraced: false,
            network: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            machine: machine.iter().map(|&name| name.to_owned()).collect(),
            expect: None,
        })
    }

    /// The task of the project's `kizz.md`, whose block is at line 3 under `Build`,
    /// with target `ci`.
    fn task(&self) -> TestResult<TaskIdentity> {
        Ok(TaskIdentity {
            id: TaskId {
                file: self.project.path().join("kizz.md"),
                index: 0,
            },
            line: 3,
            heading: Some("Build".to_owned()),
            targets: vec!["ci".to_owned()],
            program: ProgramHash::from_bytes([7; 32]),
            parts: ProgramParts::default(),
            dir: AbsolutePath::new(self.project.path())?,
        })
    }

    /// Run one build of the task with `tracer`, its steps `steps` in order.
    fn build(
        &self,
        tracer: &FakeTracer,
        steps: &[&StepSpec],
    ) -> TestResult<Built> {
        let store = Store::open(self.cache.path())?;
        let engine = Engine::new(
            tracer,
            &store,
            &NoEcho,
            Cancellation::new(),
            None,
            &WorkerPool::new()?,
        );
        let run: TaskSteps<'_> = engine.task_for(self.task()?);
        for step in steps {
            run.execute(step)?;
        }
        run.finish(true)?;
        let built = Built {
            decisions: run.decisions(),
            keys: run.keys(),
        };
        drop(run);
        drop(engine);
        store.close()?;
        Ok(built)
    }

    /// The step record stored under `key`.
    fn record(
        &self,
        key: StepKey,
    ) -> TestResult<StepRecord> {
        let store = Store::open(self.cache.path())?;
        let record = store.read()?.get::<StepRecord>(&key)?;
        store.close()?;
        Ok(record.ok_or("no record under the key")?)
    }

    /// The report of `kizz why` for the project, as plain or verbose text.
    fn why(
        &self,
        verbose: bool,
    ) -> TestResult<String> {
        let store = Store::open(self.cache.path())?;
        let report = why(&store, self.project.path())?;
        store.close()?;
        Ok(Render {
            base: self.project.path().to_path_buf(),
            home: None,
            verbose,
        }
        .why(&report))
    }
}

/// The verdict of a step that ran because the machine state changed by `changes`,
/// each a value with its old and new text.
fn machine_changed(changes: &[(&str, &str, &str)]) -> Verdict {
    Verdict::Ran(Explanation {
        reasons: vec![Reason::MachineChanged {
            changes: changes
                .iter()
                .map(|&(value, old, new)| {
                    MachineChange {
                        value: value.to_owned(),
                        old: old.to_owned(),
                        new: new.to_owned(),
                    }
                })
                .collect(),
        }],
        repeated: Vec::new(),
    })
}

/// A value of the machine state that changed: its name, and its old and new text.
type ValueChange = (&'static str, &'static str, &'static str);

/// A part, a change that the test makes to it in the fake's state, and the change
/// of value that the reason names.
type PartChange = (MachinePart, fn(&mut MachineState), ValueChange);

/// The change of each part that the tests make to the fake's state, with the
/// change of value that the reason names.
fn part_changes() -> Vec<PartChange> {
    let changes: [PartChange; 6] = [
        (
            MachinePart::Kernel,
            |state| {
                state.kernel = Some(Kernel {
                    release: "6.13.0".to_owned(),
                    version: "#1 SMP".to_owned(),
                });
            },
            ("kernel release", "6.12.0", "6.13.0"),
        ),
        (
            MachinePart::Limits,
            |state| {
                state.limits = Some(vec![ResourceLimit {
                    resource: "nofile".to_owned(),
                    soft: Some(2_048),
                    hard: None,
                }]);
            },
            ("soft limit of nofile", "1024", "2048"),
        ),
        (
            MachinePart::Affinity,
            |state| state.affinity = Some(vec![0, 1]),
            ("CPUs", "0-3", "0-1"),
        ),
        (
            MachinePart::Memory,
            |state| {
                state.memory = Some(Memory {
                    total: 8_000,
                    available: 3_000,
                });
            },
            ("available memory", "4000 bytes", "3000 bytes"),
        ),
        (
            MachinePart::Filesystem,
            |state| {
                state.filesystem = Some(FileSystem {
                    kind: 0xEF53,
                    flags: 0x1021,
                });
            },
            ("mount flags", "0x1020", "0x1021"),
        ),
        (
            MachinePart::Disk,
            |state| {
                state.disk = Some(Disk {
                    size: 100_000,
                    free: 39_000,
                });
            },
            ("free space", "40000 bytes", "39000 bytes"),
        ),
    ];
    Vec::from(changes)
}

/// For each part: a step that names it runs again when a value of that part
/// changes, with a reason that names the value, old and new, and the same command
/// without the name stays cached; a step that names another part stays cached too.
/// A further build with nothing changed caches all three.
#[test]
fn a_step_runs_again_only_when_a_part_it_names_changes() -> TestResult {
    for (part, change, (value, old, new)) in part_changes() {
        let fixture = Fixture::new()?;
        let tracer = FakeTracer::new();
        let other = if part == MachinePart::Kernel {
            "disk"
        } else {
            "kernel"
        };
        let named = fixture.step("named", &[part.name()])?;
        let plain = fixture.step("plain", &[])?;
        let elsewhere = fixture.step("elsewhere", &[other])?;
        let steps = [&named, &plain, &elsewhere];
        fixture.build(&tracer, &steps)?;
        tracer.change(change);

        let built = fixture.build(&tracer, &steps)?;
        assert_eq!(
            built.verdict(0)?,
            &machine_changed(&[(value, old, new)]),
            "{part}"
        );
        assert_eq!(built.verdict(1)?, &Verdict::Cached, "{part}");
        assert_eq!(built.verdict(2)?, &Verdict::Cached, "{part}");
        assert_eq!(tracer.runs(), 4, "{part}");

        let again = fixture.build(&tracer, &steps)?;
        for index in 0..steps.len() {
            assert_eq!(again.verdict(index)?, &Verdict::Cached, "{part}");
        }
        assert_eq!(tracer.runs(), 4, "{part}");
    }
    Ok(())
}

/// A step that names no part is never run again for a change of machine state,
/// whatever changes, and its cache check queries nothing.
#[test]
fn a_step_that_names_no_part_never_runs_for_machine_state() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::new();
    let step = fixture.step("plain", &[])?;
    fixture.build(&tracer, &[&step])?;
    for (_, change, _) in part_changes() {
        tracer.change(change);
    }
    let built = fixture.build(&tracer, &[&step])?;
    assert_eq!(built.verdict(0)?, &Verdict::Cached);
    assert_eq!(tracer.runs(), 1);
    assert_eq!(tracer.queries().len(), 1);
    Ok(())
}

/// The state is queried for the working directory of every step that runs, a
/// missing program included, and recorded with it, whether or not the step names a
/// part; a cache hit queries none to record, and only the check of a step that names
/// a part queries the state, once.
#[test]
fn the_state_is_recorded_for_every_step_that_runs_and_queried_for_no_hit() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::new();
    let plain = fixture.step("plain", &[])?;
    let named = fixture.step("named", &["kernel", "memory"])?;
    let sub = fixture.project.path().join("sub");
    fs::create_dir_all(&sub)?;
    let mut elsewhere = fixture.step("elsewhere", &[])?;
    elsewhere.cwd = AbsolutePath::new(&sub)?;
    let mut missing = fixture.step("missing", &[])?;
    missing.argv = vec!["missing-program".to_owned()];

    let first = fixture.build(&tracer, &[&plain, &named, &elsewhere])?;
    let root = fixture.project.path().to_path_buf();
    assert_eq!(tracer.queries(), [root.clone(), root.clone(), sub]);
    for index in 0..3 {
        let record = fixture.record(first.key(index)?)?;
        assert_eq!(record.machine, recorded_state(), "step {index}");
    }
    assert_eq!(fixture.record(first.key(1)?)?.spec.machine, [
        "kernel", "memory"
    ]);

    let store = Store::open(fixture.cache.path())?;
    let engine = Engine::new(
        &tracer,
        &store,
        &NoEcho,
        Cancellation::new(),
        None,
        &WorkerPool::new()?,
    );
    let run = engine.task(ProgramHash::from_bytes([7; 32]), fixture.task()?.dir);
    assert!(run.execute(&missing).is_err());
    let missing_key = *run.keys().first().ok_or("no key")?;
    drop(run);
    drop(engine);
    store.close()?;
    assert_eq!(tracer.queries().len(), 4);
    assert_eq!(fixture.record(missing_key)?.machine, recorded_state());

    let second = fixture.build(&tracer, &[&plain, &named, &elsewhere])?;
    for index in 0..3 {
        assert_eq!(second.verdict(index)?, &Verdict::Cached, "step {index}");
    }
    // Only the check of the step that names parts queried the state.
    assert_eq!(tracer.queries().len(), 5);
    assert_eq!(tracer.queries().last(), Some(&root));
    assert_eq!(tracer.runs(), 3);
    Ok(())
}

/// A change of the `machine` names changes the step key, so the step runs, with a
/// reason that names the old and new names; the names are sorted and without
/// repeats, as `run()` gives them.
#[test]
fn a_change_of_names_changes_the_key_and_runs_the_step() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::new();
    let before = fixture.step("tool", &[])?;
    let after = fixture.step("tool", &["kernel", "limits"])?;
    let first = fixture.build(&tracer, &[&before])?;
    let second = fixture.build(&tracer, &[&after])?;
    assert_ne!(first.key(0)?, second.key(0)?);
    assert_eq!(
        second.verdict(0)?,
        &Verdict::Ran(Explanation {
            reasons: vec![Reason::MachineNamesChanged {
                old: Vec::new(),
                new: vec!["kernel".to_owned(), "limits".to_owned()],
            }],
            repeated: Vec::new(),
        })
    );
    assert_eq!(tracer.runs(), 2);
    let render = Render {
        base: fixture.project.path().to_path_buf(),
        home: None,
        verbose: false,
    };
    assert_eq!(
        render.one_line(second.verdict(0)?),
        "the `machine` names changed from none to `kernel`, `limits`"
    );
    Ok(())
}

/// Golden output: the reason for a changed machine state in full and on one line,
/// and `kizz why`, which shows the parts that the step names, and with `--verbose`
/// every part, one line for each, those that the step names marked `checked`.
#[test]
fn machine_state_is_explained_and_shown_by_why() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::new();
    let step = fixture.step("build", &["kernel", "memory"])?;
    fixture.build(&tracer, &[&step])?;
    tracer.change(|state| {
        state.kernel = Some(Kernel {
            release: "6.13.0".to_owned(),
            version: "#2 SMP".to_owned(),
        });
        state.memory = Some(Memory {
            total: 8_000,
            available: 3_500,
        });
        state.disk = None;
    });
    let built = fixture.build(&tracer, &[&step])?;
    let render = Render {
        base: fixture.project.path().to_path_buf(),
        home: None,
        verbose: false,
    };
    let verdict = built.verdict(0)?;
    assert_eq!(render.full(verdict), [
        "ran because:",
        "  the machine state changed:",
        "    kernel release from 6.12.0 to 6.13.0",
        "    kernel version from #1 SMP to #2 SMP",
        "    available memory from 4000 bytes to 3500 bytes",
    ]);
    assert_eq!(
        render.one_line(verdict),
        "the machine state changed: kernel release from 6.12.0 to 6.13.0, kernel version from #1 \
         SMP to #2 SMP, available memory from 4000 bytes to 3500 bytes"
    );

    let head = "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool build`
    ran because:
      the machine state changed:
        kernel release from 6.12.0 to 6.13.0
        kernel version from #1 SMP to #2 SMP
        available memory from 4000 bytes to 3500 bytes
    untracked /proc and /sys accesses: none
    connections: none
";
    assert_eq!(
        fixture.why(false)?,
        format!(
            "{head}    machine kernel: kernel release 6.13.0, kernel version #2 SMP
    machine memory: total memory 8000 bytes, available memory 3500 bytes
    ignored accesses: 0
"
        )
    );
    assert_eq!(
        fixture.why(true)?,
        format!(
            "{head}    machine kernel (checked): kernel release 6.13.0, kernel version #2 SMP
    machine limits: soft limit of nofile 1024, hard limit of nofile unlimited
    machine affinity: CPUs 0-3
    machine memory (checked): total memory 8000 bytes, available memory 3500 bytes
    machine filesystem: file system type 0xef53, mount flags 0x1020
    machine disk: unknown
    ignored accesses: 0
"
        )
    );
    Ok(())
}
