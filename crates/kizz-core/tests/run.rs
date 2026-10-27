// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tests of running one task by its name, as `kizz run` does, with a fake tracer:
//! selection of the nearest task of a name, one run without other tasks or further
//! passes, the arguments in `task.args`, the engine option behind `--no-cache`,
//! `always` steps and `cache=never` tasks, the step events, unchecked exits,
//! `--fail-unchecked`, and `--cache-unknown-syscalls`.
//!
//! Each test writes `kizz.md` files into a temporary project directory and opens
//! the store in a temporary cache directory. Every task inherits `PATH` from the
//! caller environment, which holds the project's `bin` directory with an
//! executable `tool`. The fake tracer reads the command's arguments:
//!
//! * `out:TEXT` writes `TEXT` and a line break to standard output, which it echoes when
//!   the request asks for echo;
//! * `write:PATH=TEXT` writes `TEXT` to a file of the task directory;
//! * `exit:N` makes the command exit with status `N`;
//! * `unknown` reports a system call that the tracer does not know;
//!
//! and any other argument does nothing. Every echoed chunk and every step event
//! goes into one log, in the order they happen.

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
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use kizz_core::{
    Engine, NameError, NoPathError, PrintSink, RunReport, StepDecision, StepError, StepEvents,
    StepOutcome, TaskFailure, TaskInfo, WorkerPool, load, run, select_by_name,
};
use kizz_store::{Explanation, Reason, RunReason, StepRecord, Store, Verdict};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CapturedOutput, EchoSink, ExtendedAttributes,
    FirstReads, MachineState, OutputStream, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The first line of every task's body, so its commands find `tool`.
const INHERIT_PATH: &str = "env.inherit(\"PATH\")";

/// One log of what happened, in order: echoed output and step events.
#[derive(Default)]
struct Log {
    /// The entries, in order.
    entries: Mutex<Vec<String>>,
}

impl Log {
    /// Keep `entry`.
    fn push(
        &self,
        entry: String,
    ) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }

    /// Every entry so far, which the log then forgets.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.entries.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Echoes each chunk into the log as `echo TEXT`.
struct LogEcho<'l>(&'l Log);

impl EchoSink for LogEcho<'_> {
    fn echo(
        &self,
        _stream: OutputStream,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.0.push(format!(
            "echo {}",
            String::from_utf8_lossy(bytes).trim_end()
        ));
        Ok(())
    }
}

/// Logs each step event as `started COMMAND: REASONS` or `cached COMMAND: OUTPUT`.
struct LogEvents<'l>(&'l Log);

impl StepEvents for LogEvents<'_> {
    fn step_started(
        &self,
        decision: &StepDecision,
    ) {
        let reasons = match &decision.verdict {
            Verdict::Ran(explanation) => format!("{:?}", explanation.reasons),
            Verdict::Cached => "cached".to_owned(),
        };
        self.0
            .push(format!("started {}: {reasons}", decision.command));
    }

    fn step_cached(
        &self,
        decision: &StepDecision,
        result: &Result<StepOutcome, StepError>,
    ) {
        let output = match result {
            Ok(StepOutcome {
                output: CapturedOutput::Split { stdout, .. },
                ..
            }) => String::from_utf8_lossy(stdout).trim_end().to_owned(),
            Ok(StepOutcome {
                output: CapturedOutput::Merged { output },
                ..
            }) => String::from_utf8_lossy(output).trim_end().to_owned(),
            Err(error) => error.to_string(),
        };
        self.0
            .push(format!("cached {}: {output}", decision.command));
    }
}

/// Keeps the lines that task programs print.
#[derive(Default)]
struct Printed {
    /// The lines, in order.
    lines: Mutex<Vec<String>>,
}

impl PrintSink for Printed {
    fn print_line(
        &self,
        line: &str,
    ) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line.to_owned());
    }
}

/// A tracer failure from `error`.
fn tracer_failure(error: &dyn Error) -> TraceError {
    TraceError::Tracer {
        message: error.to_string(),
        source: None,
    }
}

/// A tracer that runs nothing, follows the module documentation's arguments, and
/// keeps every command it was asked to run.
#[derive(Default)]
struct FakeTracer {
    /// The argument vector of every run so far, joined with spaces.
    runs: Mutex<Vec<String>>,
}

