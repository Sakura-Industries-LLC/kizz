// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Golden-output tests of explanations: every reason kind, every kind of last
//! writer, environment values hidden without `--verbose` and shown with it, a step
//! that writes a file and also reads it, the warning for consecutive builds, the
//! one-line reasons, permission and owner changes, timestamp changes and the
//! `timestamps` option, order and entries changes of listed directories, system
//! calls that the tracer does not know, and the report of `kizz why`.
//!
//! Each test builds a project directory and a cache in temporary directories. A
//! build is one [`Engine`] on a store opened in the cache, and each task runs through
//! a [`TaskSteps`] made with a task identity, so the engine stores step positions,
//! last writers, and task records. The fake tracer performs the file operations
//! that a step's arguments name on real files and reports them:
//!
//! * `read:PATH` reads a file;
//! * `write:PATH=TEXT` writes `TEXT` to a file;
//! * `rw:PATH=TEXT` reads a file and then writes `TEXT` to it;
//! * `bump:PATH` reads a file and writes it back with one more `+`;
//! * `kernel:PATH` reads an absolute path, such as one under `/proc`;
//! * `probe:PATH` checks an absolute path;
//! * `list:PATH` lists a directory;
//! * `signal` makes a signal kill the command;
//! * `unknown:NUMBER` reports the system call `NUMBER` as one that the tracer does not
//!   know;
//!
//! and any other argument does nothing. Relative paths are relative to the step's
//! working directory. As the Linux tracer does, the fake notes the metadata of each
//! file that a step reads or runs before it writes it, at the moment of the read,
//! so `rw` and `bump` leave a result that is not cached.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs::{self, File, FileTimes},
    io,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::{Duration, UNIX_EPOCH},
};

use kizz_core::{
    Engine, PrintSink, ProgramHash, ProgramParts, Render, RepeatedWarning, StepDecision,
    StepExecutor as _, StepSpec, TaskIdentity, TaskInfo, TaskProgram, TaskSteps, observe,
    parse_tasks, why,
};
use kizz_store::{
    Explanation, Owner, PathState, Reason, SelfInflicted, StepPosition, StepRecord, Store, TaskId,
    TaskRecord, Verdict, Writer,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    FirstReads, OutputStream, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The program hash of the tasks that run steps directly.
const PROGRAM: [u8; 32] = [7; 32];

/// The full reason line of a step whose previous run read `state.txt` and then
/// rewrote it, so its result was not cached.
const REWROTE_STATE: &str = "  the previous run could not be cached: a file changed after the \
                             command read it: `state.txt` (the command wrote it)";

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
    let mut termination = Termination::Exited(0);
    let mut unknown = BTreeSet::new();
    let mut record = |path: PathBuf, kind: AccessKind| -> Result<(), TraceError> {
        let path = AbsolutePath::new(path).map_err(|error| tracer_failure(&error))?;
        if matches!(kind, AccessKind::Read | AccessKind::Exec) {
            first_reads.note_at(&path, &accesses);
        }
        accesses.record(path, kind);
        Ok(())
    };
    for arg in request.args.iter().skip(1) {
        let arg = arg.to_str().unwrap_or_default();
        let (op, rest) = arg.split_once(':').unwrap_or((arg, ""));
        let (path, text) = rest.split_once('=').unwrap_or((rest, ""));
        let path = cwd.join(path);
        let failed = |error: io::Error| tracer_failure(&error);
        match op {
            "read" => {
                let _content: Vec<u8> = fs::read(&path).unwrap_or_default();
                record(path, AccessKind::Read)?;
            },
            "write" => {
                fs::write(&path, text).map_err(failed)?;
                record(path, AccessKind::Write)?;
            },
            "rw" => {
                let _content: Vec<u8> = fs::read(&path).unwrap_or_default();
                record(path.clone(), AccessKind::Read)?;
                fs::write(&path, text).map_err(failed)?;
                record(path, AccessKind::Write)?;
            },
            "bump" => {
                let mut content = fs::read_to_string(&path).unwrap_or_default();
                record(path.clone(), AccessKind::Read)?;
                content.push('+');
                fs::write(&path, content).map_err(failed)?;
                record(path, AccessKind::Write)?;
            },
            "kernel" => record(path, AccessKind::Read)?,
            "probe" => record(path, AccessKind::Probe)?,
            "list" => record(path, AccessKind::List)?,
            "signal" => termination = Termination::Signaled(9),
            "unknown" => {
                unknown.insert(
                    rest.parse::<u32>()
                        .map_err(|error| tracer_failure(&error))?,
                );
            },
            _ => {},
        }
    }
    Ok(RunResult {
        termination,
        output: CapturedOutput::empty(request.capture),
        accesses,
        incomplete: None,
        first_reads,
        unknown_system_calls: unknown,
    })
}

/// A tracer that runs nothing and follows [`script`].
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
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        *runs = runs.saturating_add(1);
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

/// Prints nothing.
struct NoPrint;

impl PrintSink for NoPrint {
    fn print_line(
        &self,
        _line: &str,
    ) {
    }
}

/// What one build decided, task by task.
struct Built {
    /// The decisions of each task's run, in the order the tasks ran.
    decisions: Vec<Vec<StepDecision>>,
    /// The warnings for steps that re-ran in consecutive builds.
    warnings: Vec<RepeatedWarning>,
}

impl Built {
    /// The verdict on step `step` of task `task`.
    fn verdict(
        &self,
        task: usize,
        step: usize,
    ) -> TestResult<&Verdict> {
        Ok(&self
            .decisions
            .get(task)
            .and_then(|steps| steps.get(step))
            .ok_or("no such step")?
            .verdict)
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
    ) -> TestResult {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(())
    }

