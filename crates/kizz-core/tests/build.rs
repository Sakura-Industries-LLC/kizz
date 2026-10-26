// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tests of builds in passes: the default and learned order, failed tasks that run
//! again once a path they looked for appears, failures from the cache, the pass
//! limit, `always` commands, interruption, and task programs whose `fs.write`
//! changes a file, which is work in its pass and makes the task the file's last
//! writer.
//!
//! Each test writes `kizz.md` files into a temporary project directory and opens
//! the store in a temporary cache directory. A build loads the project, selects the
//! tasks of target `ci`, and runs them with one [`Engine`] through [`build`]. Every
//! task inherits `PATH` from the caller environment, which holds the project's
//! `bin` directory with an executable `tool`. The fake tracer performs the file
//! operations that a command's arguments name on real files and reports them:
//!
//! * `read:PATH` reads a file, which may be missing;
//! * `need:PATH` reads a file and exits with status 1 when it is missing;
//! * `write:PATH=TEXT` writes `TEXT` to a file;
//! * `bump:PATH` reads a file and writes it back with one more `+`;
//! * `exe:PATH` writes an executable program;
//! * `list:PATH` lists a directory;
//! * `interrupt` requests cancellation, as Ctrl-C does, and a signal kills the command;
//!
//! and any other argument does nothing. Relative paths are relative to the
//! command's working directory, the task directory. As the Linux tracer does, the
//! fake notes the metadata of each file that a command reads or runs before it
//! writes it, at the moment of the read, so a `bump` leaves a result that is not
//! cached, because the file changed after the command read it.

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
    fmt::Write as _,
    fs, io,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use kizz_core::{
    BuildEvents, BuildOutcome, BuildReport, ChangingInput, Engine, INTERRUPTED, Pass, PrintSink,
    Render, TargetExpr, TaskFailure, TaskInfo, TaskResult, UncheckedStep, WorkerPool, build, load,
    task_id,
};
use kizz_store::{
    BuildId, Change, LearnedOrder, MachineChange, Reason, Store, TaskId, TaskLabel, TaskRecord,
    Verdict, Writer,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CapturedOutput, EchoSink, FirstReads,
    MachineState, Memory, OutputStream, ProcessTree, RunRequest, RunResult, Termination,
    TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A tracer failure from `error`.
fn tracer_failure(error: &dyn Error) -> TraceError {
    TraceError::Tracer {
        message: error.to_string(),
        source: None,
    }
}

/// What a run of `request` does to real files and reports, as the module
/// documentation describes; `cancel` is the build's cancellation flag.
fn script(
    request: &RunRequest,
    cancel: &Cancellation,
) -> Result<RunResult, TraceError> {
    let cwd = request.cwd.as_path();
    let mut accesses = Accesses::new();
    let mut first_reads = FirstReads::new();
    first_reads.note_at(&request.program, &accesses);
    accesses.record(request.program.clone(), AccessKind::Exec);
    let mut termination = Termination::Exited(0);
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
            "need" => {
                if fs::read(&path).is_err() {
                    termination = Termination::Exited(1);
                }
                record(path, AccessKind::Read)?;
            },
            "write" => {
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
            "exe" => {
                fs::write(&path, "#!/bin/sh\n").map_err(failed)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(failed)?;
                record(path, AccessKind::Write)?;
            },
            "list" => {
                let _entries: Vec<fs::DirEntry> = fs::read_dir(&path)
                    .map(|entries| entries.filter_map(Result::ok).collect())
                    .unwrap_or_default();
                record(path, AccessKind::List)?;
            },
            "interrupt" => {
                cancel.cancel();
                termination = Termination::Signaled(2);
            },
            _ => {},
        }
    }
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

/// A tracer that runs nothing, follows [`script`], and keeps every command it was
/// asked to run; its machine query reports one more byte of available memory at
/// each query, and knows no other part.
#[derive(Default)]
struct FakeTracer {
    /// The argument vector of every run so far, joined with spaces.
    runs: Mutex<Vec<String>>,
    /// The number of machine queries so far.
    machine_queries: AtomicU64,
}

impl FakeTracer {
    /// The argument vector of every run so far, joined with spaces.
    fn runs(&self) -> Vec<String> {
        self.runs
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
        cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        let command: Vec<String> = request
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command.join(" "));
        script(request, cancel)
    }

    /// The tests of builds read no extended attributes, so there are none.
    fn extended_attributes(
        &self,
        _path: &std::path::Path,
        _follow: bool,
    ) -> io::Result<kizz_trace::ExtendedAttributes> {
        Ok(kizz_trace::ExtendedAttributes::new())
    }

    /// The available memory is the number of queries so far, this one included,
    /// so it differs at every query.
    fn machine_state(
        &self,
        _cwd: &std::path::Path,
    ) -> MachineState {
        let queries = self
            .machine_queries
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        MachineState {
            memory: Some(Memory {
                total: 1_000,
                available: queries,
            }),
            ..MachineState::default()
        }
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

/// Keeps what a build reported as it ran.
#[derive(Default)]
struct Events {
    /// Each pass that started, then each task run that ended, as text.
    seen: Mutex<Vec<String>>,
}

impl Events {
    /// What the build reported, in order.
    fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keep `line`.
    fn push(
        &self,
        line: String,
    ) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
}

impl BuildEvents for Events {
    fn pass_started(
        &self,
        pass: usize,
    ) {
        self.push(format!("pass {pass}"));
    }

    fn task_finished(
        &self,
        pass: usize,
        result: &TaskResult,
    ) {
        self.push(format!("pass {pass}: {}", describe(result)));
    }
}

/// A task run as text: its heading, whether a command ran, and whether it failed.
fn describe(result: &TaskResult) -> String {
    format!(
        "{} {} {}",
        result.task.name.as_deref().unwrap_or("?"),
        if result.ran() { "ran" } else { "cached" },
        if result.failed() { "failed" } else { "ok" },
    )
}

/// Each task run of `pass`, as [`describe`] shows it.
fn summary(pass: &Pass) -> Vec<String> {
    pass.tasks.iter().map(describe).collect()
}

/// Each pass of `report`, as [`summary`] shows it.
fn passes(report: &BuildReport) -> Vec<Vec<String>> {
    report.passes.iter().map(summary).collect()
}

/// Lines from string slices.
fn lines(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|&line| line.to_owned()).collect()
}

/// What one build reported.
struct Built {
    /// The report.
    report: BuildReport,
    /// The identity of the build.
    id: BuildId,
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

    /// Write the `kizz.md` in the directory `dir` of the project with one task of
    /// target `ci` for each heading and program in `tasks`; each program inherits
    /// `PATH` first.
    fn kizz(
        &self,
        dir: &str,
        tasks: &[(&str, &str)],
    ) -> TestResult {
        let mut markdown = String::from("# Tasks\n");
        for (heading, program) in tasks {
            let block = ["```starlark ci", "env.inherit(\"PATH\")", program, "```"].join("\n");
            write!(markdown, "\n## {heading}\n\n{block}\n")?;
        }
        let file = if dir.is_empty() {
            "kizz.md".to_owned()
        } else {
            format!("{dir}/kizz.md")
        };
        self.write(&file, &markdown)
    }

    /// The task of the `kizz.md` in `dir`, relative to the project, with index
    /// `index`.
    fn id(
        &self,
        dir: &str,
        index: u32,
    ) -> TaskId {
        TaskId {
            file: self.project.path().join(dir).join("kizz.md"),
            index,
        }
    }

    /// The tasks of target `ci`, in the default order.
    fn tasks(&self) -> TestResult<Vec<TaskInfo>> {
        let project = load(self.project.path(), &WorkerPool::new()?)?;
        let target: TargetExpr = "ci".parse()?;
        let mut tasks = Vec::new();
        for selected in target.select(&project.tasks) {
            tasks.push(TaskInfo::new(selected.task, &selected.selected)?);
        }
        Ok(tasks)
    }

    /// Open the store in the cache.
    fn store(&self) -> TestResult<Store> {
        Ok(Store::open(self.cache.path())?)
    }

    /// Run one build of the tasks of target `ci` with `tracer` and `cancel`,
    /// reporting to `events`.
    fn build_with(
        &self,
        tracer: &FakeTracer,
        cancel: Cancellation,
        events: &dyn BuildEvents,
    ) -> TestResult<Built> {
        self.build_full(tracer, cancel, events, &[])
    }

    /// Run one build of the tasks of target `ci` with `tracer` and `cancel`,
    /// reporting to `events`, with the variables `vars` in the caller environment
    /// besides `PATH`.
    fn build_full(
        &self,
        tracer: &FakeTracer,
        cancel: Cancellation,
        events: &dyn BuildEvents,
        vars: &[(&str, &str)],
    ) -> TestResult<Built> {
        let tasks = self.tasks()?;
        let mut caller: BTreeMap<String, String> = [(
            "PATH".to_owned(),
            self.path("bin").to_string_lossy().into_owned(),
        )]
        .into();
        for (name, value) in vars {
            caller.insert((*name).to_owned(), (*value).to_owned());
        }
        let store = self.store()?;
        let engine = Engine::new(tracer, &store, &NoEcho, cancel, None, &WorkerPool::new()?);
        let report = build(&engine, &tasks, &caller, &NoPrint, events)?;
        let id = engine.build();
        drop(engine);
        store.close()?;
        Ok(Built { report, id })
    }

    /// Run one build of the tasks of target `ci` with `tracer`.
    fn build(
        &self,
        tracer: &FakeTracer,
    ) -> TestResult<Built> {
        self.build_with(tracer, Cancellation::new(), &Events::default())
    }

    /// Run one build of the tasks of target `ci` with `tracer`, with the variables
    /// `vars` in the caller environment besides `PATH`.
    fn build_vars(
        &self,
        tracer: &FakeTracer,
        vars: &[(&str, &str)],
    ) -> TestResult<BuildReport> {
        Ok(self
            .build_full(tracer, Cancellation::new(), &Events::default(), vars)?
            .report)
    }

    /// Read `relative` in the project.
    fn read(
        &self,
        relative: &str,
    ) -> TestResult<String> {
        Ok(fs::read_to_string(self.path(relative))?)
    }

    /// The producers learned for the task `task`, if any were stored.
    fn learned(
        &self,
        task: &TaskId,
    ) -> TestResult<Option<Vec<TaskId>>> {
        let store = self.store()?;
        let learned = store
            .read()?
            .get::<LearnedOrder>(task)?
            .map(|order| order.producers.into_iter().collect());
        store.close()?;
        Ok(learned)
    }

    /// The task record of `task`.
    fn task_record(
        &self,
        task: &TaskId,
    ) -> TestResult<TaskRecord> {
        let store = self.store()?;
        let record = store.read()?.get::<TaskRecord>(task)?;
        store.close()?;
        Ok(record.ok_or("no task record")?)
    }

    /// Run one build of the tasks of target `ci` with `tracer`, failing task runs
    /// that leave a step unchecked when `fail` is set, as `kizz build
    /// --fail-unchecked` does; return the report, whose standing unchecked exits
    /// ([`BuildReport::unchecked`]) are the build's warnings.
    fn build_unchecked(
        &self,
        tracer: &FakeTracer,
        fail: bool,
    ) -> TestResult<BuildReport> {
        let tasks = self.tasks()?;
        let caller: BTreeMap<String, String> = [(
            "PATH".to_owned(),
            self.path("bin").to_string_lossy().into_owned(),
        )]
        .into();
        let store = self.store()?;
        let engine = Engine::new(
            tracer,
            &store,
            &NoEcho,
            Cancellation::new(),
            None,
            &WorkerPool::new()?,
        )
        .with_fail_unchecked(fail);
        assert_eq!(engine.fails_unchecked(), fail);
        let report = build(&engine, &tasks, &caller, &NoPrint, &Events::default())?;
        drop(engine);
        store.close()?;
        Ok(report)
    }
}

/// The headings of the tasks in the order of `report`, from the fixture's
/// `kizz.md` files.
fn order_headings(report: &BuildReport) -> TestResult<Vec<String>> {
    let first = report.passes.first().ok_or("no pass")?;
    let mut headings = Vec::new();
    for id in &report.order {
        let task = first
            .tasks
            .iter()
            .find(|result| &task_id(&result.task) == id)
            .ok_or("a task in the order did not run")?;
        headings.push(task.task.name.clone().unwrap_or_default());
    }
    Ok(headings)
}

/// The run of the task with heading `heading` in pass `pass` of `report`,
/// counting passes from 1.
fn task_in<'r>(
    report: &'r BuildReport,
    pass: usize,
    heading: &str,
) -> TestResult<&'r TaskResult> {
    let pass = report
        .passes
        .iter()
        .find(|candidate| candidate.number == pass)
        .ok_or("no such pass")?;
    Ok(pass
        .tasks
        .iter()
        .find(|result| result.task.name.as_deref() == Some(heading))
        .ok_or("no such task in the pass")?)
}

