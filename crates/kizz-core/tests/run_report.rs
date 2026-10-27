// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tests of the observation report of `kizz run --output`, with a fake tracer:
//! one command for each step that the run reached, in order, a step that ran with
//! its process tree and a cache hit as cached, `--no-cache` giving every step its
//! tree, the task program's own accesses with their outcomes, the placeholder
//! process of a step whose program is not found, the tree asked for only when a
//! report is wanted, and decision 1's regressions: a command whose fake run
//! reports an unknown system call or a reason makes the report incomplete.
//!
//! Each test writes a `kizz.md` into a temporary project directory and opens the
//! store in a temporary cache directory. Every task inherits `PATH` from the caller
//! environment, which holds the project's `bin` directory with an executable
//! `tool`. The fake tracer, [`ReportTracer`], keeps whether each request asked for
//! the process tree, builds the tree of each result with
//! [`ProcessTree::of_run`] when it did (decision 1), names `ENOENT`, and reads the
//! command's arguments:
//!
//! * `out:TEXT` writes `TEXT` and a line break to standard output;
//! * `read:PATH` reads a file of the task directory;
//! * `unknown:N` reports the system call `N`, which the tracer does not know;
//! * `reason:TEXT` reports that the trace is incomplete, with `TEXT` as the reason;
//!
//! and any other argument does nothing.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use kizz_core::{
    CommandOutcome, Engine, NoStepEvents, PrintSink, Report, ReportMode, RunReport, TaskInfo,
    WorkerPool, load, run, select_by_name,
};
use kizz_store::Store;
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CapturedOutput, EchoSink, ExtendedAttributes,
    FirstReads, IncompleteReason, MachineState, Observations, Outcome, OutputStream, ProcessTree,
    RunRequest, RunResult, Termination, TraceError, Tracer, unnamed_error,
};
use serde_json::Value;
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The name of the task program in every report, as output names the task.
const TASK: &str = "kizz.md:3 (Task)";

/// Drops every line that task programs print.
struct NoPrint;

impl PrintSink for NoPrint {
    fn print_line(
        &self,
        _line: &str,
    ) {
    }
}

/// Drops every chunk of live output.
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

/// The fake tracer of the report tests, as the module's documentation says: it
/// keeps whether each request asked for the process tree, and builds the tree of
/// each of its results with [`ProcessTree::of_run`], Step 5's helper, so the tree
/// carries the run's reason, unknown system calls and connections (decision 1).
#[derive(Default)]
struct ReportTracer {
    /// Whether each request so far asked for the process tree, in order.
    trees: Mutex<Vec<bool>>,
}