    /// The project's `bin` directory, as `PATH` holds it.
    fn bin(&self) -> String {
        self.path("bin").to_string_lossy().into_owned()
    }

    /// A step that runs `argv` in the project with `PATH` set to `bin`.
    fn step(
        &self,
        argv: &[&str],
    ) -> TestResult<StepSpec> {
        Ok(StepSpec {
            argv: argv.iter().map(|&arg| arg.to_owned()).collect(),
            stdin: None,
            env: [("PATH".to_owned(), self.bin())].into(),
            cwd: AbsolutePath::new(self.project.path())?,
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            expect: None,
        })
    }

    /// The task with index `index` in the project's `kizz.md`, whose block is at
    /// `line` under `heading`, with target `ci`.
    fn task(
        &self,
        index: u32,
        line: u32,
        heading: &str,
    ) -> TestResult<TaskIdentity> {
        Ok(TaskIdentity {
            id: TaskId {
                file: self.path("kizz.md"),
                index,
            },
            line,
            heading: Some(heading.to_owned()),
            targets: vec!["ci".to_owned()],
            program: ProgramHash::from_bytes(PROGRAM),
            parts: ProgramParts::default(),
            dir: AbsolutePath::new(self.project.path())?,
        })
    }

    /// Open the store in the cache.
    fn store(&self) -> TestResult<Store> {
        Ok(Store::open(self.cache.path())?)
    }

    /// Run one build with `tracer`: each task with its steps, in order, in
    /// `passes` passes, finishing each task run as succeeded; the engine caches the
    /// results of runs that made unknown system calls when `cache_unknown` is set.
    fn build_passes(
        &self,
        tracer: &FakeTracer,
        tasks: &[(&TaskIdentity, &[&StepSpec])],
        passes: usize,
        cache_unknown: bool,
    ) -> TestResult<Built> {
        let store = self.store()?;
        let engine = Engine::new(tracer, &store, &NoEcho, Cancellation::new(), None)
            .with_cache_unknown_system_calls(cache_unknown);
        let mut decisions = Vec::new();
        for _ in 0..passes {
            for (identity, steps) in tasks {
                let run: TaskSteps<'_> = engine.task_for((*identity).clone());
                for step in *steps {
                    // Failed steps are part of what is explained, not test failures.
                    let _result: Result<_, _> = run.execute(step);
                }
                run.finish(true)?;
                decisions.push(run.decisions());
            }
        }
        let built = Built {
            decisions,
            warnings: engine.warnings(),
        };
        drop(engine);
        store.close()?;
        Ok(built)
    }

    /// Run one build with `tracer`, each task once.
    fn build(
        &self,
        tracer: &FakeTracer,
        tasks: &[(&TaskIdentity, &[&StepSpec])],
    ) -> TestResult<Built> {
        self.build_passes(tracer, tasks, 1, false)
    }

    /// The renderer of the project, verbose or not.
    fn render(
        &self,
        verbose: bool,
    ) -> Render {
        Render {
            base: self.project.path().to_path_buf(),
            home: None,
            verbose,
        }
    }

    /// The full reasons of `verdict`, as plain or verbose output shows them.
    fn full(
        &self,
        verdict: &Verdict,
        verbose: bool,
    ) -> Vec<String> {
        self.render(verbose).full(verdict)
    }

    /// The report of `kizz why` for the project, as text.
    fn why(
        &self,
        verbose: bool,
    ) -> TestResult<String> {
        let store = self.store()?;
        let report = why(&store, self.project.path())?;
        store.close()?;
        Ok(self.render(verbose).why(&report))
    }
}

/// Lines from string slices.
fn lines(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|&line| line.to_owned()).collect()
}

/// A step with no record says that it never ran, and an unchanged step is cached.
#[test]
fn a_step_without_a_record_never_ran_and_an_unchanged_one_is_cached() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("in.txt", "one")?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let step = fixture.step(&["tool", "read:in.txt"])?;
    let built = fixture.build(&tracer, &[(&task, &[&step])])?;
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  never run: there is no record for this step"
        ])
    );
    let built = fixture.build(&tracer, &[(&task, &[&step])])?;
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&["cached: no recorded input changed"])
    );
    assert_eq!(tracer.runs(), 1);
    Ok(())
}

/// Changed arguments are compared word by word.
#[test]
fn changed_arguments_show_old_and_new_word_by_word() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let before = fixture.step(&["tool", "-O2", "a.c"])?;
    let after = fixture.step(&["tool", "-O0", "a.c", "-g"])?;
    fixture.build(&tracer, &[(&task, &[&before])])?;
    let built = fixture.build(&tracer, &[(&task, &[&after])])?;
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  the arguments changed:",
            "    old: tool -O2 a.c",
            "    new: tool -O0 a.c -g",
            "    word 2: `-O2` became `-O0`",
            "    word 4 added: `-g`",
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        "the arguments changed: word 2: `-O2` became `-O0`, word 4 added: `-g`"
    );
    Ok(())
}