/// Whether `label` names the task with heading `heading`.
fn is_task(
    label: &TaskLabel,
    heading: &str,
) -> bool {
    label.heading.as_deref() == Some(heading)
}

/// Two tasks in the wrong order settle in two passes on the first build, and in
/// one pass on the second, because the order has been learned; a later build that
/// changes the producer still takes one pass.
#[test]
fn two_tasks_in_the_wrong_order_settle_in_two_passes_then_one() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        ("Use", "run(\"tool\", \"read:out.txt\")"),
        ("Make", "run(\"tool\", \"write:out.txt=made\")"),
    ])?;
    let tracer = FakeTracer::default();
    let events = Events::default();
    let first = fixture
        .build_with(&tracer, Cancellation::new(), &events)?
        .report;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(order_headings(&first)?, lines(&["Use", "Make"]));
    assert_eq!(passes(&first), vec![
        lines(&["Use ran ok", "Make ran ok"]),
        lines(&["Use ran ok", "Make cached ok"]),
        lines(&["Use cached ok", "Make cached ok"]),
    ]);
    assert_eq!(first.settled_in(), 2);
    assert_eq!(
        tracer.runs(),
        lines(&[
            "tool read:out.txt",
            "tool write:out.txt=made",
            "tool read:out.txt"
        ])
    );
    assert_eq!(
        events.seen(),
        lines(&[
            "pass 1",
            "pass 1: Use ran ok",
            "pass 1: Make ran ok",
            "pass 2",
            "pass 2: Use ran ok",
            "pass 2: Make cached ok",
            "pass 3",
            "pass 3: Use cached ok",
            "pass 3: Make cached ok",
        ])
    );

    // The consumer ran again in the second pass because the producer wrote its
    // input in this build.
    let rerun = task_in(&first, 2, "Use")?;
    let Some(Verdict::Ran(explanation)) = rerun.decisions.first().map(|step| &step.verdict) else {
        return Err("the consumer did not run in the second pass".into());
    };
    let [Reason::InputsChanged { inputs }] = explanation.reasons.as_slice() else {
        return Err(format!("unexpected reasons {:?}", explanation.reasons).into());
    };
    let [input] = inputs.as_slice() else {
        return Err(format!("unexpected inputs {inputs:?}").into());
    };
    assert_eq!(input.path, fixture.path("out.txt"));
    assert_eq!(input.change, Change::Appeared);
    assert!(
        matches!(&input.writer, Writer::ThisBuild { task, position: 0 } if is_task(task, "Make")),
        "{:?}",
        input.writer
    );

    // The learned order: the consumer's producer, and none for the producer.
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );
    assert_eq!(fixture.learned(&fixture.id("", 1))?, None);

    let second = fixture.build(&tracer)?.report;
    assert_eq!(second.outcome, BuildOutcome::Succeeded);
    assert_eq!(order_headings(&second)?, lines(&["Make", "Use"]));
    assert_eq!(passes(&second), vec![lines(&[
        "Make cached ok",
        "Use cached ok"
    ])]);
    assert_eq!(second.settled_in(), 1);
    assert_eq!(tracer.runs().len(), 3);

    // A change to the producer runs it and then its consumer, in one pass.
    fixture.kizz("", &[
        ("Use", "run(\"tool\", \"read:out.txt\")"),
        ("Make", "run(\"tool\", \"write:out.txt=changed\")"),
    ])?;
    let third = fixture.build(&tracer)?.report;
    assert_eq!(third.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&third), vec![
        lines(&["Make ran ok", "Use ran ok"]),
        lines(&["Make cached ok", "Use cached ok"]),
    ]);
    assert_eq!(third.settled_in(), 1);
    assert_eq!(
        tracer.runs().get(3..),
        Some(&lines(&["tool write:out.txt=changed", "tool read:out.txt"])[..])
    );
    Ok(())
}

