// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Runs in which a file changed after the command first read it, with a fake
//! tracer and real files.
//!
//! Each test builds a project directory and a cache in temporary directories and
//! opens the store in that cache. A build is one [`Engine`]. The fake tracer
//! performs the file operations that a step's arguments name on real files, in
//! order, and reports them; as the Linux tracer does, it notes the metadata of each
//! file that the command reads or runs before it writes it, at the moment of the
//! read:
//!
//! * `read:PATH` reads a file;
//! * `write:PATH=TEXT` writes `TEXT` to a file;
//! * `remove:PATH` removes a file, if it is there;
//! * `relink:LINK=TARGET` points the symbolic link `LINK` at `TARGET`;
//! * `outside:PATH=TEXT` writes `TEXT` to a file without reporting a write, as an edit by
//!   something else while the command runs.
//!
//! Relative paths are relative to the project directory.

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
    fs,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use kizz_core::{
    Engine, ProgramHash, Render, StepDecision, StepExecutor as _, StepSpec, WorkerPool, step_key,
};
use kizz_store::{ChangedRead, NotCachedReason, RunReason, StepKey, StepRecord, Store, Verdict};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    FirstReads, OutputStream, ProcessTree, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The program hash every step of these tests uses.
const PROGRAM: ProgramHash = ProgramHash::from_bytes([7; 32]);

/// A tracer failure from `error`.
fn tracer_failure(error: &dyn Error) -> TraceError {
    TraceError::Tracer {
        message: error.to_string(),
        source: None,
    }
}

/// What a run of `request` does to real files and reports, as the module
/// documentation describes.
fn script(request: &RunRequest) -> Result<RunResult, TraceError> {
    let cwd = request.cwd.as_path();
    let mut accesses = Accesses::new();
    let mut first_reads = FirstReads::new();
    first_reads.note_at(&request.program, &accesses);
    accesses.record(request.program.clone(), AccessKind::Exec);
    for arg in request.args.iter().skip(1) {
        let arg = arg.to_str().unwrap_or_default();
        let (op, rest) = arg.split_once(':').unwrap_or((arg, ""));
        let (path, text) = rest.split_once('=').unwrap_or((rest, ""));
        let path = cwd.join(path);
        let absolute = AbsolutePath::new(&path).map_err(|error| tracer_failure(&error))?;
        let failed = |error: std::io::Error| tracer_failure(&error);
        match op {
            "read" => {
                let _content: Vec<u8> = fs::read(&path).unwrap_or_default();
                first_reads.note_at(&absolute, &accesses);
                accesses.record(absolute, AccessKind::Read);
            },
            "write" => {
                fs::write(&path, text).map_err(failed)?;
                accesses.record(absolute, AccessKind::Write);
            },
            "remove" => {
                match fs::remove_file(&path) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(failed(error));
                    },
                    Ok(()) | Err(_) => {},
                }
                accesses.record(absolute, AccessKind::Write);
            },
            "relink" => {
                fs::remove_file(&path).map_err(failed)?;
                symlink(text, &path).map_err(failed)?;
                accesses.record(absolute, AccessKind::Write);
            },
            "outside" => fs::write(&path, text).map_err(failed)?,
            _ => {},
        }
    }
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

/// A tracer that runs nothing, follows [`script`], and counts its runs.
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
        script(request)
    }

    /// These tests read no extended attributes, so there are none.
    fn extended_attributes(
        &self,
        _path: &std::path::Path,
        _follow: bool,
    ) -> std::io::Result<kizz_trace::ExtendedAttributes> {
        Ok(kizz_trace::ExtendedAttributes::new())
    }

    /// These tests name no machine state, so the fake knows no part.
    fn machine_state(
        &self,
        _cwd: &std::path::Path,
    ) -> kizz_trace::MachineState {
        kizz_trace::MachineState::default()
    }
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

/// What one build of one step did.
struct Built {
    /// Whether the step came from the cache.
    cached: bool,
    /// The steps whose results were not cached, with why.
    not_cached: Vec<(StepKey, NotCachedReason)>,
    /// What the build decided about the step.
    decision: StepDecision,
}

/// A project and a cache, each in a temporary directory, with an executable `tool`
/// in the project's `bin` directory.
struct Fixture {
    /// The project directory, the working directory of every step.
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
        fixture.write("bin/tool", "#!/bin/sh\n")?;
        fs::set_permissions(fixture.path("bin/tool"), fs::Permissions::from_mode(0o755))?;
        Ok(fixture)
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