/// A changed environment names the variables added, removed, and changed, and shows
/// their values only when verbose.
#[test]
fn environment_values_are_hidden_without_verbose_and_shown_with_it() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let mut before = fixture.step(&["tool"])?;
    before.env.insert("GONE".to_owned(), "old-value".to_owned());
    before
        .env
        .insert("TOKEN".to_owned(), "secret-one".to_owned());
    let mut after = fixture.step(&["tool"])?;
    after.env.insert("NEW".to_owned(), "new-value".to_owned());
    after
        .env
        .insert("TOKEN".to_owned(), "secret-two".to_owned());
    fixture.build(&tracer, &[(&task, &[&before])])?;
    let built = fixture.build(&tracer, &[(&task, &[&after])])?;
    let verdict = built.verdict(0, 0)?;

    let plain = fixture.full(verdict, false);
    assert_eq!(
        plain,
        lines(&[
            "ran because:",
            "  the environment changed:",
            "    removed GONE",
            "    added NEW",
            "    changed TOKEN",
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(verdict),
        "the environment changed: removed GONE, added NEW, changed TOKEN"
    );
    let shown = format!("{plain:?}{}", fixture.render(false).one_line(verdict));
    for value in ["old-value", "new-value", "secret"] {
        assert!(!shown.contains(value), "{shown}");
    }

    assert_eq!(
        fixture.full(verdict, true),
        lines(&[
            "ran because:",
            "  the environment changed:",
            "    removed GONE=old-value",
            "    added NEW=new-value",
            "    changed TOKEN from `secret-one` to `secret-two`",
        ])
    );
    assert_eq!(
        fixture.render(true).one_line(verdict),
        "the environment changed: removed GONE=old-value, added NEW=new-value, changed TOKEN \
         from `secret-one` to `secret-two`"
    );

    // `kizz why` follows the same rule.
    let why = fixture.why(false)?;
    assert!(why.contains("    changed TOKEN\n"), "{why}");
    assert!(!why.contains("secret"), "{why}");
    let verbose = fixture.why(true)?;
    assert!(
        verbose.contains("    changed TOKEN from `secret-one` to `secret-two`\n"),
        "{verbose}"
    );
    Ok(())
}

/// The working directory, `stdin`, the capture mode, the ignore patterns, and the
/// `track` patterns each give their reason, in the order of the spec's list.
#[test]
fn directory_stdin_capture_ignore_and_track_changes_give_their_reasons() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.path("sub"))?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let before = fixture.step(&["tool"])?;
    let mut after = fixture.step(&["tool"])?;
    after.cwd = AbsolutePath::new(fixture.path("sub"))?;
    after.stdin = Some("hello\n".to_owned());
    after.capture = CaptureMode::Merged;
    after.ignore = vec!["*.o".to_owned()];
    after.track = vec!["/proc/cpuinfo".to_owned()];
    fixture.build(&tracer, &[(&task, &[&before])])?;
    let built = fixture.build(&tracer, &[(&task, &[&after])])?;
    let verdict = built.verdict(0, 0)?;
    assert_eq!(
        fixture.full(verdict, false),
        lines(&[
            "ran because:",
            "  the working directory changed from `.` to `sub`",
            "  `stdin` changed",
            "  the capture mode changed from split to merged",
            "  the ignore patterns changed from none to `*.o`",
            "  the `track` patterns changed from none to `/proc/cpuinfo`",
        ])
    );
    assert_eq!(
        fixture.full(verdict, true).get(2).map(String::as_str),
        Some("  `stdin` changed from /dev/null to \"hello\\n\"")
    );
    Ok(())
}

/// A changed task program names its block or the loaded module that changed.
#[test]
fn a_changed_program_names_its_block_or_the_loaded_module() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("in.txt", "one")?;
    let markdown = |extra: &str| {
        [
            "# Build",
            "",
            "```starlark ci",
            "load(\"lib.star\", \"flags\")",
            "env.inherit(\"PATH\")",
            &format!("{extra}run(\"tool\", flags)"),
            "```",
            "",
        ]
        .join("\n")
    };
    let tracer = FakeTracer::default();
    let caller: BTreeMap<String, String> = [("PATH".to_owned(), fixture.bin())].into();
    let build = || -> TestResult<Verdict> {
        let file = fixture.path("kizz.md");
        let parsed = parse_tasks(&file, Path::new("kizz.md"), &fs::read_to_string(&file)?);
        let task = TaskInfo::new(parsed.tasks.first().ok_or("no task")?, &["ci".to_owned()])?;
        let program = TaskProgram::load(task, &NoPrint).program?;
        let store = fixture.store()?;
        let engine = Engine::new(&tracer, &store, &NoEcho, Cancellation::new(), None);
        let steps = engine.task_for(TaskIdentity::of(&program));
        let run = program.run(&caller, &steps, &NoPrint);
        run.result.map_err(|failure| failure.to_string())?;
        steps.finish(true)?;
        let verdict = steps
            .decisions()
            .first()
            .ok_or("no decision")?
            .verdict
            .clone();
        drop(steps);
        drop(engine);
        store.close()?;
        Ok(verdict)
    };

    fixture.write("kizz.md", &markdown(""))?;
    fixture.write("lib.star", "flags = \"read:in.txt\"\n")?;
    build()?;
    assert_eq!(build()?, Verdict::Cached);

    fixture.write("lib.star", "# Edited.\nflags = \"read:in.txt\"\n")?;
    assert_eq!(
        fixture.full(&build()?, false),
        lines(&[
            "ran because:",
            "  the task program changed: module `lib.star`"
        ])
    );

    fixture.write("kizz.md", &markdown("# Edited.\n"))?;
    assert_eq!(
        fixture.full(&build()?, false),
        lines(&["ran because:", "  the task program changed: its block"])
    );
    Ok(())
}