impl ReportTracer {
    /// Whether each request so far asked for the tree, which the tracer then
    /// forgets.
    fn take(&self) -> Vec<bool> {
        std::mem::take(&mut *self.trees.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Tracer for ReportTracer {
    /// Keep whether `request` asks for the tree, and run it as the module's
    /// documentation says.
    fn run(
        &self,
        request: &RunRequest,
        _echo: &dyn EchoSink,
        _cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        self.trees
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.tree);
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Exec);
        let mut stdout = Vec::new();
        let mut unknown = BTreeSet::new();
        let mut incomplete = None;
        for arg in request.args.iter().skip(1) {
            let arg = arg.to_string_lossy().into_owned();
            let (op, rest) = arg.split_once(':').unwrap_or((arg.as_str(), ""));
            match op {
                "out" => stdout.extend_from_slice(format!("{rest}\n").as_bytes()),
                "read" => accesses.record(request.cwd.join(rest), AccessKind::Read),
                "unknown" => {
                    unknown.insert(rest.parse().unwrap_or(400));
                },
                "reason" => {
                    incomplete = Some(IncompleteReason::Other {
                        detail: rest.to_owned(),
                    });
                },
                _ => {},
            }
        }
        let mut result = RunResult {
            termination: Termination::Exited(0),
            output: CapturedOutput::Split {
                stdout,
                stderr: Vec::new(),
            },
            tree: None,
            accesses,
            incomplete,
            first_reads: FirstReads::new(),
            unknown_system_calls: unknown,
            connections: BTreeSet::new(),
        };
        if request.tree {
            result.tree = Some(ProcessTree::of_run(request, &result));
        }
        Ok(result)
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

    /// `ENOENT` by its name, as the Linux tracer names it, and every other number
    /// by the default answer.
    fn error_name(
        &self,
        errno: i32,
    ) -> String {
        if errno == 2 {
            "ENOENT".to_owned()
        } else {
            unnamed_error(errno)
        }
    }
}

/// How a run of the tests goes, as the options of `kizz run` ask.
#[derive(Debug, Clone, Copy, Default)]
struct Options {
    /// `--output`: the engine keeps what the report needs.
    report: bool,
    /// `--no-cache`.
    no_cache: bool,
}

/// `--output` alone.
const REPORT: Options = Options {
    report: true,
    no_cache: false,
};

/// A project and a cache, each in a temporary directory, with an executable `tool`
/// in the project's `bin` directory.
struct Fixture {
    /// The project directory.
    project: TempDir,
    /// The cache directory.
    cache: TempDir,
    /// The tracer of every run.
    tracer: ReportTracer,
}

impl Fixture {
    /// A new fixture.
    fn new() -> TestResult<Self> {
        let fixture = Self {
            project: tempfile::tempdir()?,
            cache: tempfile::tempdir()?,
            tracer: ReportTracer::default(),
        };
        fixture.write("bin/tool", "#!/bin/sh\n")?;
        fs::set_permissions(fixture.path("bin/tool"), fs::Permissions::from_mode(0o755))?;
        Ok(fixture)
    }

    /// The path of `relative` in the project.
    fn path(
        &self,
        relative: &str,
    ) -> PathBuf {
        self.project.path().join(relative)
    }

    /// The absolute path of `relative` in the project.
    fn absolute(
        &self,
        relative: &str,
    ) -> TestResult<AbsolutePath> {
        Ok(AbsolutePath::new(self.path(relative))?)
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

    /// Write the project's `kizz.md` with one task under the heading `Task`, whose
    /// info string is `starlark name=NAME` and whose block, on line 3, is `body`
    /// after `env.inherit("PATH")`, or `body` alone when `inherit` is not set.
    fn task(
        &self,
        name: &str,
        body: &str,
        inherit: bool,
    ) -> TestResult {
        let inherited = if inherit {
            "env.inherit(\"PATH\")\n"
        } else {
            ""
        };
        self.write(
            "kizz.md",
            &format!("# Task\n\n```starlark name={name}\n{inherited}{body}\n```\n"),
        )
    }

    /// Run the task named `name` in the project once with `options`.
    fn run(
        &self,
        name: &str,
        options: Options,
    ) -> TestResult<RunReport> {
        let pool = WorkerPool::new()?;
        let project = load(self.project.path(), &pool)?;
        let task = TaskInfo::for_run(select_by_name(&project.tasks, name)?, Vec::new())?;
        let caller: BTreeMap<String, String> = [(
            "PATH".to_owned(),
            self.path("bin").to_string_lossy().into_owned(),
        )]
        .into();
        let store = Store::open(self.cache.path())?;
        let engine = Engine::new(
            &self.tracer,
            &store,
            &NoEcho,
            Cancellation::new(),
            None,
            &pool,
        )
        .with_command_name("kizz run")
        .with_echo_every_step(true)
        .with_no_cache(options.no_cache)
        .with_report(options.report);
        let report = run(&engine, &task, &caller, &NoPrint, &NoStepEvents)?;
        drop(engine);
        store.close()?;
        Ok(report)
    }
}

/// The observation report of `run`, whose task program is [`TASK`].
fn report_of(run: &RunReport) -> TestResult<Report> {
    Ok(run
        .observation_report(TASK)
        .ok_or("the run kept no report")?)
}

/// The JSON of `report` in `mode`, parsed.
fn json(
    report: &Report,
    mode: ReportMode,
) -> TestResult<Value> {
    Ok(serde_json::from_str(&report.json(mode))?)
}

/// The value at `pointer` in `value`.
fn at<'v>(
    value: &'v Value,
    pointer: &str,
) -> TestResult<&'v Value> {
    Ok(value
        .pointer(pointer)
        .ok_or_else(|| format!("no {pointer} in {value}"))?)
}

/// The observations of `pairs`, each a path of the project, an access kind and an
/// outcome.
fn observed(
    fixture: &Fixture,
    pairs: &[(&str, AccessKind, Outcome)],
) -> TestResult<Observations> {
    let mut observations = Observations::new();
    for (path, kind, outcome) in pairs {
        observations.record(fixture.absolute(path)?, *kind, outcome.clone());
    }
    Ok(observations)
}

/// The report lists each step in order: a step that ran with its tree, whose one
/// process is the command, numbered 1, with the program's exec and the lookup's
/// probe of `bin/tool`, and a cache hit as cached, with `complete` set to `null`,
/// its recorded program and outcome, and no process, in the model, in JSON and in
/// text; the cached command does not count against the report's completeness.
#[test]
fn the_report_lists_each_step_a_run_with_its_tree_and_a_hit_as_cached() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.task(
        "two",
        "run(\"tool\", \"out:a\")\nrun(\"tool\", \"out:b\", always = True)",
        true,
    )?;
    fixture.run("two", Options::default())?;
    fixture.tracer.take();