impl FakeTracer {
    /// Every command run so far, which the tracer then forgets.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.runs.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Tracer for FakeTracer {
    fn run(
        &self,
        request: &RunRequest,
        echo: &dyn EchoSink,
        _cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        let args: Vec<String> = request
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(args.join(" "));
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Exec);
        let mut stdout = Vec::new();
        let mut termination = Termination::Exited(0);
        let mut unknown = BTreeSet::new();
        for arg in args.iter().skip(1) {
            let (op, rest) = arg.split_once(':').unwrap_or((arg, ""));
            match op {
                "out" => {
                    let line = format!("{rest}\n");
                    if request.echo {
                        echo.echo(OutputStream::Stdout, line.as_bytes())
                            .map_err(|error| tracer_failure(&error))?;
                    }
                    stdout.extend_from_slice(line.as_bytes());
                },
                "write" => {
                    let (path, text) = rest.split_once('=').unwrap_or((rest, ""));
                    let path = request.cwd.as_path().join(path);
                    fs::write(&path, text).map_err(|error| tracer_failure(&error))?;
                    let path = AbsolutePath::new(path).map_err(|error| tracer_failure(&error))?;
                    accesses.record(path, AccessKind::Write);
                },
                "exit" => {
                    termination = Termination::Exited(rest.parse().unwrap_or(1));
                },
                "unknown" => {
                    unknown.insert(999);
                },
                _ => {},
            }
        }
        Ok(RunResult {
            termination,
            output: CapturedOutput::Split {
                stdout,
                stderr: Vec::new(),
            },
            tree: None,
            accesses,
            incomplete: None,
            first_reads: FirstReads::new(),
            unknown_system_calls: unknown,
            connections: BTreeSet::new(),
        })
    }

    /// The tests read no extended attributes, so there are none.
    fn extended_attributes(
        &self,
        _path: &Path,
        _follow: bool,
    ) -> io::Result<ExtendedAttributes> {
        Ok(ExtendedAttributes::new())
    }

    /// The tests name no part of the machine state.
    fn machine_state(
        &self,
        _cwd: &Path,
    ) -> MachineState {
        MachineState::default()
    }
}

/// What a run does beyond the defaults, as the options of `kizz run` ask.
#[derive(Debug, Clone, Copy, Default)]
struct Options {
    /// `--no-cache`.
    no_cache: bool,
    /// `--fail-unchecked`.
    fail_unchecked: bool,
    /// `--cache-unknown-syscalls`.
    cache_unknown: bool,
}

/// A project and a cache, each in a temporary directory, with an executable `tool`
/// in the project's `bin` directory.
struct Fixture {
    /// The project directory.
    project: TempDir,
    /// The cache directory.
    cache: TempDir,
    /// The tracer of every run.
    tracer: FakeTracer,
    /// The log of every run.
    log: Log,
}