/// Changed inputs name every kind of last writer: another task in this build, an
/// earlier build, and something outside kizz.
#[test]
fn changed_inputs_name_who_last_wrote_them() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let generate = fixture.task(0, 3, "Generate")?;
    let consume = fixture.task(1, 9, "Consume")?;
    let read = fixture.step(&["tool", "read:gen.txt"])?;
    let write = |text: &str| fixture.step(&["tool", &format!("write:gen.txt={text}")]);

    fixture.build(&tracer, &[
        (&generate, &[&write("one")?]),
        (&consume, &[&read]),
    ])?;

    // Another task in this build.
    let built = fixture.build(&tracer, &[
        (&generate, &[&write("three")?]),
        (&consume, &[&read]),
    ])?;
    assert_eq!(
        fixture.full(built.verdict(1, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            "    `gen.txt`: size changed from 3 to 5 bytes; last written by step 1 of \
             kizz.md:3 (Generate) in this build",
        ])
    );

    // An earlier build.
    fixture.build(&tracer, &[(&generate, &[&write("fourth!")?])])?;
    let built = fixture.build(&tracer, &[(&consume, &[&read])])?;
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            "    `gen.txt`: size changed from 5 to 7 bytes; last written by step 1 of \
             kizz.md:3 (Generate) in an earlier build",
        ])
    );

    // Something outside kizz: the last write kizz recorded is older than the state
    // the step recorded.
    fixture.write("gen.txt", "edited by hand")?;
    let built = fixture.build(&tracer, &[(&consume, &[&read])])?;
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            "    `gen.txt`: size changed from 7 to 14 bytes; last written by something \
             outside kizz",
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        "`gen.txt` changed (size changed from 7 to 14 bytes)"
    );

    // A path no step ever wrote.
    fixture.write("other.txt", "a")?;
    let reader = fixture.step(&["tool", "read:other.txt"])?;
    fixture.build(&tracer, &[(&consume, &[&reader])])?;
    fixture.write("other.txt", "ab")?;
    let built = fixture.build(&tracer, &[(&consume, &[&reader])])?;
    let Verdict::Ran(explanation) = built.verdict(0, 0)? else {
        return Err("the reader must run".into());
    };
    let writers: Vec<&Writer> = explanation
        .reasons
        .iter()
        .flat_map(Reason::inputs)
        .map(|input| &input.writer)
        .collect();
    assert_eq!(writers, [&Writer::Outside]);
    Ok(())
}

/// The report of `kizz why` for a project whose one task, `kizz.md:3 (Build)`, ran
/// `tool read:mode.txt` once because `input` changed.
fn why_for_one_changed_input(input: &str) -> String {
    format!(
        "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool read:mode.txt`
    ran because:
      1 input changed:
        {input}
    untracked /proc and /sys accesses: none
    ignored accesses: 0
"
    )
}

/// A permission change and an owner change name their old and new values in the
/// full reasons, in the one-line reason of `kizz build`, and in `kizz why`. Tests
/// cannot change owners, so the test writes another owner into the stored record.
#[test]
fn permission_and_owner_changes_name_their_old_and_new_values() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("mode.txt", "same")?;
    let path = fixture.path("mode.txt");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let read = fixture.step(&["tool", "read:mode.txt"])?;
    fixture.build(&tracer, &[(&task, &[&read])])?;

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let built = fixture.build(&tracer, &[(&task, &[&read])])?;
    let input = "`mode.txt`: permissions changed from 0644 to 0600; last written by something \
                 outside kizz";
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            &format!("    {input}")
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        "`mode.txt` changed (permissions changed from 0644 to 0600)"
    );
    assert_eq!(fixture.why(false)?, why_for_one_changed_input(input));

    let metadata = fs::metadata(&path)?;
    let current = Owner {
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    let other = Owner {
        uid: current.uid.wrapping_add(1),
        gid: current.gid.wrapping_add(1),
    };
    let store = fixture.store()?;
    let mut txn = store.write()?;
    for (key, mut record) in txn.scan::<StepRecord>(&[])? {
        for observation in &mut record.observations {
            if observation.path == path
                && let PathState::File { meta, .. } = &mut observation.state
            {
                meta.owner = other;
            }
        }
        txn.put(&key, &record)?;
    }
    txn.commit()?;
    store.close()?;

    let built = fixture.build(&tracer, &[(&task, &[&read])])?;
    let input = format!(
        "`mode.txt`: owner changed from {other} to {current}; last written by something \
         outside kizz"
    );
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            &format!("    {input}")
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        format!("`mode.txt` changed (owner changed from {other} to {current})")
    );
    assert_eq!(fixture.why(false)?, why_for_one_changed_input(&input));
    assert_eq!(tracer.runs(), 3);
    Ok(())
}

/// Set the modification time of `path` to `millis` milliseconds after the Unix
/// epoch, which also gives it a new change time, and return its change time in
/// nanoseconds since the epoch.
fn set_modified(
    path: &Path,
    millis: u64,
) -> TestResult<i64> {
    let then = UNIX_EPOCH
        .checked_add(Duration::from_millis(millis))
        .ok_or("time out of range")?;
    File::open(path)?.set_times(FileTimes::new().set_modified(then))?;
    let metadata = fs::metadata(path)?;
    Ok(metadata
        .ctime()
        .checked_mul(1_000_000_000)
        .and_then(|nanos| nanos.checked_add(metadata.ctime_nsec()))
        .ok_or("change time out of range")?)
}

/// A time in nanoseconds since the Unix epoch as explanations show it: seconds with
/// nine decimals.
fn seconds(nanos: i64) -> String {
    format!(
        "{}.{:09}",
        nanos.div_euclid(1_000_000_000),
        nanos.rem_euclid(1_000_000_000)
    )
}