    let second = fixture.run("two", REPORT)?;
    assert_eq!(fixture.tracer.take(), [true]);
    let report = report_of(&second)?;
    let tool = fixture.absolute("bin/tool")?;
    let [cached, ran] = report.commands.as_slice() else {
        return Err(format!("expected two commands, got {report:?}").into());
    };
    assert!(cached.cached);
    assert_eq!(cached.complete(), None);
    assert_eq!(cached.outcome, CommandOutcome::Exited(0));
    assert_eq!(cached.program.as_ref(), Some(&tool));
    assert!(cached.processes.is_empty(), "{cached:?}");
    assert_eq!(cached.argv, ["tool", "out:a"]);

    assert!(!ran.cached);
    assert_eq!(ran.complete(), Some(true));
    assert_eq!(ran.argv, ["tool", "out:b"]);
    let [process] = ran.processes.as_slice() else {
        return Err(format!("expected one process, got {ran:?}").into());
    };
    assert_eq!(process.id, 1);
    assert_eq!(process.exit, Some(Termination::Exited(0)));
    let [program] = process.programs.as_slice() else {
        return Err(format!("expected one program, got {process:?}").into());
    };
    assert_eq!(program.path, tool.as_path());
    let tool_kinds = program
        .observations
        .get(&tool)
        .ok_or("no observation of the tool")?;
    assert_eq!(tool_kinds.keys().copied().collect::<Vec<_>>(), [
        AccessKind::Probe,
        AccessKind::Exec
    ]);
    assert!(report.complete());

    let value = json(&report, ReportMode::Hierarchical)?;
    assert_eq!(at(&value, "/complete")?, &Value::Bool(true));
    assert_eq!(at(&value, "/commands/0/cached")?, &Value::Bool(true));
    assert_eq!(at(&value, "/commands/0/complete")?, &Value::Null);
    assert_eq!(
        at(&value, "/commands/0/processes")?,
        &Value::Array(Vec::new())
    );
    assert_eq!(at(&value, "/commands/1/cached")?, &Value::Bool(false));
    assert_eq!(at(&value, "/commands/1/complete")?, &Value::Bool(true));
    assert_eq!(at(&value, "/commands/1/processes/0/id")?, &Value::from(1));
    assert_eq!(
        at(&value, "/commands/1/processes/0/programs/0/path")?,
        &Value::from(tool.to_string())
    );
    assert_eq!(at(&value, "/task_program/task")?, &Value::from(TASK));

    let text = report.text(ReportMode::Hierarchical);
    assert!(text.contains("command 1: tool out:a\n"), "{text}");
    assert!(text.contains("  trace: cached, not traced\n"), "{text}");
    assert!(text.contains("command 2: tool out:b\n"), "{text}");
    assert!(text.contains("  trace: complete\n"), "{text}");
    Ok(())
}

/// With `--no-cache`, every step runs, so every command of the report has its
/// tree, and the processes are numbered across the commands.
#[test]
fn with_no_cache_every_step_has_its_tree() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.task(
        "two",
        "run(\"tool\", \"out:a\")\nrun(\"tool\", \"out:b\")",
        true,
    )?;
    fixture.run("two", Options::default())?;
    let cached = fixture.run("two", REPORT)?;
    let cached = report_of(&cached)?;
    assert!(
        cached.commands.iter().all(|command| command.cached),
        "{cached:?}"
    );
    fixture.tracer.take();

    let forced = fixture.run("two", Options {
        report: true,
        no_cache: true,
    })?;
    assert_eq!(fixture.tracer.take(), [true, true]);
    let report = report_of(&forced)?;
    let ids: Vec<Vec<u32>> = report
        .commands
        .iter()
        .map(|command| {
            assert!(!command.cached, "{command:?}");
            assert_eq!(command.complete(), Some(true), "{command:?}");
            command.processes.iter().map(|process| process.id).collect()
        })
        .collect();
    assert_eq!(ids, [vec![1], vec![2]]);
    Ok(())
}