impl Fixture {
    /// A new fixture.
    fn new() -> TestResult<Self> {
        let fixture = Self {
            project: tempfile::tempdir()?,
            cache: tempfile::tempdir()?,
            tracer: FakeTracer::default(),
            log: Log::default(),
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

    /// Write a `kizz.md` at `relative` with one task for each info string and body
    /// of `tasks`, each under a heading of its own; each body follows
    /// `env.inherit("PATH")`.
    fn kizz(
        &self,
        relative: &str,
        tasks: &[(&str, &str)],
    ) -> TestResult {
        let mut markdown = String::new();
        for (index, (info, body)) in tasks.iter().enumerate() {
            write!(
                markdown,
                "# Task {index}\n\n```starlark {info}\n{INHERIT_PATH}\n{body}\n```\n\n"
            )?;
        }
        self.write(relative, &markdown)
    }

    /// Run the task named `name` under the directory `dir` of the project once,
    /// with the arguments `args` and `options`; the lines its program prints go to
    /// `printed`.
    fn run_in(
        &self,
        dir: &str,
        name: &str,
        args: &[&str],
        options: Options,
        printed: &Printed,
    ) -> TestResult<RunReport> {
        let pool = WorkerPool::new()?;
        let project = load(&self.path(dir), &pool)?;
        let task = select_by_name(&project.tasks, name)?;
        let task = TaskInfo::for_run(task, args.iter().map(|&arg| arg.to_owned()).collect())?;
        let caller: BTreeMap<String, String> = [(
            "PATH".to_owned(),
            self.path("bin").to_string_lossy().into_owned(),
        )]
        .into();
        let store = Store::open(self.cache.path())?;
        let echo = LogEcho(&self.log);
        let engine = Engine::new(
            &self.tracer,
            &store,
            &echo,
            Cancellation::new(),
            None,
            &pool,
        )
        .with_command_name("kizz run")
        .with_echo_every_step(true)
        .with_no_cache(options.no_cache)
        .with_fail_unchecked(options.fail_unchecked)
        .with_cache_unknown_system_calls(options.cache_unknown);
        let report = run(&engine, &task, &caller, printed, &LogEvents(&self.log))?;
        drop(engine);
        store.close()?;
        Ok(report)
    }

    /// Run the task named `name` in the project once, with `options`.
    fn run(
        &self,
        name: &str,
        options: Options,
    ) -> TestResult<RunReport> {
        self.run_in("", name, &[], options, &Printed::default())
    }

    /// The commands of every step record in the cache, sorted, each with its
    /// reason.
    fn records(&self) -> TestResult<Vec<(String, RunReason)>> {
        let store = Store::open(self.cache.path())?;
        let records = store.read()?.scan::<StepRecord>(&[])?;
        store.close()?;
        let mut records: Vec<(String, RunReason)> = records
            .into_iter()
            .map(|(_, record)| (record.spec.argv.join(" "), record.reason))
            .collect();
        records.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(records)
    }
}

/// The verdict of a step that ran for `reason`.
fn ran(reason: Reason) -> Verdict {
    Verdict::Ran(Explanation {
        reasons: vec![reason],
        repeated: Vec::new(),
    })
}

/// The verdicts of the steps of `report`, in order.
fn verdicts(report: &RunReport) -> Vec<Verdict> {
    report
        .result
        .decisions
        .iter()
        .map(|decision| decision.verdict.clone())
        .collect()
}

/// The task the fewest levels below the directory wins, even when it comes later
/// in the default order; two at the same level fail with both paths; an unknown
/// name fails with the names under the directory.
#[test]
fn the_nearest_task_wins_and_ties_and_unknown_names_fail() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("a/b/kizz.md", &[(
        "name=pick",
        "run(\"tool\", \"out:deep\")",
    )])?;
    fixture.kizz("c/kizz.md", &[("name=pick", "run(\"tool\", \"out:near\")")])?;
    fixture.kizz("d/kizz.md", &[("name=same ci", "x = 1")])?;
    fixture.kizz("e/kizz.md", &[("name=same", "x = 1")])?;

    let report = fixture.run("pick", Options::default())?;
    assert!(report.succeeded());
    assert_eq!(fixture.tracer.take(), ["tool out:near"]);
    assert_eq!(report.result.task.relative_file, PathBuf::from("c/kizz.md"));

    let project = load(fixture.project.path(), &WorkerPool::new()?)?;
    assert_eq!(
        select_by_name(&project.tasks, "same"),
        Err(NameError::Tie {
            name: "same".to_owned(),
            level: 1,
            tasks: vec![
                "d/kizz.md:3 (Task 0)".to_owned(),
                "e/kizz.md:3 (Task 0)".to_owned()
            ],
        })
    );
    assert_eq!(
        select_by_name(&project.tasks, "missing"),
        Err(NameError::Unknown {
            name: "missing".to_owned(),
            names: vec!["pick".to_owned(), "same".to_owned()],
        })
    );
    let below = load(&fixture.path("a"), &WorkerPool::new()?)?;
    assert_eq!(
        select_by_name(&below.tasks, "pick").map(|task| task.relative_file.clone()),
        Ok(PathBuf::from("b/kizz.md"))
    );
    Ok(())
}

/// The run runs only its task, once: no other task runs, and no pass follows even
/// when its program changes a file; the arguments reach `task.args`,
/// `task.selected` is empty, and two runs with different arguments whose program
/// ignores them have the same step keys, so the second is a cache hit.
#[test]
fn one_run_of_one_task_with_its_arguments() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("kizz.md", &[
        ("ci", "run(\"tool\", \"out:other\")"),
        (
            "name=greet",
            "print(task.args, task.selected)\nfs.write(\"gen.txt\", \"made\")\nrun(\"tool\", \
             \"out:fixed\")",
        ),
        ("name=echo", "run(\"tool\", *task.args)"),
    ])?;

    let printed = Printed::default();
    let first = fixture.run_in("", "greet", &["a", "b"], Options::default(), &printed)?;
    assert!(first.succeeded());
    assert_eq!(fixture.tracer.take(), ["tool out:fixed"]);
    assert_eq!(first.result.changed.len(), 1, "{first:?}");
    assert_eq!(first.result.task.args, ["a", "b"]);
    assert_eq!(first.result.task.selected, Vec::<String>::new());
    assert_eq!(
        printed
            .lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
        ["greet: [\"a\", \"b\"] []"]
    );
    assert_eq!(fixture.read_gen()?, "made");