/// A task that fails because a file it looks for does not exist yet runs again,
/// and succeeds, once a later task creates it; its task record holds the program
/// hash and the summary of its last run.
#[test]
fn a_task_that_fails_for_a_missing_file_runs_again_once_a_later_task_creates_it() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        ("Check", "run(\"tool\", \"need:gen.txt\", expect = [0])"),
        ("Generate", "run(\"tool\", \"write:gen.txt=data\")"),
    ])?;
    let tracer = FakeTracer::default();
    let built = fixture.build(&tracer)?;
    let report = &built.report;
    assert_eq!(report.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(report), vec![
        lines(&["Check ran failed", "Generate ran ok"]),
        lines(&["Check ran ok", "Generate cached ok"]),
        lines(&["Check cached ok", "Generate cached ok"]),
    ]);
    assert_eq!(report.settled_in(), 2);
    let failed = task_in(report, 1, "Check")?;
    assert!(
        matches!(&failed.result, Err(TaskFailure::UnexpectedExit { .. })),
        "{:?}",
        failed.result
    );
    assert_eq!(failed.missing, vec![fixture.path("gen.txt")]);
    assert_eq!(task_in(report, 2, "Check")?.missing, Vec::<PathBuf>::new());
    assert_eq!(
        tracer.runs(),
        lines(&[
            "tool need:gen.txt",
            "tool write:gen.txt=data",
            "tool need:gen.txt"
        ])
    );

    let record = fixture.task_record(&fixture.id("", 0))?;
    assert!(record.succeeded);
    assert_eq!(record.build, built.id);
    assert_eq!(record.steps, 1);
    assert_eq!(record.heading.as_deref(), Some("Check"));
    assert_ne!(record.program_hash, [0; 32]);
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );
    Ok(())
}

/// A task whose program a later task builds fails, listing the program's missing
/// locations, and runs again once the program exists.
#[test]
fn a_task_whose_program_a_later_task_builds_runs_again_once_it_exists() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        ("Use", "run(\"gen\", \"go\")"),
        ("Compile", "run(\"tool\", \"exe:bin/gen\")"),
    ])?;
    let tracer = FakeTracer::default();
    let report = fixture.build(&tracer)?.report;
    assert_eq!(report.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&report), vec![
        lines(&["Use ran failed", "Compile ran ok"]),
        lines(&["Use ran ok", "Compile cached ok"]),
        lines(&["Use cached ok", "Compile cached ok"]),
    ]);
    assert_eq!(report.settled_in(), 2);
    let failed = task_in(&report, 1, "Use")?;
    let Err(TaskFailure::Program { message }) = &failed.result else {
        return Err(format!("unexpected result {:?}", failed.result).into());
    };
    assert!(message.contains("`gen`"), "{message}");
    assert_eq!(failed.missing, vec![fixture.path("bin/gen")]);
    assert_eq!(
        tracer.runs(),
        lines(&["tool exe:bin/gen", "gen go"]),
        "the program that was missing is never started until it exists"
    );
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );
    Ok(())
}

/// A task that keeps failing with unchanged inputs runs once, then fails from the
/// cache; the build fails after the pass that runs nothing, listing each failed
/// task with the paths it looked for and did not find. Once those paths appear,
/// the tasks run again.
#[test]
fn a_task_that_keeps_failing_runs_once_then_fails_from_the_cache() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        ("Check", "run(\"tool\", \"need:never.txt\", expect = [0])"),
        (
            "Flag",
            "if not fs.exists(\"flag.txt\"):\n    fail(\"flag.txt is missing\")",
        ),
        ("Fine", "run(\"tool\", \"write:ok.txt=1\")"),
    ])?;
    let tracer = FakeTracer::default();
    let built = fixture.build(&tracer)?;
    let report = &built.report;
    assert_eq!(passes(report), vec![
        lines(&["Check ran failed", "Flag cached failed", "Fine ran ok"]),
        lines(&[
            "Check cached failed",
            "Flag cached failed",
            "Fine cached ok"
        ]),
    ]);
    assert_eq!(report.settled_in(), 1);
    assert_eq!(
        tracer.runs(),
        lines(&["tool need:never.txt", "tool write:ok.txt=1"]),
        "the failing command runs once"
    );
    let from_cache = task_in(report, 2, "Check")?;
    assert!(
        from_cache
            .decisions
            .iter()
            .all(|decision| decision.verdict == Verdict::Cached)
    );
    let BuildOutcome::Failed { failures } = &report.outcome else {
        return Err(format!("unexpected outcome {:?}", report.outcome).into());
    };
    let listed: Vec<(Option<&str>, &[PathBuf])> = failures
        .iter()
        .map(|failure| (failure.task.name.as_deref(), failure.missing.as_slice()))
        .collect();
    assert_eq!(listed, vec![
        (Some("Check"), &[fixture.path("never.txt")][..]),
        (Some("Flag"), &[fixture.path("flag.txt")][..]),
    ]);
    let flag = failures.get(1).ok_or("no second failure")?;
    assert!(
        flag.failure.to_string().contains("flag.txt is missing"),
        "{}",
        flag.failure
    );
    let record = fixture.task_record(&fixture.id("", 0))?;
    assert!(!record.succeeded);
    assert_eq!(record.build, built.id);

    // A second build has nothing new to try either.
    let again = fixture.build(&tracer)?.report;
    assert_eq!(passes(&again), vec![lines(&[
        "Check cached failed",
        "Flag cached failed",
        "Fine cached ok"
    ])]);
    assert_eq!(again.settled_in(), 1);
    assert!(matches!(again.outcome, BuildOutcome::Failed { .. }));
    assert_eq!(tracer.runs().len(), 2);

    // Once the looked-for paths appear, the failed tasks run again and succeed.
    fixture.write("never.txt", "here")?;
    fixture.write("flag.txt", "here")?;
    let fixed = fixture.build(&tracer)?.report;
    assert_eq!(fixed.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&fixed), vec![
        lines(&["Check ran ok", "Flag cached ok", "Fine cached ok"]),
        lines(&["Check cached ok", "Flag cached ok", "Fine cached ok"]),
    ]);
    assert_eq!(fixed.settled_in(), 1);
    assert_eq!(tracer.runs().len(), 3);
    Ok(())
}