    /// A step that runs `argv` in the project with `PATH` set to the project's
    /// `bin` directory, ignoring the paths that `ignore` matches.
    fn step(
        &self,
        argv: &[&str],
        ignore: &[&str],
    ) -> TestResult<StepSpec> {
        Ok(StepSpec {
            argv: argv.iter().map(|&arg| arg.to_owned()).collect(),
            stdin: None,
            env: [(
                "PATH".to_owned(),
                self.path("bin").to_string_lossy().into_owned(),
            )]
            .into(),
            cwd: AbsolutePath::new(self.project.path())?,
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            untraced: false,
            ignore: ignore.iter().map(|&pattern| pattern.to_owned()).collect(),
            track: Vec::new(),
            timestamps: false,
            machine: Vec::new(),
            expect: None,
        })
    }

    /// Run `step` in one build, a new engine with `tracer`.
    fn build(
        &self,
        tracer: &FakeTracer,
        step: &StepSpec,
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
        let task = engine.task(PROGRAM, AbsolutePath::new(self.project.path())?);
        let outcome = task.execute(step).map_err(|error| error.message)?;
        let decision = task
            .decisions()
            .into_iter()
            .next()
            .ok_or("the build decided nothing")?;
        let built = Built {
            cached: outcome.cached,
            not_cached: engine.not_cached()?,
            decision,
        };
        drop(task);
        drop(engine);
        store.close()?;
        Ok(built)
    }

    /// The record of `step`.
    fn record(
        &self,
        step: &StepSpec,
    ) -> TestResult<StepRecord> {
        let store = Store::open(self.cache.path())?;
        let record = store
            .read()?
            .get::<StepRecord>(&step_key(PROGRAM, step, 0)?)?;
        store.close()?;
        record.ok_or_else(|| "the step has no record".into())
    }

    /// The one-line reason of `decision`, with paths relative to the project.
    fn reason(
        &self,
        decision: &StepDecision,
    ) -> String {
        Render {
            base: self.project.path().to_path_buf(),
            home: None,
            verbose: false,
        }
        .one_line(&decision.verdict)
    }
}

/// The reason for the files `files`, each with whether the command wrote it.
fn changed(files: &[(&Path, bool)]) -> NotCachedReason {
    NotCachedReason::ChangedAfterRead {
        files: files
            .iter()
            .map(|(path, written)| {
                ChangedRead {
                    path: path.to_path_buf(),
                    written: *written,
                }
            })
            .collect(),
    }
}

/// Check that `step`, whose run left the reason `why`, was not cached in the build
/// `built`, and that the next build runs it again with that reason; return the
/// one-line reason of that run.
fn runs_again(
    fixture: &Fixture,
    tracer: &FakeTracer,
    step: &StepSpec,
    built: &Built,
    why: &NotCachedReason,
) -> TestResult<String> {
    assert!(!built.cached);
    let key = step_key(PROGRAM, step, 0)?;
    assert_eq!(built.not_cached, vec![(key, why.clone())]);
    assert_eq!(fixture.record(step)?.not_cached.as_ref(), Some(why));
    let runs = tracer.runs();
    let next = fixture.build(tracer, step)?;
    assert!(!next.cached);
    assert_eq!(tracer.runs(), runs.saturating_add(1));
    let RunReason::NotCached { why: reason, .. } = fixture.record(step)?.reason else {
        return Err("the next run must say that the previous one was not cached".into());
    };
    assert_eq!(&reason, why);
    Ok(fixture.reason(&next.decision))
}

/// The review's case: a command that reads a file and then rewrites it leaves a
/// result that is not cached, whose reason names the file and says that the command
/// wrote it, and the next build runs it again with that reason.
#[test]
fn a_command_that_rewrites_a_file_it_read_is_not_cached() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write("in.txt", "old\n")?;
    let tracer = FakeTracer::default();
    let step = fixture.step(
        &[
            "tool",
            "read:in.txt",
            "write:out.txt=old",
            "write:in.txt=new!\n",
        ],
        &[],
    )?;
    let built = fixture.build(&tracer, &step)?;
    let why = changed(&[(&input, true)]);
    let reason = runs_again(&fixture, &tracer, &step, &built, &why)?;
    assert!(
        reason.starts_with(
            "the previous run could not be cached: a file changed after the command read it: \
             `in.txt` (the command wrote it)"
        ),
        "{reason}"
    );
    Ok(())
}

/// A file that something outside the command rewrites while the command runs,
/// after the command read it, leaves a result that is not cached, whose reason
/// says that something else changed it.
#[test]
fn a_file_changed_by_something_else_during_the_run_is_not_cached() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write("in.txt", "old\n")?;
    let tracer = FakeTracer::default();
    let step = fixture.step(&["tool", "read:in.txt", "outside:in.txt=edited\n"], &[])?;
    let built = fixture.build(&tracer, &step)?;
    let why = changed(&[(&input, false)]);
    let reason = runs_again(&fixture, &tracer, &step, &built, &why)?;
    assert!(
        reason.starts_with(
            "the previous run could not be cached: a file changed after the command read it: \
             `in.txt` (something else changed it while the command ran)"
        ),
        "{reason}"
    );
    Ok(())
}