/// With `timestamps = True`, a new modification time with the same bytes names the
/// old and new times, and the change time when it changed too, in the full reasons,
/// in the one-line reason of `kizz build`, and in `kizz why`; switching the option
/// off names the option, with its old and new values, in all three.
#[test]
fn timestamp_changes_and_the_timestamps_option_are_explained() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("mode.txt", "same")?;
    let path = fixture.path("mode.txt");
    let old_ctime = set_modified(&path, 1_700_000_000_000)?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let mut stamped = fixture.step(&["tool", "read:mode.txt"])?;
    stamped.timestamps = true;
    fixture.build(&tracer, &[(&task, &[&stamped])])?;
    assert_eq!(
        fixture
            .build(&tracer, &[(&task, &[&stamped])])?
            .verdict(0, 0)?,
        &Verdict::Cached
    );

    let new_ctime = set_modified(&path, 1_700_000_001_500)?;
    let built = fixture.build(&tracer, &[(&task, &[&stamped])])?;
    let modification = "timestamps changed: modification time from 1700000000.000000000 to \
                        1700000001.500000000";
    let change = if new_ctime == old_ctime {
        modification.to_owned()
    } else {
        format!(
            "{modification}, change time from {} to {}",
            seconds(old_ctime),
            seconds(new_ctime)
        )
    };
    let input = format!("`mode.txt`: {change}; last written by something outside kizz");
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            &format!("    {input}")
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        format!("`mode.txt` changed ({change})")
    );
    assert_eq!(fixture.why(false)?, why_for_one_changed_input(&input));

    let plain = fixture.step(&["tool", "read:mode.txt"])?;
    let built = fixture.build(&tracer, &[(&task, &[&plain])])?;
    let option = "the `timestamps` option changed from True to False";
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&["ran because:", &format!("  {option}")])
    );
    assert_eq!(fixture.render(false).one_line(built.verdict(0, 0)?), option);
    assert_eq!(
        fixture.why(false)?,
        format!(
            "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool read:mode.txt`
    ran because:
      {option}
    untracked /proc and /sys accesses: none
    ignored accesses: 0
"
        )
    );

    // Switching back finds the record of the step key with the option, which
    // nothing changed since.
    let built = fixture.build(&tracer, &[(&task, &[&stamped])])?;
    assert_eq!(built.verdict(0, 0)?, &Verdict::Cached);
    assert_eq!(tracer.runs(), 3);
    Ok(())
}

/// The report of `kizz why` for a project whose one task, `kizz.md:3 (Build)`, ran
/// `tool list:listed` once because `input` changed.
fn why_for_one_changed_listing(input: &str) -> String {
    format!(
        "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool list:listed`
    ran because:
      1 input changed:
        {input}
    untracked /proc and /sys accesses: none
    ignored accesses: 0
"
    )
}

/// A listed directory whose recorded entries are its current entries in reverse
/// order runs its step with an order change, and one with a new entry runs it with
/// an entries change that names the entry, in the full reasons, in the one-line
/// reason of `kizz build`, and in `kizz why`. The test writes the reversed listing,
/// with the hashes of its order, into the stored record.
#[test]
fn order_and_entries_changes_of_a_listed_directory_are_explained() -> TestResult {
    let fixture = Fixture::new()?;
    for name in ["a", "b", "c"] {
        fixture.write(&format!("listed/{name}"), name)?;
    }
    let path = fixture.path("listed");
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let list = fixture.step(&["tool", "list:listed"])?;
    fixture.build(&tracer, &[(&task, &[&list])])?;

    let store = fixture.store()?;
    let mut txn = store.write()?;
    for (key, mut record) in txn.scan::<StepRecord>(&[])? {
        for observation in &mut record.observations {
            if observation.path == path
                && let PathState::Directory {
                    listing: Some(listing),
                    ..
                } = &mut observation.state
            {
                let entries = listing.entries.clone().ok_or("no entries")?;
                *listing = observe::listing(entries.into_iter().rev().collect());
            }
        }
        txn.put(&key, &record)?;
    }
    txn.commit()?;
    store.close()?;

    let built = fixture.build(&tracer, &[(&task, &[&list])])?;
    let input = "`listed`: the order of its entries changed; last written by something outside \
                 kizz";
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            &format!("    {input}")
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        "`listed` changed (the order of its entries changed)"
    );
    assert_eq!(fixture.why(false)?, why_for_one_changed_listing(input));

    fixture.write("listed/d", "d")?;
    let built = fixture.build(&tracer, &[(&task, &[&list])])?;
    let input = "`listed`: directory entries changed: `d` appeared; last written by something \
                 outside kizz";
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            &format!("    {input}")
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        "`listed` changed (directory entries changed: `d` appeared)"
    );
    assert_eq!(fixture.why(false)?, why_for_one_changed_listing(input));

    let built = fixture.build(&tracer, &[(&task, &[&list])])?;
    assert_eq!(built.verdict(0, 0)?, &Verdict::Cached);
    assert_eq!(tracer.runs(), 3);
    Ok(())
}