/// Tasks that keep changing each other's inputs stop at the pass limit, one more
/// than the number of tasks, with the paths that keep changing and their writers.
#[test]
fn tasks_that_keep_changing_each_others_inputs_stop_at_the_pass_limit() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        ("Ping", "run(\"tool\", \"read:b.txt\", \"bump:a.txt\")"),
        ("Pong", "run(\"tool\", \"read:a.txt\", \"bump:b.txt\")"),
    ])?;
    let tracer = FakeTracer::default();
    let report = fixture.build(&tracer)?.report;
    assert_eq!(passes(&report), vec![
        lines(&["Ping ran ok", "Pong ran ok"]),
        lines(&["Ping ran ok", "Pong ran ok"]),
        lines(&["Ping ran ok", "Pong ran ok"]),
    ]);
    assert_eq!(tracer.runs().len(), 6);
    let BuildOutcome::NotSettled {
        limit,
        changing,
        written,
        worked,
    } = &report.outcome
    else {
        return Err(format!("unexpected outcome {:?}", report.outcome).into());
    };
    assert_eq!(*limit, 3);
    assert_eq!(report.settled_in(), 3);
    let described: Vec<(PathBuf, Option<&str>, Option<&str>)> = changing
        .iter()
        .map(|input: &ChangingInput| {
            let writer = if let Writer::ThisBuild { task, .. } = &input.writer {
                task.heading.as_deref()
            } else {
                None
            };
            (input.path.clone(), input.reader.heading.as_deref(), writer)
        })
        .collect();
    assert_eq!(described, vec![
        (fixture.path("a.txt"), Some("Pong"), Some("Ping")),
        (fixture.path("b.txt"), Some("Ping"), Some("Pong")),
    ]);
    assert!(written.is_empty(), "{written:?}");
    let worked: Vec<Option<&str>> = worked.iter().map(|task| task.name.as_deref()).collect();
    assert_eq!(worked, vec![Some("Ping"), Some("Pong")]);
    Ok(())
}

/// A command marked `always` that a pass reaches in the first pass runs once in that
/// build, when the pass first reaches it; later passes reuse its result without
/// running it, and the next build runs it again.
#[test]
fn an_always_command_reached_in_the_first_pass_runs_once_per_build() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        ("Use", "run(\"tool\", \"read:out.txt\")"),
        (
            "Make",
            "run(\"tool\", \"write:out.txt=made\")\nrun(\"tool\", \"bump:count.txt\", always = True)",
        ),
    ])?;
    let tracer = FakeTracer::default();
    let first = fixture.build(&tracer)?.report;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&first), vec![
        lines(&["Use ran ok", "Make ran ok"]),
        lines(&["Use ran ok", "Make cached ok"]),
        lines(&["Use cached ok", "Make cached ok"]),
    ]);
    assert_eq!(first.settled_in(), 2);
    assert_eq!(
        tracer.runs(),
        lines(&[
            "tool read:out.txt",
            "tool write:out.txt=made",
            "tool bump:count.txt",
            "tool read:out.txt",
        ])
    );
    let later = task_in(&first, 2, "Make")?;
    assert!(
        later
            .decisions
            .iter()
            .all(|decision| decision.verdict == Verdict::Cached)
    );
    assert_eq!(fs::read_to_string(fixture.path("count.txt"))?, "+");

    let second = fixture.build(&tracer)?.report;
    assert_eq!(passes(&second), vec![
        lines(&["Make ran ok", "Use cached ok"]),
        lines(&["Make cached ok", "Use cached ok"]),
    ]);
    assert_eq!(second.settled_in(), 1);
    let bumped = task_in(&second, 1, "Make")?;
    let verdicts: Vec<&Verdict> = bumped
        .decisions
        .iter()
        .map(|decision| &decision.verdict)
        .collect();
    assert!(
        matches!(verdicts.as_slice(), [Verdict::Cached, Verdict::Ran(explanation)]
            if explanation.reasons == [Reason::Always]),
        "{verdicts:?}"
    );
    assert_eq!(fs::read_to_string(fixture.path("count.txt"))?, "++");
    assert_eq!(tracer.runs().len(), 5);
    Ok(())
}

/// A step that names `memory`, whose available memory differs at every query, runs
/// at most once per build: a later pass of the build that ran it compares no
/// machine state again, so the build settles, and the next build compares it once,
/// finds it changed, and runs the step with a reason that names the value. A step
/// that names no part is queried only when it runs.
#[test]
fn a_step_that_names_changing_memory_runs_once_per_build_and_settles() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        (
            "Use",
            "run(\"tool\", \"read:out.txt\", machine = [\"memory\"])",
        ),
        ("Make", "run(\"tool\", \"write:out.txt=made\")"),
    ])?;
    let tracer = FakeTracer::default();
    let first = fixture.build(&tracer)?.report;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&first), vec![
        lines(&["Use ran ok", "Make ran ok"]),
        lines(&["Use ran ok", "Make cached ok"]),
        lines(&["Use cached ok", "Make cached ok"]),
    ]);
    // One query before each of the three runs, and none for a cache check.
    assert_eq!(tracer.machine_queries.load(Ordering::SeqCst), 3);

    let second = fixture.build(&tracer)?.report;
    assert_eq!(second.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&second), vec![
        lines(&["Make cached ok", "Use ran ok"]),
        lines(&["Make cached ok", "Use cached ok"]),
    ]);
    // One query to compare, and one before the run.
    assert_eq!(tracer.machine_queries.load(Ordering::SeqCst), 5);
    let ran = task_in(&second, 1, "Use")?;
    let [decision] = ran.decisions.as_slice() else {
        return Err(format!("unexpected decisions {:?}", ran.decisions).into());
    };
    assert_eq!(
        decision.verdict,
        Verdict::Ran(kizz_store::Explanation {
            reasons: vec![Reason::MachineChanged {
                changes: vec![MachineChange {
                    value: "available memory".to_owned(),
                    old: "3 bytes".to_owned(),
                    new: "4 bytes".to_owned(),
                }],
            }],
            repeated: Vec::new(),
        })
    );
    Ok(())
}