/// A file that the command writes and then reads is cached: its first read came
/// after a write, so nothing was noted for it.
#[test]
fn a_file_the_command_writes_and_then_reads_is_cached() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let step = fixture.step(
        &[
            "tool",
            "write:gen.txt=one",
            "read:gen.txt",
            "write:gen.txt=two!",
        ],
        &[],
    )?;
    let built = fixture.build(&tracer, &step)?;
    assert!(!built.cached);
    assert_eq!(built.not_cached, Vec::new());
    assert_eq!(fixture.record(&step)?.not_cached, None);
    let next = fixture.build(&tracer, &step)?;
    assert!(next.cached);
    assert_eq!(tracer.runs(), 1);
    Ok(())
}

/// An ignored path that changes during the run is never compared, and the result
/// is cached; so is a file that was read and that nothing changed.
#[test]
fn an_ignored_path_that_changes_during_the_run_is_never_compared() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("scratch.tmp", "old\n")?;
    fixture.write("in.txt", "kept\n")?;
    let tracer = FakeTracer::default();
    let step = fixture.step(
        &[
            "tool",
            "read:scratch.tmp",
            "read:in.txt",
            "outside:scratch.tmp=changed\n",
        ],
        &["*.tmp"],
    )?;
    let built = fixture.build(&tracer, &step)?;
    assert!(!built.cached);
    assert_eq!(built.not_cached, Vec::new());
    assert_eq!(fixture.record(&step)?.not_cached, None);
    let next = fixture.build(&tracer, &step)?;
    assert!(next.cached);
    assert_eq!(tracer.runs(), 1);
    Ok(())
}

/// A first read through a symbolic link whose target the command changes
/// afterwards is not cached: the link is compared through its final target.
#[test]
fn a_link_retargeted_after_its_first_read_is_not_cached() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("a.txt", "same")?;
    fixture.write("b.txt", "same")?;
    let link = fixture.path("current");
    symlink("a.txt", &link)?;
    let tracer = FakeTracer::default();
    let step = fixture.step(&["tool", "read:current", "relink:current=b.txt"], &[])?;
    let built = fixture.build(&tracer, &step)?;
    let why = changed(&[(&link, true)]);
    let reason = runs_again(&fixture, &tracer, &step, &built, &why)?;
    assert!(
        reason.contains("`current` (the command wrote it)"),
        "{reason}"
    );
    Ok(())
}

/// A file removed after its first read is not cached.
#[test]
fn a_file_removed_after_its_first_read_is_not_cached() -> TestResult {
    let fixture = Fixture::new()?;
    let input = fixture.write("in.txt", "gone soon")?;
    let tracer = FakeTracer::default();
    let step = fixture.step(&["tool", "read:in.txt", "remove:in.txt"], &[])?;
    let built = fixture.build(&tracer, &step)?;
    let why = changed(&[(&input, true)]);
    let reason = runs_again(&fixture, &tracer, &step, &built, &why)?;
    assert!(
        reason.contains("`in.txt` (the command wrote it)"),
        "{reason}"
    );
    Ok(())
}

/// Several changed files are listed sorted by path, each with who changed it, and
/// a file that the command read and did not change is not listed.
#[test]
fn several_changed_files_are_listed_sorted_with_who_changed_each() -> TestResult {
    let fixture = Fixture::new()?;
    let first = fixture.write("a.txt", "a")?;
    fixture.write("b.txt", "b")?;
    let last = fixture.write("c.txt", "c")?;
    let tracer = FakeTracer::default();
    let step = fixture.step(
        &[
            "tool",
            "read:c.txt",
            "read:b.txt",
            "read:a.txt",
            "outside:c.txt=cc",
            "write:a.txt=aa",
        ],
        &[],
    )?;
    let built = fixture.build(&tracer, &step)?;
    let why = changed(&[(&first, true), (&last, false)]);
    let reason = runs_again(&fixture, &tracer, &step, &built, &why)?;
    assert!(
        reason.starts_with(
            "the previous run could not be cached: 2 files changed after the command read \
             them: `a.txt` (the command wrote it), `c.txt` (something else changed it while \
             the command ran)"
        ),
        "{reason}"
    );
    assert!(matches!(built.decision.verdict, Verdict::Ran(_)));
    Ok(())
}