/// A step that writes a file and also reads it: when the file changed because this
/// command wrote it on its previous run, the explanation says so directly.
#[test]
fn a_step_that_writes_a_file_and_also_reads_it_is_explained_directly() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "State")?;
    let short = fixture.step(&["tool", "rw:state.txt=aa"])?;
    let long = fixture.step(&["tool", "rw:state.txt=bbb"])?;
    fixture.build(&tracer, &[(&task, &[&short])])?;
    fixture.build(&tracer, &[(&task, &[&long])])?;
    let built = fixture.build(&tracer, &[(&task, &[&short])])?;
    let verdict = built.verdict(0, 0)?;
    assert_eq!(
        fixture.full(verdict, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            "    `state.txt`: size changed from 2 to 3 bytes; last written by this command on \
             its previous run",
            "      re-ran because `state.txt` changed; this command writes that file and also \
             reads it",
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(verdict),
        "re-ran because `state.txt` changed; this command writes that file and also reads it"
    );
    let Verdict::Ran(explanation) = verdict else {
        return Err("the step must run".into());
    };
    assert_eq!(
        explanation
            .reasons
            .iter()
            .flat_map(Reason::inputs)
            .map(|input| input.self_inflicted.clone())
            .collect::<Vec<_>>(),
        [Some(SelfInflicted::WritesAndReads)]
    );
    Ok(())
}

/// The self-inflicted causes of `verdict`'s changed inputs, in order.
fn self_inflicted(verdict: &Verdict) -> TestResult<Vec<Option<SelfInflicted>>> {
    let Verdict::Ran(explanation) = verdict else {
        return Err("the step must run".into());
    };
    Ok(explanation
        .reasons
        .iter()
        .flat_map(Reason::inputs)
        .map(|input| input.self_inflicted.clone())
        .collect())
}