/// The engine asks the tracer for the tree only when a report is wanted: a run
/// without it asks for none and keeps nothing for a report, and the same run with
/// it asks for the tree of every command it runs.
#[test]
fn the_tree_is_asked_for_only_when_a_report_is_wanted() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.task(
        "two",
        "run(\"tool\", \"out:a\")\nrun(\"tool\", \"out:b\")",
        true,
    )?;
    let no_cache = Options {
        report: false,
        no_cache: true,
    };
    let plain = fixture.run("two", no_cache)?;
    assert_eq!(fixture.tracer.take(), [false, false]);
    assert_eq!(plain.observed, None);
    assert!(plain.observation_report(TASK).is_none());

    let reported = fixture.run("two", Options {
        report: true,
        ..no_cache
    })?;
    assert_eq!(fixture.tracer.take(), [true, true]);
    assert!(reported.observed.is_some());
    Ok(())
}

/// The task program's entry holds its own accesses with their outcomes: the module
/// it loads, read; its `fs.read`, read; its `fs.write`, written; and its existence
/// check of a missing file, a probe with `ENOENT`, which the tracer's error-name
/// query names; and nothing that a command touched. The compact report combines
/// them with the commands' observations.
#[test]
fn the_task_program_entry_holds_its_own_accesses_with_outcomes() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("lib.star", "x = 1\n")?;
    fixture.write("in.txt", "in")?;
    let body = [
        "load(\"lib.star\", \"x\")",
        "env.inherit(\"PATH\")",
        "fs.read(\"in.txt\")",
        "fs.write(\"out.txt\", \"made\")",
        "fs.exists(\"missing.txt\")",
        "run(\"tool\", \"read:cmd.txt\")",
    ]
    .join("\n");
    fixture.task("files", &body, false)?;

    let report = report_of(&fixture.run("files", REPORT)?)?;
    let task_program = report.task_program.as_ref().ok_or("no task program")?;
    assert_eq!(task_program.task, TASK);
    assert_eq!(
        task_program.observations,
        observed(&fixture, &[
            ("in.txt", AccessKind::Read, Outcome::Ok),
            ("lib.star", AccessKind::Read, Outcome::Ok),
            (
                "missing.txt",
                AccessKind::Probe,
                Outcome::Error("ENOENT".to_owned())
            ),
            ("out.txt", AccessKind::Write, Outcome::Ok),
        ])?
    );

    let value = json(&report, ReportMode::Hierarchical)?;
    assert_eq!(at(&value, "/task_program/task")?, &Value::from(TASK));
    let entries = at(&value, "/task_program/observations")?
        .as_array()
        .ok_or("no observations")?;
    let missing = fixture.path("missing.txt").to_string_lossy().into_owned();
    let entry = entries
        .iter()
        .find(|entry| entry.get("path") == Some(&Value::from(missing.as_str())))
        .ok_or("no entry for missing.txt")?;
    assert_eq!(
        at(entry, "/access/probe")?,
        &Value::Array(vec![Value::from("ENOENT")])
    );

    let compact = json(&report, ReportMode::Compact)?;
    let paths: Vec<&str> = at(&compact, "/observations")?
        .as_array()
        .ok_or("no observations")?
        .iter()
        .filter_map(|entry| entry.get("path").and_then(Value::as_str))
        .collect();
    for path in [
        "in.txt",
        "lib.star",
        "missing.txt",
        "out.txt",
        "cmd.txt",
        "bin/tool",
    ] {
        let path = fixture.path(path).to_string_lossy().into_owned();
        assert!(paths.contains(&path.as_str()), "{path} in {paths:?}");
    }
    let text = report.text(ReportMode::Hierarchical);
    assert!(text.contains(&format!("task program: {TASK}\n")), "{text}");
    Ok(())
}