/// A command marked `always` that a pass first reaches in a later pass runs exactly
/// once in that build, in the pass that first reaches it; later passes reuse that
/// result without running it, and the build settles.
#[test]
fn an_always_command_first_reached_in_a_later_pass_runs_once_then() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        (
            "Late",
            "if fs.exists(\"ready.txt\"):\n    run(\"tool\", \"bump:count.txt\", always = True)",
        ),
        ("Ready", "run(\"tool\", \"write:ready.txt=1\")"),
    ])?;
    let tracer = FakeTracer::default();
    let first = fixture.build(&tracer)?.report;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&first), vec![
        lines(&["Late cached ok", "Ready ran ok"]),
        lines(&["Late ran ok", "Ready cached ok"]),
        lines(&["Late cached ok", "Ready cached ok"]),
    ]);
    assert_eq!(first.settled_in(), 2);
    assert_eq!(
        tracer.runs(),
        lines(&["tool write:ready.txt=1", "tool bump:count.txt"])
    );
    assert_eq!(task_in(&first, 1, "Late")?.decisions.len(), 0);
    let reached = task_in(&first, 2, "Late")?;
    assert!(
        matches!(
            reached.decisions.as_slice(),
            [decision] if matches!(&decision.verdict, Verdict::Ran(explanation)
                if explanation.reasons == [Reason::NeverRun])
        ),
        "{:?}",
        reached.decisions
    );
    let reused = task_in(&first, 3, "Late")?;
    assert!(
        matches!(reused.decisions.as_slice(), [decision] if decision.verdict == Verdict::Cached),
        "{:?}",
        reused.decisions
    );
    assert_eq!(fs::read_to_string(fixture.path("count.txt"))?, "+");
    Ok(())
}

/// A command marked `always` whose step key changes between passes, because its
/// arguments come from a file that a later task writes, runs once for each key, in
/// the pass that first reaches that key; later passes reuse that key's result.
#[test]
fn an_always_command_with_a_new_step_key_runs_once_for_each_key() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("arg.txt", "one")?;
    fixture.kizz("", &[
        (
            "Stamp",
            "run(\"tool\", \"bump:count.txt\", fs.read(\"arg.txt\"), always = True)",
        ),
        ("Rename", "run(\"tool\", \"write:arg.txt=two\")"),
    ])?;
    let tracer = FakeTracer::default();
    let first = fixture.build(&tracer)?.report;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&first), vec![
        lines(&["Stamp ran ok", "Rename ran ok"]),
        lines(&["Stamp ran ok", "Rename cached ok"]),
        lines(&["Stamp cached ok", "Rename cached ok"]),
    ]);
    assert_eq!(first.settled_in(), 2);
    assert_eq!(
        tracer.runs(),
        lines(&[
            "tool bump:count.txt one",
            "tool write:arg.txt=two",
            "tool bump:count.txt two",
        ])
    );
    let reused = task_in(&first, 3, "Stamp")?;
    assert!(
        matches!(reused.decisions.as_slice(), [decision] if decision.verdict == Verdict::Cached),
        "{:?}",
        reused.decisions
    );
    assert_eq!(fs::read_to_string(fixture.path("count.txt"))?, "++");
    Ok(())
}

/// Without a learned order, parent directories come before their subdirectories,
/// sibling directories come in sorted order, and task blocks come in file order;
/// a pass that runs nothing ends the build at once.
#[test]
fn the_default_order_puts_parents_first_and_blocks_in_file_order() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("b", &[("B", "x = 1")])?;
    fixture.kizz("a/c", &[("AC", "x = 1")])?;
    fixture.kizz("a", &[("A", "x = 1")])?;
    fixture.kizz("", &[("Root one", "x = 1"), ("Root two", "x = 1")])?;
    let tracer = FakeTracer::default();
    let report = fixture.build(&tracer)?.report;
    assert_eq!(
        order_headings(&report)?,
        lines(&["Root one", "Root two", "A", "AC", "B"])
    );
    assert_eq!(report.order, vec![
        fixture.id("", 0),
        fixture.id("", 1),
        fixture.id("a", 0),
        fixture.id("a/c", 0),
        fixture.id("b", 0),
    ]);
    assert_eq!(report.passes.len(), 1);
    assert_eq!(report.settled_in(), 1);
    assert_eq!(report.outcome, BuildOutcome::Succeeded);
    assert_eq!(tracer.runs(), Vec::<String>::new());
    for id in &report.order {
        assert_eq!(fixture.learned(id)?, None);
    }
    Ok(())
}

/// A task that listed a directory another task wrote into learns that task as
/// its producer, and the next build runs the producer first.
#[test]
fn a_directory_another_task_wrote_into_is_learned() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.path("out"))?;
    fixture.kizz("", &[
        ("List", "run(\"tool\", \"list:out\")"),
        ("Fill", "run(\"tool\", \"write:out/x.txt=1\")"),
    ])?;
    let tracer = FakeTracer::default();
    let first = fixture.build(&tracer)?.report;
    assert_eq!(passes(&first), vec![
        lines(&["List ran ok", "Fill ran ok"]),
        lines(&["List ran ok", "Fill cached ok"]),
        lines(&["List cached ok", "Fill cached ok"]),
    ]);
    assert_eq!(first.settled_in(), 2);
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );
    let second = fixture.build(&tracer)?.report;
    assert_eq!(order_headings(&second)?, lines(&["Fill", "List"]));
    assert_eq!(second.passes.len(), 1);
    assert_eq!(second.settled_in(), 1);
    Ok(())
}

/// Once cancellation is requested, the build starts no further command or task,
/// the interrupted command's result is not cached, and the build reports the
/// interruption without storing a learned order.
#[test]
fn an_interrupted_build_starts_nothing_new() -> TestResult {
    let fixture = Fixture::new()?;
    let after = ("After", "run(\"tool\", \"write:after.txt=1\")");
    let make = ("Make", "run(\"tool\", \"write:out.txt=made\")");
    fixture.kizz("", &[
        ("Use", "run(\"tool\", \"read:out.txt\")"),
        make,
        after,
    ])?;
    let tracer = FakeTracer::default();
    let learned = fixture.build(&tracer)?.report;
    assert_eq!(learned.outcome, BuildOutcome::Succeeded);
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );
    fs::remove_file(fixture.path("after.txt"))?;

    // The consumer now interrupts the build and consumes nothing the producer
    // writes, which a finished build would learn.
    fixture.kizz("", &[
        (
            "Use",
            "run(\"tool\", \"interrupt\")\nrun(\"tool\", \"write:never.txt=1\")",
        ),
        make,
        after,
    ])?;
    let before = tracer.runs().len();
    let events = Events::default();
    let report = fixture
        .build_with(&tracer, Cancellation::new(), &events)?
        .report;
    assert_eq!(report.outcome, BuildOutcome::Interrupted);
    assert_eq!(passes(&report), vec![lines(&[
        "Make cached ok",
        "Use ran failed"
    ])]);
    assert_eq!(report.settled_in(), 1);
    assert_eq!(
        tracer.runs().get(before..),
        Some(&lines(&["tool interrupt"])[..])
    );
    let stopped = task_in(&report, 1, "Use")?;
    assert!(
        stopped
            .result
            .as_ref()
            .is_err_and(|failure| failure.to_string().contains(INTERRUPTED)),
        "{:?}",
        stopped.result
    );
    assert!(!fixture.path("never.txt").exists());
    assert!(!fixture.path("after.txt").exists());
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );

    // The next build runs the interrupted command again, because its result was
    // not cached.
    let next = fixture.build(&tracer)?.report;
    assert_eq!(next.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        tracer.runs().get(before..),
        Some(&lines(&["tool interrupt", "tool interrupt"])[..])
    );
    Ok(())
}