    let second = fixture.run_in("", "greet", &["c"], Options::default(), &Printed::default())?;
    assert!(second.succeeded());
    assert_eq!(fixture.tracer.take(), Vec::<String>::new());
    assert_eq!(verdicts(&second), [Verdict::Cached]);
    assert_eq!(fixture.records()?, [(
        "tool out:fixed".to_owned(),
        RunReason::NeverRun
    )]);

    let echoed = fixture.run_in(
        "",
        "echo",
        &["out:x", "out:y"],
        Options::default(),
        &Printed::default(),
    )?;
    assert!(echoed.succeeded());
    assert_eq!(fixture.tracer.take(), ["tool out:x out:y"]);
    Ok(())
}

impl Fixture {
    /// The content of `gen.txt` in the project.
    fn read_gen(&self) -> TestResult<String> {
        Ok(fs::read_to_string(self.path("gen.txt"))?)
    }
}

/// `--no-cache` runs every step, with its reason, and records each result, so a
/// run without it is then a cache hit; a step that never ran keeps that reason.
#[test]
fn no_cache_runs_every_step_and_records_it() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("kizz.md", &[(
        "name=two",
        "run(\"tool\", \"out:one\")\nrun(\"tool\", \"out:two\")",
    )])?;
    let no_cache = Options {
        no_cache: true,
        ..Options::default()
    };

    let first = fixture.run("two", no_cache)?;
    assert_eq!(verdicts(&first), [
        ran(Reason::NeverRun),
        ran(Reason::NeverRun)
    ]);
    assert_eq!(fixture.tracer.take(), ["tool out:one", "tool out:two"]);

    let cached = fixture.run("two", Options::default())?;
    assert_eq!(verdicts(&cached), [Verdict::Cached, Verdict::Cached]);
    assert_eq!(fixture.tracer.take(), Vec::<String>::new());

    let forced = fixture.run("two", no_cache)?;
    assert_eq!(verdicts(&forced), [
        ran(Reason::RunNoCache),
        ran(Reason::RunNoCache)
    ]);
    assert_eq!(forced.command_counts(), (2, 0));
    assert_eq!(fixture.tracer.take(), ["tool out:one", "tool out:two"]);
    assert_eq!(fixture.records()?, [
        ("tool out:one".to_owned(), RunReason::RunNoCache),
        ("tool out:two".to_owned(), RunReason::RunNoCache),
    ]);

    let again = fixture.run("two", Options::default())?;
    assert_eq!(verdicts(&again), [Verdict::Cached, Verdict::Cached]);
    assert_eq!(again.command_counts(), (0, 2));
    assert_eq!(fixture.tracer.take(), Vec::<String>::new());
    Ok(())
}

/// Without `--no-cache`, a step marked `always` and every step of a task marked
/// `cache=never` still run on every run, with their own reasons, also with the
/// option; nothing makes them cached.
#[test]
fn always_and_cache_never_steps_run_without_the_option() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("kizz.md", &[
        (
            "name=mixed",
            "run(\"tool\", \"out:plain\")\nrun(\"tool\", \"out:always\", always = True)",
        ),
        ("name=never cache=never", "run(\"tool\", \"out:never\")"),
    ])?;
    for options in [Options::default(), Options::default(), Options {
        no_cache: true,
        ..Options::default()
    }] {
        fixture.run("mixed", options)?;
        fixture.run("never", options)?;
    }
    fixture.tracer.take();

    let mixed = fixture.run("mixed", Options::default())?;
    assert_eq!(verdicts(&mixed), [Verdict::Cached, ran(Reason::Always)]);
    let never = fixture.run("never", Options::default())?;
    assert_eq!(verdicts(&never), [ran(Reason::TaskCacheNever)]);
    let forced = fixture.run("mixed", Options {
        no_cache: true,
        ..Options::default()
    })?;
    assert_eq!(verdicts(&forced), [
        ran(Reason::RunNoCache),
        ran(Reason::Always)
    ]);
    assert_eq!(fixture.tracer.take(), [
        "tool out:always",
        "tool out:never",
        "tool out:plain",
        "tool out:always"
    ]);
    Ok(())
}

/// The events say that a step starts before any of its output, which is echoed
/// live although the `run()` has no `stream = True`, and that a step came from the
/// cache, with its stored output.
#[test]
fn events_tell_when_a_step_starts_and_when_it_comes_from_the_cache() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("kizz.md", &[(
        "name=say",
        "run(\"tool\", \"out:hello\")\nrun(\"tool\", \"out:world\")",
    )])?;

    fixture.run("say", Options::default())?;
    assert_eq!(fixture.log.take(), [
        "started tool out:hello: [NeverRun]",
        "echo hello",
        "started tool out:world: [NeverRun]",
        "echo world",
    ]);

    fixture.run("say", Options::default())?;
    assert_eq!(fixture.log.take(), [
        "cached tool out:hello: hello",
        "cached tool out:world: world",
    ]);
    Ok(())
}