/// A step that writes a file and also reads it, whose file is edited outside kizz
/// before each of two consecutive builds, is not blamed for the change: the writer
/// is something outside kizz, and neither the direct explanation nor the warning
/// for consecutive builds appears.
#[test]
fn consecutive_external_edits_of_a_file_a_step_writes_and_reads_are_not_self_inflicted()
-> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("state.txt", "a")?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "State")?;
    let bump = fixture.step(&["tool", "bump:state.txt"])?;
    fixture.build(&tracer, &[(&task, &[&bump])])?;

    fixture.write("state.txt", "xyz")?;
    let second = fixture.build(&tracer, &[(&task, &[&bump])])?;
    // The step reads the file and then rewrites it, so its result is never cached,
    // and each explanation starts with that reason.
    assert_eq!(
        fixture.full(second.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            REWROTE_STATE,
            "  1 input changed since:",
            "    `state.txt`: size changed from 2 to 3 bytes; last written by something \
             outside kizz",
        ])
    );
    assert_eq!(self_inflicted(second.verdict(0, 0)?)?, [None]);
    assert_eq!(second.warnings, Vec::<RepeatedWarning>::new());

    fixture.write("state.txt", "edited!")?;
    let third = fixture.build(&tracer, &[(&task, &[&bump])])?;
    let verdict = third.verdict(0, 0)?;
    assert_eq!(
        fixture.full(verdict, false),
        lines(&[
            "ran because:",
            REWROTE_STATE,
            "  1 input changed since:",
            "    `state.txt`: size changed from 4 to 7 bytes; last written by something \
             outside kizz",
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(verdict),
        "the previous run could not be cached: a file changed after the command read it: \
         `state.txt` (the command wrote it); `state.txt` changed (size changed from 4 to 7 \
         bytes)"
    );
    assert_eq!(self_inflicted(verdict)?, [None]);
    assert_eq!(third.warnings, Vec::<RepeatedWarning>::new());

    let store = fixture.store()?;
    let report = why(&store, fixture.project.path())?;
    store.close()?;
    assert_eq!(report.warnings(), Vec::<RepeatedWarning>::new());
    Ok(())
}

/// A step that writes a file and also reads it, whose file another task rewrites
/// in each of two consecutive builds, names that task as the writer and is not
/// blamed for the change: neither the direct explanation nor the warning for
/// consecutive builds appears.
#[test]
fn another_writer_of_a_file_a_step_writes_and_reads_is_not_self_inflicted() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let other = fixture.task(0, 3, "Other")?;
    let state = fixture.task(1, 9, "State")?;
    let write = |text: &str| fixture.step(&["tool", &format!("write:state.txt={text}")]);
    let bump = fixture.step(&["tool", "bump:state.txt"])?;
    fixture.build(&tracer, &[(&other, &[&write("a")?]), (&state, &[&bump])])?;

    let second = fixture.build(&tracer, &[(&other, &[&write("bbbb")?]), (&state, &[&bump])])?;
    assert_eq!(
        fixture.full(second.verdict(1, 0)?, false),
        lines(&[
            "ran because:",
            REWROTE_STATE,
            "  1 input changed since:",
            "    `state.txt`: size changed from 2 to 4 bytes; last written by step 1 of \
             kizz.md:3 (Other) in this build",
        ])
    );
    assert_eq!(self_inflicted(second.verdict(1, 0)?)?, [None]);
    assert_eq!(second.warnings, Vec::<RepeatedWarning>::new());

    let third = fixture.build(&tracer, &[
        (&other, &[&write("cccccccc")?]),
        (&state, &[&bump]),
    ])?;
    let verdict = third.verdict(1, 0)?;
    assert_eq!(
        fixture.full(verdict, false),
        lines(&[
            "ran because:",
            REWROTE_STATE,
            "  1 input changed since:",
            "    `state.txt`: size changed from 5 to 8 bytes; last written by step 1 of \
             kizz.md:3 (Other) in this build",
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(verdict),
        "the previous run could not be cached: a file changed after the command read it: \
         `state.txt` (the command wrote it); `state.txt` changed (size changed from 5 to 8 \
         bytes)"
    );
    assert_eq!(self_inflicted(verdict)?, [None]);
    assert_eq!(third.warnings, Vec::<RepeatedWarning>::new());

    // With nothing changed since, the step runs again only because its previous
    // run was not cached, and it rewrites the file once more.
    let fourth = fixture.build(&tracer, &[(&state, &[&bump])])?;
    assert_eq!(
        fixture.full(fourth.verdict(0, 0)?, false),
        lines(&["ran because:", REWROTE_STATE])
    );
    // Rewritten in an earlier build, the file names that build's writer instead.
    fixture.build(&tracer, &[(&other, &[&write("dd")?])])?;
    let sixth = fixture.build(&tracer, &[(&state, &[&bump])])?;
    assert_eq!(
        fixture.full(sixth.verdict(0, 0)?, false),
        lines(&[
            "ran because:",
            REWROTE_STATE,
            "  1 input changed since:",
            "    `state.txt`: size changed from 10 to 2 bytes; last written by step 1 of \
             kizz.md:3 (Other) in an earlier build",
        ])
    );
    assert_eq!(self_inflicted(sixth.verdict(0, 0)?)?, [None]);
    assert_eq!(sixth.warnings, Vec::<RepeatedWarning>::new());
    Ok(())
}

/// A step that re-runs in consecutive builds because a step that runs on every
/// build rewrites its input gets the loud warning, in its explanation, in the
/// engine's warnings, and in `kizz why`.
#[test]
fn consecutive_self_inflicted_reruns_get_a_loud_warning() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("shared.txt", "")?;
    let tracer = FakeTracer::default();
    let stamp = fixture.task(0, 3, "Stamp")?;
    let check = fixture.task(1, 9, "Check")?;
    let mut bump = fixture.step(&["tool", "bump:shared.txt"])?;
    bump.always = true;
    let read = fixture.step(&["tool", "read:shared.txt"])?;
    let tasks: [(&TaskIdentity, &[&StepSpec]); 2] = [(&stamp, &[&bump]), (&check, &[&read])];

    let first = fixture.build(&tracer, &tasks)?;
    assert_eq!(first.warnings, Vec::<RepeatedWarning>::new());
    let second = fixture.build(&tracer, &tasks)?;
    assert_eq!(
        fixture.full(second.verdict(0, 0)?, false),
        lines(&["ran because:", "  marked `always`"])
    );
    assert_eq!(
        fixture.full(second.verdict(1, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            "    `shared.txt`: size changed from 1 to 2 bytes; last written by step 1 of \
             kizz.md:3 (Stamp) in this build",
            "      re-ran because `shared.txt` changed; step 1 of kizz.md:3 (Stamp) writes that \
             file on every build",
        ])
    );
    // One self-inflicted re-run is not yet a pattern.
    assert_eq!(second.warnings, Vec::<RepeatedWarning>::new());

    let third = fixture.build(&tracer, &tasks)?;
    let warning = "re-ran in consecutive builds for the same self-inflicted reason: \
                   `shared.txt` changed, and the build itself changes it every time; add an \
                   ignore pattern if it is not a real input";
    assert_eq!(
        fixture.full(third.verdict(1, 0)?, false),
        lines(&[
            "ran because:",
            "  1 input changed:",
            "    `shared.txt`: size changed from 2 to 3 bytes; last written by step 1 of \
             kizz.md:3 (Stamp) in this build",
            "      re-ran because `shared.txt` changed; step 1 of kizz.md:3 (Stamp) writes that \
             file on every build",
            &format!("warning: {warning}"),
        ])
    );
    assert_eq!(third.warnings.len(), 1);
    let rendered: Vec<String> = third
        .warnings
        .iter()
        .map(|warning| fixture.render(false).repeated(warning))
        .collect();
    assert_eq!(rendered, [format!(
        "step 1 of kizz.md:9 (Check) (`tool read:shared.txt`) {warning}"
    )]);

    let store = fixture.store()?;
    let report = why(&store, fixture.project.path())?;
    store.close()?;
    assert_eq!(report.warnings(), third.warnings);
    Ok(())
}

/// A command marked `always` says so, and a previous run that could not be cached
/// says why.
#[test]
fn always_and_results_that_were_not_cached_give_their_reasons() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let mut always = fixture.step(&["tool", "always"])?;
    always.always = true;
    let killed = fixture.step(&["tool", "signal"])?;
    let built = fixture.build(&tracer, &[(&task, &[&always, &killed])])?;
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 0)?),
        "never run: there is no record for this step"
    );
    let built = fixture.build(&tracer, &[(&task, &[&always, &killed])])?;
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&["ran because:", "  marked `always`"])
    );
    assert_eq!(
        fixture.full(built.verdict(0, 1)?, false),
        lines(&[
            "ran because:",
            "  the previous run could not be cached: killed by signal 9"
        ])
    );
    assert_eq!(
        fixture.render(false).one_line(built.verdict(0, 1)?),
        "the previous run could not be cached: killed by signal 9"
    );
    Ok(())
}