/// No task program changed a file, as [`changed_in`] shows it.
const NO_CHANGES: [(String, Vec<String>); 0] = [];

/// The files that each task program changed in pass `pass` of `report`, counting
/// passes from 1, as `(heading, paths relative to the project)` for each task that
/// changed one, in the order the tasks ran.
fn changed_in(
    fixture: &Fixture,
    report: &BuildReport,
    pass: usize,
) -> TestResult<Vec<(String, Vec<String>)>> {
    let pass = report
        .passes
        .iter()
        .find(|candidate| candidate.number == pass)
        .ok_or("no such pass")?;
    Ok(pass
        .tasks
        .iter()
        .filter(|result| result.changed_files())
        .map(|result| {
            let paths = result
                .changed
                .iter()
                .map(|path| {
                    path.as_path()
                        .strip_prefix(fixture.project.path())
                        .unwrap_or(path.as_path())
                        .display()
                        .to_string()
                })
                .collect();
            (result.task.name.clone().unwrap_or_default(), paths)
        })
        .collect())
}

/// The only changed input of the first step of the task with heading `heading` in
/// pass `pass` of `report`, which must have run for it.
fn only_input<'r>(
    report: &'r BuildReport,
    pass: usize,
    heading: &str,
) -> TestResult<&'r kizz_store::ChangedInput> {
    let rerun = task_in(report, pass, heading)?;
    let Some(Verdict::Ran(explanation)) = rerun.decisions.first().map(|step| &step.verdict) else {
        return Err(format!("{heading} did not run in pass {pass}").into());
    };
    let [Reason::InputsChanged { inputs }] = explanation.reasons.as_slice() else {
        return Err(format!("unexpected reasons {:?}", explanation.reasons).into());
    };
    let [input] = inputs.as_slice() else {
        return Err(format!("unexpected inputs {inputs:?}").into());
    };
    Ok(input)
}

/// The review's case without commands: a task earlier in the default order copies
/// `in.txt` to `out.txt` with `fs.read` and `fs.write`, and a later task's program
/// writes new text to `in.txt`. The write is work, so another pass copies the new
/// text, and the build settles in 2 passes and runs 3; the next build settles in 1
/// and changes nothing. The learned order runs the writer first, and it still does
/// after a build in which the program does not write the file, because the program
/// is stored as the file's last writer.
#[test]
fn a_task_program_write_is_work_that_another_pass_follows() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("in.txt", "old\n")?;
    fixture.kizz("", &[
        ("Copy", "fs.write(\"out.txt\", fs.read(\"in.txt\"))"),
        (
            "Update",
            "if env.caller(\"SKIP\", \"\") != \"1\":\n    fs.write(\"in.txt\", \"new\\n\")",
        ),
    ])?;
    let tracer = FakeTracer::default();
    let first = fixture.build(&tracer)?.report;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(order_headings(&first)?, lines(&["Copy", "Update"]));
    assert_eq!(first.passes.len(), 3);
    assert_eq!(first.settled_in(), 2);
    assert_eq!(changed_in(&fixture, &first, 1)?, vec![
        ("Copy".to_owned(), lines(&["out.txt"])),
        ("Update".to_owned(), lines(&["in.txt"])),
    ]);
    assert_eq!(changed_in(&fixture, &first, 2)?, vec![(
        "Copy".to_owned(),
        lines(&["out.txt"])
    )]);
    assert_eq!(changed_in(&fixture, &first, 3)?, NO_CHANGES);
    assert!(first.passes.iter().all(|pass| !pass.ran()));
    assert_eq!(tracer.runs(), Vec::<String>::new());
    assert_eq!(fixture.read("out.txt")?, "new\n");
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );
    assert_eq!(fixture.learned(&fixture.id("", 1))?, None);

    let second = fixture.build(&tracer)?.report;
    assert_eq!(second.outcome, BuildOutcome::Succeeded);
    assert_eq!(order_headings(&second)?, lines(&["Update", "Copy"]));
    assert_eq!(second.passes.len(), 1);
    assert_eq!(second.settled_in(), 1);
    assert_eq!(changed_in(&fixture, &second, 1)?, NO_CHANGES);
    assert_eq!(fixture.read("out.txt")?, "new\n");

    // A build in which the program does not write the file keeps the order,
    // because the program is still the file's last writer.
    let third = fixture.build_vars(&tracer, &[("SKIP", "1")])?;
    assert_eq!(third.outcome, BuildOutcome::Succeeded);
    assert_eq!(order_headings(&third)?, lines(&["Update", "Copy"]));
    assert_eq!(third.settled_in(), 1);
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );
    let fourth = fixture.build_vars(&tracer, &[("SKIP", "1")])?;
    assert_eq!(order_headings(&fourth)?, lines(&["Update", "Copy"]));
    assert_eq!(fourth.settled_in(), 1);
    assert_eq!(tracer.runs(), Vec::<String>::new());
    Ok(())
}

/// A cached command whose input a later task's program changes runs again in the
/// next pass, and its reason names the input and the writing task's program, in
/// this build; a program write of an earlier build is named as such. The learned
/// order then runs the writer first.
#[test]
fn a_cached_command_runs_again_when_a_task_program_changes_its_input() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("in.txt", "old\n")?;
    let user = (
        "Use",
        "if env.caller(\"SKIP\", \"\") != \"1\":\n    run(\"tool\", \"read:in.txt\")",
    );
    fixture.kizz("", &[user])?;
    let tracer = FakeTracer::default();
    let first = fixture.build(&tracer)?.report;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(tracer.runs(), lines(&["tool read:in.txt"]));

    fixture.kizz("", &[
        user,
        (
            "Update",
            "fs.write(\"in.txt\", env.caller(\"TEXT\", \"new\\n\"))",
        ),
    ])?;
    let second = fixture.build(&tracer)?.report;
    assert_eq!(second.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&second), vec![
        lines(&["Use cached ok", "Update cached ok"]),
        lines(&["Use ran ok", "Update cached ok"]),
        lines(&["Use cached ok", "Update cached ok"]),
    ]);
    assert_eq!(second.settled_in(), 2);
    assert_eq!(changed_in(&fixture, &second, 1)?, vec![(
        "Update".to_owned(),
        lines(&["in.txt"])
    )]);
    assert_eq!(
        tracer.runs(),
        lines(&["tool read:in.txt", "tool read:in.txt"])
    );
    let input = only_input(&second, 2, "Use")?;
    assert_eq!(input.path, fixture.path("in.txt"));
    assert_eq!(input.self_inflicted, None);
    let Writer::ProgramThisBuild { task } = &input.writer else {
        return Err(format!("unexpected writer {:?}", input.writer).into());
    };
    assert!(is_task(task, "Update"), "{task:?}");
    assert_eq!(task.task, fixture.id("", 1));
    let render = Render {
        base: fixture.project.path().to_path_buf(),
        home: None,
        verbose: false,
    };
    let named = render.writer(&input.writer);
    assert!(
        named.starts_with("the program of kizz.md:") && named.ends_with(" (Update) in this build"),
        "{named}"
    );
    assert_eq!(
        fixture.learned(&fixture.id("", 0))?,
        Some(vec![fixture.id("", 1)])
    );

    // The learned order runs the writer first, and nothing runs.
    let third = fixture.build(&tracer)?.report;
    assert_eq!(order_headings(&third)?, lines(&["Update", "Use"]));
    assert_eq!(third.settled_in(), 1);
    assert_eq!(tracer.runs().len(), 2);

    // The program changes the file in a build that does not run the command; the
    // next build runs it, naming the program of that earlier build.
    let fourth = fixture.build_vars(&tracer, &[("SKIP", "1"), ("TEXT", "newer\n")])?;
    assert_eq!(fourth.outcome, BuildOutcome::Succeeded);
    assert_eq!(fourth.settled_in(), 1);
    assert_eq!(tracer.runs().len(), 2);
    let fifth = fixture.build_vars(&tracer, &[("TEXT", "newer\n")])?;
    assert_eq!(fifth.settled_in(), 1);
    assert_eq!(tracer.runs().len(), 3);
    let input = only_input(&fifth, 1, "Use")?;
    assert_eq!(input.path, fixture.path("in.txt"));
    assert_eq!(input.self_inflicted, None);
    assert!(
        matches!(&input.writer, Writer::ProgramEarlierBuild { task } if is_task(task, "Update")),
        "{:?}",
        input.writer
    );
    let named = render.writer(&input.writer);
    assert!(named.ends_with(" (Update) in an earlier build"), "{named}");
    Ok(())
}