/// An unchecked non-zero exit is in the report once the run ends, as the warning
/// says; `--fail-unchecked` fails the task with it, under the command name that
/// the engine was given, `kizz run`; and an interrupted run would warn about none.
#[test]
fn unchecked_exits_warn_and_fail_unchecked_fails_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("kizz.md", &[("name=ignore", "run(\"tool\", \"exit:3\")")])?;

    let warned = fixture.run("ignore", Options::default())?;
    assert!(warned.succeeded());
    let unchecked = warned.unchecked();
    assert_eq!(unchecked.len(), 1, "{warned:?}");
    assert!(unchecked.iter().all(|step| step.ran), "{unchecked:?}");

    let cached = fixture.run("ignore", Options::default())?;
    let unchecked = cached.unchecked();
    assert_eq!(unchecked.len(), 1, "{cached:?}");
    assert!(unchecked.iter().all(|step| !step.ran), "{unchecked:?}");

    let failed = fixture.run("ignore", Options {
        fail_unchecked: true,
        ..Options::default()
    })?;
    assert!(!failed.succeeded());
    let Err(failure @ TaskFailure::Unchecked { exits, invoked }) = &failed.result.result else {
        return Err(format!("unexpected result {failed:?}").into());
    };
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(*invoked, "kizz run");
    let message = failure.to_string();
    assert!(
        message.starts_with(
            "the task program left a non-zero exit unchecked, and `kizz run --fail-unchecked` \
             makes that fail the task\ncommand `tool exit:3`: exit code 3"
        ),
        "{message}"
    );

    let interrupted = RunReport {
        interrupted: true,
        ..warned
    };
    assert_eq!(interrupted.unchecked(), Vec::new());
    assert!(!interrupted.succeeded());
    Ok(())
}

/// A run that made a system call that the tracer does not know is not cached, so
/// it runs again, unless `--cache-unknown-syscalls` is given, which caches it.
#[test]
fn cache_unknown_syscalls_caches_a_run_with_an_unknown_call() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("kizz.md", &[("name=odd", "run(\"tool\", \"unknown\")")])?;

    fixture.run("odd", Options::default())?;
    let again = fixture.run("odd", Options::default())?;
    assert_eq!(again.command_counts(), (1, 0));
    assert_eq!(fixture.tracer.take().len(), 2);

    let caching = Options {
        cache_unknown: true,
        ..Options::default()
    };
    let recorded = fixture.run("odd", caching)?;
    assert_eq!(recorded.command_counts(), (1, 0));
    let hit = fixture.run("odd", caching)?;
    assert_eq!(verdicts(&hit), [Verdict::Cached]);
    assert_eq!(fixture.tracer.take(), ["tool unknown"]);
    Ok(())
}

/// A run of a task whose `run("tool")` has no `PATH`, with or without
/// `--no-cache`, tells its step events nothing, counts no command that ran or came
/// from the cache, asks the tracer for nothing, and fails with exactly the message
/// of [`NoPathError`], after the runtime's prefix, once.
#[test]
fn a_run_without_path_counts_no_command() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.kizz("kizz.md", &[(
        "name=bare",
        "env.unset(\"PATH\")\nrun(\"tool\")",
    )])?;
    let expected = format!("run(): {}", NoPathError {
        program: "tool".to_owned(),
    });
    for no_cache in [false, true] {
        let report = fixture.run("bare", Options {
            no_cache,
            ..Options::default()
        })?;
        assert!(!report.succeeded());
        assert_eq!(report.command_counts(), (0, 0), "{no_cache}");
        assert_eq!(report.result.decisions, Vec::new(), "{no_cache}");
        assert_eq!(fixture.log.take(), Vec::<String>::new(), "{no_cache}");
        let Err(failure) = &report.result.result else {
            return Err(format!("unexpected result {report:?}").into());
        };
        let message = failure.to_string();
        assert_eq!(message.matches(&expected).count(), 1, "{message}");
        assert!(!message.contains("run(): run():"), "{message}");
    }
    assert_eq!(fixture.tracer.take(), Vec::<String>::new());
    assert_eq!(fixture.records()?, Vec::new());
    Ok(())
}