/// Within one build, a step that ran keeps its reasons when a later pass finds it
/// cached, and `kizz why` prints the reasons of the most recent build, with the
/// positions the last run no longer reached removed.
#[test]
fn why_reports_the_most_recent_build_of_each_step() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("in.txt", "one")?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let mut first = fixture.step(&[
        "tool",
        "read:in.txt",
        "kernel:/proc/cpuinfo",
        "probe:/sys/kernel",
        "read:scratch.tmp",
    ])?;
    first.ignore = vec!["*.tmp".to_owned()];
    let second = fixture.step(&["tool", "second"])?;

    let built = fixture.build_passes(&tracer, &[(&task, &[&first, &second])], 2, false)?;
    assert_eq!(built.verdict(1, 0)?, &Verdict::Cached);
    assert_eq!(
        fixture.why(false)?,
        "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool read:in.txt kernel:/proc/cpuinfo probe:/sys/kernel read:scratch.tmp`
    ran because:
      never run: there is no record for this step
    untracked /proc and /sys accesses: `/proc/cpuinfo` (read), `/sys/kernel` (probe)
    ignored accesses: 1
  step 2: `tool second`
    ran because:
      never run: there is no record for this step
    untracked /proc and /sys accesses: none
    ignored accesses: 0
"
    );

    // A later build that reaches only the first step, which is cached.
    fixture.build(&tracer, &[(&task, &[&first])])?;
    let why = fixture.why(false)?;
    assert!(why.contains("  step 1: `tool read:in.txt"), "{why}");
    assert!(
        why.contains("    cached: no recorded input changed\n"),
        "{why}"
    );
    assert!(!why.contains("step 2"), "{why}");

    let store = fixture.store()?;
    let txn = store.read()?;
    let positions = txn.scan::<StepPosition>(&task.id.key_prefix())?;
    assert_eq!(positions.len(), 1);
    let record = txn.get::<TaskRecord>(&task.id)?.ok_or("no task record")?;
    assert_eq!((record.steps, record.line), (1, 3));
    drop(txn);
    store.close()?;
    Ok(())
}

/// `kizz why` shows only the most recent build's tasks under the path, and says
/// when no build recorded any.
#[test]
fn why_selects_the_most_recent_build_under_the_path() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let older = fixture.task(0, 3, "Older")?;
    let newer = fixture.task(1, 9, "Newer")?;
    let step = fixture.step(&["tool"])?;
    fixture.build(&tracer, &[(&older, &[&step])])?;
    fixture.build(&tracer, &[(&newer, &[&step])])?;
    let why = fixture.why(false)?;
    assert!(
        why.starts_with("The most recent build ran 1 task under `.`.\n"),
        "{why}"
    );
    assert!(why.contains("kizz.md:9 (Newer) [ci]"), "{why}");
    assert!(!why.contains("Older"), "{why}");

    let store = fixture.store()?;
    let report = why_under(&store, &fixture.path("elsewhere"))?;
    store.close()?;
    assert_eq!(
        fixture.render(false).why(&report),
        "No build has recorded a task under `elsewhere`.\n"
    );
    Ok(())
}

/// The report of `kizz why` for `path`.
fn why_under(
    store: &Store,
    path: &Path,
) -> TestResult<kizz_core::WhyReport> {
    Ok(why(store, path)?)
}

/// The explanation of a run carries every reason in order; an explanation with no
/// reasons renders as `ran because:` alone.
#[test]
fn explanations_render_every_reason_in_order() {
    let render = Render {
        base: PathBuf::from("/w"),
        home: Some(PathBuf::from("/home/u")),
        verbose: false,
    };
    let verdict = Verdict::Ran(Explanation {
        reasons: vec![Reason::Always, Reason::NeverRun],
        repeated: Vec::new(),
    });
    assert_eq!(
        render.one_line(&verdict),
        "marked `always`; never run: there is no record for this step"
    );
    assert_eq!(render.full(&Verdict::Ran(Explanation::default())), [
        "ran because:"
    ]);
}

/// The report of `kizz why` for a project whose one task, `kizz.md:3 (Build)`, ran
/// `tool unknown:1000000 unknown:400` once, with the lines `reasons` and the line of
/// the two system calls that the tracer does not know.
fn why_for_unknown_system_calls(reasons: &str) -> String {
    format!(
        "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool unknown:1000000 unknown:400`
{reasons}
    untracked /proc and /sys accesses: none
    system calls that the tracer does not know: 400, 1000000
    ignored accesses: 0
"
    )
}

/// A run that made system calls that the tracer does not know is not cached
/// without the option: the next build runs the step again, and its full reasons,
/// its one-line reason and `kizz why` name each number, and `kizz why` lists them on
/// a line of their own. With the option, the result is cached: `kizz why` still
/// lists the numbers, and a later build without the option is a cache hit.
#[test]
fn unknown_system_calls_are_explained() -> TestResult {
    let fixture = Fixture::new()?;
    let tracer = FakeTracer::default();
    let task = fixture.task(0, 3, "Build")?;
    let step = fixture.step(&["tool", "unknown:1000000", "unknown:400"])?;
    fixture.build(&tracer, &[(&task, &[&step])])?;

    let built = fixture.build(&tracer, &[(&task, &[&step])])?;
    let reason = "the previous run could not be cached: the command made system calls 400 and \
                  1000000, which the tracer does not know";
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&["ran because:", &format!("  {reason}")])
    );
    assert_eq!(fixture.render(false).one_line(built.verdict(0, 0)?), reason);
    assert_eq!(
        fixture.why(false)?,
        why_for_unknown_system_calls(&format!("    ran because:\n      {reason}"))
    );
    assert_eq!(tracer.runs(), 2);

    // With the option, the run is cached, and its record still lists the numbers.
    let built = fixture.build_passes(&tracer, &[(&task, &[&step])], 1, true)?;
    assert_eq!(
        fixture.full(built.verdict(0, 0)?, false),
        lines(&["ran because:", &format!("  {reason}")])
    );
    let built = fixture.build(&tracer, &[(&task, &[&step])])?;
    assert_eq!(built.verdict(0, 0)?, &Verdict::Cached);
    assert_eq!(
        fixture.why(false)?,
        why_for_unknown_system_calls("    cached: no recorded input changed")
    );
    assert_eq!(tracer.runs(), 3);
    Ok(())
}