/// A program write that leaves its file as it was is no work and adds no pass.
#[test]
fn a_program_write_that_changes_nothing_adds_no_pass() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("same.txt", "same\n")?;
    fixture.kizz("", &[
        ("Same", "fs.write(\"same.txt\", \"same\\n\")"),
        ("Json", "fs.write_json(\"same.json\", {\"a\": 1})"),
    ])?;
    fixture.write("same.json", "{\n  \"a\": 1\n}\n")?;
    let tracer = FakeTracer::default();
    let report = fixture.build(&tracer)?.report;
    assert_eq!(report.outcome, BuildOutcome::Succeeded);
    assert_eq!(report.passes.len(), 1);
    assert_eq!(report.settled_in(), 1);
    assert_eq!(changed_in(&fixture, &report, 1)?, NO_CHANGES);
    assert!(report.passes.iter().all(|pass| !pass.did_work()));
    Ok(())
}

/// A program that writes different text to a file it reads, on every pass, stops
/// the build at the pass limit, and the outcome names the file with that task's
/// program as its writer.
#[test]
fn a_program_that_keeps_changing_a_file_stops_at_the_pass_limit() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("count.txt", "")?;
    fixture.kizz("", &[(
        "Count",
        "fs.write(\"count.txt\", fs.read(\"count.txt\") + \"+\")",
    )])?;
    let tracer = FakeTracer::default();
    let report = fixture.build(&tracer)?.report;
    assert_eq!(report.passes.len(), 2);
    assert_eq!(report.settled_in(), 2);
    assert_eq!(fixture.read("count.txt")?, "++");
    let BuildOutcome::NotSettled {
        limit,
        changing,
        written,
        worked,
    } = &report.outcome
    else {
        return Err(format!("unexpected outcome {:?}", report.outcome).into());
    };
    assert_eq!(*limit, 2);
    assert!(changing.is_empty(), "{changing:?}");
    let [write] = written.as_slice() else {
        return Err(format!("unexpected writes {written:?}").into());
    };
    assert_eq!(write.path, fixture.path("count.txt"));
    assert!(is_task(&write.writer, "Count"), "{write:?}");
    assert_eq!(write.writer.task, fixture.id("", 0));
    let worked: Vec<Option<&str>> = worked.iter().map(|task| task.name.as_deref()).collect();
    assert_eq!(worked, vec![Some("Count")]);
    Ok(())
}

/// An unchecked step as the tests compare it: the heading of its task, its
/// command, how it ended, whether the build ran a command under its step key, and
/// whether its result came from the cache.
type Unchecked = (Option<String>, String, Termination, bool, bool);

/// The unchecked steps `steps`, as the tests compare them.
fn unchecked_steps(steps: &[UncheckedStep]) -> Vec<Unchecked> {
    steps
        .iter()
        .map(|step| {
            (
                step.task.heading.clone(),
                step.exit.command(),
                step.exit.termination,
                step.ran,
                step.exit.cached,
            )
        })
        .collect()
}

/// The number of unchecked steps of each task run of `report`, pass by pass.
fn unchecked_per_run(report: &BuildReport) -> Vec<Vec<usize>> {
    report
        .passes
        .iter()
        .map(|pass| {
            pass.tasks
                .iter()
                .map(|result| result.unchecked.len())
                .collect()
        })
        .collect()
}

/// A superseded result never warns (the final-state rule): `Use`, earlier in the
/// default order, fails unchecked in pass 1, because `Make` has not yet written
/// its input, and succeeds in pass 2; `Switch` fails unchecked in pass 1, and its
/// run in pass 2 no longer makes that command. Neither leaves a standing unchecked
/// exit, with or without `--fail-unchecked`; with the option their pass-1 runs
/// fail, and the build still succeeds, because its final pass decides.
#[test]
fn a_superseded_unchecked_exit_never_stands() -> TestResult {
    for fail in [false, true] {
        let fixture = Fixture::new()?;
        fixture.kizz("", &[
            ("Use", "run(\"tool\", \"need:out.txt\")"),
            (
                "Switch",
                "if fs.exists(\"flag.txt\"):\n    run(\"tool\", \"ok\")\nelse:\n    \
                 run(\"tool\", \"need:flag.txt\")",
            ),
            (
                "Make",
                "run(\"tool\", \"write:out.txt=made\")\nfs.write(\"flag.txt\", \"set\\n\")",
            ),
        ])?;
        let tracer = FakeTracer::default();
        let report = fixture.build_unchecked(&tracer, fail)?;
        let first = if fail { "failed" } else { "ok" };
        let use_line = format!("Use ran {first}");
        let switch_line = format!("Switch ran {first}");
        assert_eq!(passes(&report), [
            lines(&[use_line.as_str(), switch_line.as_str(), "Make ran ok"]),
            lines(&["Use ran ok", "Switch ran ok", "Make cached ok"]),
            lines(&["Use cached ok", "Switch cached ok", "Make cached ok"]),
        ]);
        assert_eq!(unchecked_per_run(&report), [
            vec![1, 1, 0],
            vec![0, 0, 0],
            vec![0, 0, 0]
        ]);
        assert_eq!(report.outcome, BuildOutcome::Succeeded, "fail: {fail}");
        let standing = report.unchecked();
        assert!(standing.is_empty(), "fail: {fail}: {standing:?}");
    }
    Ok(())
}