/// A step whose program is not found starts no process, so its command holds the
/// placeholder process, without a process ID or status, whose program is the name
/// that kizz looked up, with the lookup's probe of each `PATH` location, `ENOENT`.
#[test]
fn a_step_whose_program_is_not_found_has_the_placeholder_process() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.task("absent", "run(\"absent\", \"x\")", true)?;

    let run = fixture.run("absent", REPORT)?;
    assert!(!run.succeeded());
    assert_eq!(fixture.tracer.take(), Vec::<bool>::new());
    let report = report_of(&run)?;
    let [command] = report.commands.as_slice() else {
        return Err(format!("expected one command, got {report:?}").into());
    };
    assert!(
        matches!(command.outcome, CommandOutcome::NotFound(_)),
        "{command:?}"
    );
    assert_eq!(command.program, None);
    let [process] = command.processes.as_slice() else {
        return Err(format!("expected one process, got {command:?}").into());
    };
    assert_eq!((process.id, process.pid, process.exit), (1, None, None));
    let [program] = process.programs.as_slice() else {
        return Err(format!("expected one program, got {process:?}").into());
    };
    assert_eq!(program.path, Path::new("absent"));
    assert_eq!(
        program.observations,
        observed(&fixture, &[(
            "bin/absent",
            AccessKind::Probe,
            Outcome::Error("ENOENT".to_owned())
        )])?
    );
    let value = json(&report, ReportMode::Hierarchical)?;
    assert_eq!(
        at(&value, "/commands/0/outcome/kind")?,
        &Value::from("not_found")
    );
    assert_eq!(at(&value, "/commands/0/processes/0/pid")?, &Value::Null);
    Ok(())
}

/// Decision 1's regression through `kizz run --output`: the report of the run of
/// the task `name`, whose one command's fake run reports what `body` asks for and
/// exits with 0, is incomplete at the top and for the command, in the model and in
/// both JSON modes; the command's one reason, in JSON, is `kind`, with `number`
/// and `detail`, on process 1 in a hierarchical report; and the text reads
/// `trace: incomplete`, with `line` under the command.
fn check_incomplete(
    name: &str,
    body: &str,
    kind: &str,
    number: Option<u32>,
    detail: &str,
    line: &str,
) -> TestResult {
    let fixture = Fixture::new()?;
    fixture.task(name, body, true)?;
    let run = fixture.run(name, REPORT)?;
    assert!(run.succeeded());
    let report = report_of(&run)?;
    assert!(!report.complete());
    let command = report.commands.first().ok_or("no command")?;
    assert_eq!(command.complete(), Some(false));
    for mode in [ReportMode::Hierarchical, ReportMode::Compact] {
        let value = json(&report, mode)?;
        assert_eq!(at(&value, "/complete")?, &Value::Bool(false), "{mode:?}");
        assert_eq!(
            at(&value, "/commands/0/complete")?,
            &Value::Bool(false),
            "{mode:?}"
        );
        let reasons = at(&value, "/commands/0/incomplete")?
            .as_array()
            .ok_or("no reasons")?;
        let [reason] = reasons.as_slice() else {
            return Err(format!("expected one reason, got {reasons:?}").into());
        };
        assert_eq!(reason.get("kind"), Some(&Value::from(kind)), "{mode:?}");
        assert_eq!(reason.get("detail"), Some(&Value::from(detail)), "{mode:?}");
        assert_eq!(
            reason.get("number"),
            Some(&number.map_or(Value::Null, Value::from)),
            "{mode:?}"
        );
        if mode == ReportMode::Hierarchical {
            assert_eq!(reason.get("process"), Some(&Value::from(1)), "{mode:?}");
        }
        let text = report.text(mode);
        assert_eq!(text.lines().nth(1), Some("trace: incomplete"), "{text}");
        assert!(text.contains("  trace: incomplete\n"), "{text}");
        let expected = if mode == ReportMode::Hierarchical {
            line.to_owned()
        } else {
            line.replace(", process 1", "")
        };
        assert!(text.contains(&format!("    {expected}\n")), "{text}");
    }
    Ok(())
}

/// Decision 1: a run of a task whose command's fake run reports the unknown system
/// call 400 gives a report that is incomplete at the top and for that command,
/// with an `unknown_system_call` reason with the number 400 on process 1.
#[test]
fn a_command_with_an_unknown_system_call_makes_the_report_incomplete() -> TestResult {
    check_incomplete(
        "odd",
        "run(\"tool\", \"unknown:400\")",
        "unknown_system_call",
        Some(400),
        "the tracer does not know system call 400",
        "unknown system call, process 1: the tracer does not know system call 400",
    )
}

/// Decision 1: a run of a task whose command's fake run reports a reason that the
/// trace is incomplete gives a report that is incomplete at the top and for that
/// command, with the reason's kind and detail on process 1.
#[test]
fn a_command_with_a_reason_makes_the_report_incomplete() -> TestResult {
    check_incomplete(
        "lost",
        "run(\"tool\", \"reason:the tracer lost a process\")",
        "other",
        None,
        "the tracer lost a process",
        "other, process 1: the tracer lost a process",
    )
}