/// Exactly once, and again from the cache: `Ignore`, whose final result is
/// unchecked, in a first build that takes three passes, the final one included,
/// and reaches the step in each, gives exactly one standing unchecked exit, which
/// says that the build ran the command; the next build, which replays every step
/// from the cache, gives the same standing unchecked exit, which says that its
/// result came from the cache. `Check`, which reads `rc`, gives none.
#[test]
fn a_standing_unchecked_exit_stands_once_and_again_from_the_cache() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        (
            "Ignore",
            "run(\"tool\", \"need:absent.txt\")\nrun(\"tool\", \"read:in.txt\")",
        ),
        (
            "Check",
            "result = run(\"tool\", \"need:absent.txt\", \"checked\")\nif result.rc != 1:\n    \
             fail(\"unexpected\")",
        ),
        ("Make", "fs.write(\"in.txt\", \"in\\n\")"),
    ])?;
    let tracer = FakeTracer::default();

    let first = fixture.build_unchecked(&tracer, false)?;
    assert_eq!(first.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&first), [
        lines(&["Ignore ran ok", "Check ran ok", "Make cached ok"]),
        lines(&["Ignore ran ok", "Check cached ok", "Make cached ok"]),
        lines(&["Ignore cached ok", "Check cached ok", "Make cached ok"]),
    ]);
    assert_eq!(
        unchecked_per_run(&first),
        [vec![1, 0, 0], vec![1, 0, 0], vec![1, 0, 0]],
        "every pass reaches the unchecked step"
    );
    let standing = first.unchecked();
    assert_eq!(unchecked_steps(&standing), [(
        Some("Ignore".to_owned()),
        "tool need:absent.txt".to_owned(),
        Termination::Exited(1),
        true,
        true,
    )]);
    let [step] = standing.as_slice() else {
        return Err(format!("unexpected steps {standing:?}").into());
    };
    assert_eq!(step.task.task, fixture.id("", 0));
    assert_eq!(step.exit.position, 0);
    let ran = tracer.runs().len();

    // The build learned that `Ignore` reads the file that `Make` writes, so `Make`
    // now runs first.
    let second = fixture.build_unchecked(&tracer, false)?;
    assert_eq!(second.outcome, BuildOutcome::Succeeded);
    assert_eq!(passes(&second), [lines(&[
        "Make cached ok",
        "Ignore cached ok",
        "Check cached ok"
    ])]);
    assert_eq!(unchecked_steps(&second.unchecked()), [(
        Some("Ignore".to_owned()),
        "tool need:absent.txt".to_owned(),
        Termination::Exited(1),
        false,
        true,
    )]);
    assert_eq!(tracer.runs().len(), ran, "the second build ran nothing");
    Ok(())
}

/// A build that does not settle, and one that is interrupted, give no standing
/// unchecked exit, although their task runs left steps unchecked; the same report
/// with a settled outcome, built by hand, would give the final pass's.
#[test]
fn unsettled_and_interrupted_builds_have_no_standing_unchecked_exit() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("count.txt", "")?;
    fixture.kizz("", &[(
        "Count",
        "run(\"tool\", \"need:absent.txt\")\nfs.write(\"count.txt\", fs.read(\"count.txt\") + \
         \"+\")",
    )])?;
    let report = fixture.build_unchecked(&FakeTracer::default(), false)?;
    assert!(
        matches!(report.outcome, BuildOutcome::NotSettled { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(unchecked_per_run(&report), [vec![1], vec![1]]);
    assert!(report.unchecked().is_empty(), "{:?}", report.unchecked());
    let settled = BuildReport {
        outcome: BuildOutcome::Succeeded,
        ..report.clone()
    };
    assert_eq!(settled.unchecked().len(), 1);
    let failed = BuildReport {
        outcome: BuildOutcome::Failed {
            failures: Vec::new(),
        },
        ..report.clone()
    };
    assert_eq!(failed.unchecked().len(), 1);
    let interrupted = BuildReport {
        outcome: BuildOutcome::Interrupted,
        ..report
    };
    assert!(
        interrupted.unchecked().is_empty(),
        "{:?}",
        interrupted.unchecked()
    );

    // A build that the last command of its first task interrupts.
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        (
            "Stop",
            "run(\"tool\", \"need:absent.txt\")\nrun(\"tool\", \"interrupt\")",
        ),
        ("After", "run(\"tool\", \"need:absent.txt\", \"after\")"),
    ])?;
    let report = fixture.build_unchecked(&FakeTracer::default(), false)?;
    assert_eq!(report.outcome, BuildOutcome::Interrupted);
    assert_eq!(passes(&report), [lines(&["Stop ran ok"])]);
    assert_eq!(unchecked_per_run(&report), [vec![2]]);
    assert!(report.unchecked().is_empty(), "{:?}", report.unchecked());
    Ok(())
}

/// With `--fail-unchecked`, a task whose final run leaves a non-zero exit
/// unchecked fails when its program ends, with a failure that names the command,
/// and the build fails; a task that reads `rc` does not. In the first build the
/// command runs in pass 1 and its result comes from the cache in pass 2, which
/// ends the build; in the second build it comes from the cache and fails the same
/// way. Without the option, the same project builds, with the same standing
/// unchecked exit.
#[test]
fn fail_unchecked_fails_a_standing_unchecked_exit_and_the_build() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("", &[
        ("Ignore", "run(\"tool\", \"need:absent.txt\")"),
        (
            "Check",
            "result = run(\"tool\", \"need:absent.txt\", \"checked\")\nif result.rc != 1:\n    \
             fail(\"unexpected\")",
        ),
    ])?;
    let tracer = FakeTracer::default();
    let expected_passes = [
        vec![
            lines(&["Ignore ran failed", "Check ran ok"]),
            lines(&["Ignore cached failed", "Check cached ok"]),
        ],
        vec![lines(&["Ignore cached failed", "Check cached ok"])],
    ];
    for expected in expected_passes {
        let report = fixture.build_unchecked(&tracer, true)?;
        assert_eq!(passes(&report), expected);
        let BuildOutcome::Failed { failures } = &report.outcome else {
            return Err(format!("unexpected outcome {:?}", report.outcome).into());
        };
        let [failed] = failures.as_slice() else {
            return Err(format!("unexpected failures {failures:?}").into());
        };
        assert_eq!(failed.task.name.as_deref(), Some("Ignore"));
        let TaskFailure::Unchecked { exits } = &failed.failure else {
            return Err(format!("unexpected failure {:?}", failed.failure).into());
        };
        let commands: Vec<(String, Termination, bool)> = exits
            .iter()
            .map(|exit| (exit.command(), exit.termination, exit.cached))
            .collect();
        assert_eq!(commands, [(
            "tool need:absent.txt".to_owned(),
            Termination::Exited(1),
            true
        )]);
        let message = failed.failure.to_string();
        assert!(
            message.contains("command `tool need:absent.txt`: exit code 1"),
            "{message}"
        );
        let standing = report.unchecked();
        assert_eq!(standing.len(), 1, "{standing:?}");
        assert!(
            !fixture.task_record(&fixture.id("", 0))?.succeeded,
            "the task record says that the run failed"
        );
        assert!(fixture.task_record(&fixture.id("", 1))?.succeeded);
    }
    assert_eq!(tracer.runs().len(), 2, "only the first build ran commands");
    let report = fixture.build_unchecked(&tracer, false)?;
    assert_eq!(report.outcome, BuildOutcome::Succeeded);
    let standing = report.unchecked();
    assert_eq!(standing.len(), 1, "{standing:?}");
    Ok(())
}
