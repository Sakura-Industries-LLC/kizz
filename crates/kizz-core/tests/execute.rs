// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tests of running a command for `kizz execute` with fake tracers: each outcome's
//! exit status and report, the request that reaches the tracer, the lookup's
//! observations in the first program of process 1, the placeholder process of a
//! command that started no process, a caller environment without `PATH`, and,
//! under the orchestrator's decision 1 of 2026-10-26, reports built from fake runs
//! that report an unknown system call, a reason that the trace is incomplete, or a
//! connection.
//!
//! The one fake tracer, [`Fake`], answers each request with the result or error
//! that a test scripts, and records the requests it gets. A scripted result that
//! the request asks the tree of carries its tree, which [`ProcessTree::of_run`]
//! builds from the whole result, so the tree holds the run's reason, unknown
//! system calls and connections, as the Linux tracer's does; this is the one fake
//! whose runs these tests build reports from. One test then adds a reason, an
//! unknown system call and a connection to the run alone, as the Linux tracer does
//! for a reason that it attributes to no program, to show that the report still
//! says what the run reported. A failed start carries the tree that
//! [`ProcessTree::of_request`] builds, since it reports none of these.
//!
//! Lookups run on real files in temporary directories, and the fake names the
//! errors `ENOENT` and `EACCES`, as the Linux tracer does.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsString,
    fs, io,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    sync::{Mutex, PoisonError},
};

use kizz_core::{
    CommandOutcome, ExecuteRequest, Executed, ReportMode, STATUS_FAILED, STATUS_NOT_EXECUTABLE,
    STATUS_NOT_FOUND, execute, exit_status, not_executed,
    report::{ReasonKind, ReportProcess},
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CapturedOutput, Connection, EchoSink,
    ExtendedAttributes, FirstReads, IncompleteReason, MachineState, Outcome, ProcessTree,
    RunRequest, RunResult, Termination, TraceError, Tracer, unnamed_error,
};
use serde_json::Value;

/// The result of a test.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The answer of the fake tracer to a request.
type Answer = Box<dyn Fn(&RunRequest) -> Result<RunResult, TraceError> + Send + Sync>;

/// How a scripted start fails: the error for a request, given the tree of its one
/// process.
type FailedStart = fn(&RunRequest, ProcessTree) -> TraceError;

/// A fake tracer that answers each request with [`Answer`] and records the
/// requests it gets.
struct Fake {
    /// The scripted answer.
    answer: Answer,
    /// Every request, in order.
    requests: Mutex<Vec<RunRequest>>,
}

impl Fake {
    /// A fake that answers with `answer`.
    fn new(
        answer: impl Fn(&RunRequest) -> Result<RunResult, TraceError> + Send + Sync + 'static
    ) -> Self {
        Self {
            answer: Box::new(answer),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// The requests that the fake got.
    fn requests(&self) -> Vec<RunRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Tracer for Fake {
    /// Record `request` and answer it with the scripted answer.
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
        (self.answer)(request)
    }

    /// No extended attributes, for any path.
    fn extended_attributes(
        &self,
        _path: &Path,
        _follow: bool,
    ) -> io::Result<ExtendedAttributes> {
        Ok(ExtendedAttributes::new())
    }

    /// The default machine state.
    fn machine_state(
        &self,
        _cwd: &Path,
    ) -> MachineState {
        MachineState::default()
    }

    /// `ENOENT` and `EACCES` by their names, as the Linux tracer names them, and
    /// every other number by the default answer.
    fn error_name(
        &self,
        errno: i32,
    ) -> String {
        match errno {
            2 => "ENOENT".to_owned(),
            13 => "EACCES".to_owned(),
            other => unnamed_error(other),
        }
    }
}

/// What a scripted run reports besides how it ended and its accesses.
#[derive(Debug, Clone, Default)]
struct Reported {
    /// The reason that the trace is incomplete, if any.
    incomplete: Option<IncompleteReason>,
    /// The unknown system calls.
    unknown: BTreeSet<u32>,
    /// The connections.
    connections: BTreeSet<Connection>,
}

/// The result of `request` that ended as `termination` says, touched `accesses`,
/// and reported `reported`, with the tree that [`ProcessTree::of_run`] builds from
/// the whole result when the request asks for it.
fn finished(
    request: &RunRequest,
    termination: Termination,
    accesses: Accesses,
    reported: Reported,
) -> RunResult {
    let mut result = RunResult {
        termination,
        output: CapturedOutput::empty(request.capture),
        accesses,
        incomplete: reported.incomplete,
        first_reads: FirstReads::new(),
        unknown_system_calls: reported.unknown,
        connections: reported.connections,
        tree: None,
    };
    if request.tree {
        result.tree = Some(ProcessTree::of_run(request, &result));
    }
    result
}

/// A fake whose run of any request ends as `termination` says, after it ran the
/// program, and reports `reported`.
fn ending(
    termination: Termination,
    reported: Reported,
) -> Fake {
    Fake::new(move |request| {
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Exec);
        Ok(finished(request, termination, accesses, reported.clone()))
    })
}

/// A temporary directory that holds `bin/tool`, executable, `text/tool`, not
/// executable, and the empty directory `empty`, with its path.
fn programs() -> TestResult<(tempfile::TempDir, AbsolutePath)> {
    let dir = tempfile::tempdir()?;
    let root = AbsolutePath::new(dir.path().canonicalize()?)?;
    fs::create_dir(root.as_path().join("empty"))?;
    for (sub, mode) in [("bin", 0o755), ("text", 0o644)] {
        fs::create_dir(root.as_path().join(sub))?;
        let tool = root.as_path().join(sub).join("tool");
        fs::write(&tool, "#!/bin/sh\n")?;
        fs::set_permissions(&tool, fs::Permissions::from_mode(mode))?;
    }
    Ok((dir, root))
}

/// The request of `kizz execute` for `args` in `root`, with a `PATH` of the
/// directories `path` under `root`, when it is given, and one other variable.
fn request(
    root: &AbsolutePath,
    args: &[&str],
    path: Option<&[&str]>,
) -> ExecuteRequest {
    let mut env = BTreeMap::from([(OsString::from("KIZZ_TEST"), OsString::from("1"))]);
    if let Some(directories) = path {
        let joined: Vec<String> = directories
            .iter()
            .map(|directory| root.as_path().join(directory).display().to_string())
            .collect();
        env.insert(OsString::from("PATH"), OsString::from(joined.join(":")));
    }
    ExecuteRequest {
        args: args.iter().map(OsString::from).collect(),
        env,
        cwd: root.clone(),
    }
}

/// Run `request` through `tracer` with a new handle.
fn run(
    tracer: &Fake,
    request: ExecuteRequest,
) -> Executed {
    execute(tracer, request, &Cancellation::new())
}

/// The first process of the report's one command.
fn first_process(executed: &Executed) -> TestResult<&ReportProcess> {
    executed
        .report
        .commands
        .first()
        .and_then(|command| command.processes.first())
        .ok_or_else(|| "the report has no process".into())
}

/// The outcomes of `path` for `kind` in the first program of the first process.
fn outcomes_of(
    executed: &Executed,
    path: &Path,
    kind: AccessKind,
) -> TestResult<BTreeSet<Outcome>> {
    let program = first_process(executed)?
        .programs
        .first()
        .ok_or("the first process has no program")?;
    Ok(program
        .observations
        .get(&AbsolutePath::new(path)?)
        .and_then(|kinds| kinds.get(&kind))
        .cloned()
        .unwrap_or_default())
}

/// The outcomes that `names` name: `ok`, or an error's name.
fn named(names: &[&str]) -> BTreeSet<Outcome> {
    names
        .iter()
        .map(|name| {
            if *name == "ok" {
                Outcome::Ok
            } else {
                Outcome::Error((*name).to_owned())
            }
        })
        .collect()
}

/// The program, found through the caller's `PATH`, runs with the caller's
/// environment, unchanged, and working directory, with the arguments as given,
/// keeping the terminal and with the process tree; its exit code is the status.
#[test]
fn the_program_runs_with_the_caller_s_environment_and_directory() -> TestResult {
    let (_dir, root) = programs()?;
    let tracer = ending(Termination::Exited(3), Reported::default());
    let given = request(&root, &["tool", "a b", ""], Some(&["empty", "bin"]));
    let executed = run(&tracer, given.clone());
    assert_eq!(executed.outcome, CommandOutcome::Exited(3));
    assert_eq!(executed.status, 3);
    let [sent]: [RunRequest; 1] = tracer
        .requests()
        .try_into()
        .map_err(|_| "expected one request")?;
    assert_eq!(sent.program, root.join("bin/tool"));
    assert_eq!(sent.args, given.args);
    assert_eq!(sent.env, given.env);
    assert_eq!(sent.cwd, root);
    assert!(sent.terminal && sent.tree && !sent.echo);
    assert_eq!(sent.stdin, None);
    let command = executed.report.commands.first().ok_or("no command")?;
    assert_eq!(command.program, Some(root.join("bin/tool")));
    assert_eq!(command.argv, given.args);
    assert_eq!(command.cwd, root);
    assert_eq!(command.complete(), Some(true));
    assert!(executed.report.complete());
    Ok(())
}

/// Each location that the lookup checked is an observation of the first program of
/// process 1: the directory that lacks the program with `ENOENT`, and the found
/// program with `ok`, beside what the program itself did.
#[test]
fn the_lookup_s_observations_belong_to_the_first_program_of_process_1() -> TestResult {
    let (_dir, root) = programs()?;
    let tracer = ending(Termination::Exited(0), Reported::default());
    let executed = run(&tracer, request(&root, &["tool"], Some(&["empty", "bin"])));
    let process = first_process(&executed)?;
    assert_eq!(process.id, 1);
    let missing = root.as_path().join("empty/tool");
    let found = root.as_path().join("bin/tool");
    assert_eq!(
        outcomes_of(&executed, &missing, AccessKind::Probe)?,
        named(&["ENOENT"])
    );
    assert_eq!(
        outcomes_of(&executed, &found, AccessKind::Probe)?,
        named(&["ok"])
    );
    assert_eq!(
        outcomes_of(&executed, &found, AccessKind::Exec)?,
        named(&["ok"])
    );
    Ok(())
}

/// A name that no directory of `PATH` holds is `not_found`, with status 127: no
/// process starts, and the report holds the placeholder process, without a process
/// ID or status, whose program is the name, with every checked location.
#[test]
fn a_name_that_path_does_not_hold_is_not_found() -> TestResult {
    let (_dir, root) = programs()?;
    let tracer = ending(Termination::Exited(0), Reported::default());
    let executed = run(
        &tracer,
        request(&root, &["absent", "x"], Some(&["empty", "bin"])),
    );
    assert!(matches!(executed.outcome, CommandOutcome::NotFound(_)));
    assert_eq!(executed.status, STATUS_NOT_FOUND);
    assert_eq!(tracer.requests(), Vec::<RunRequest>::new());
    let process = first_process(&executed)?;
    assert_eq!(
        (process.pid, process.parent, process.exit),
        (None, None, None)
    );
    let [program] = process.programs.as_slice() else {
        panic!("expected one program, got {process:?}");
    };
    assert_eq!(program.path, Path::new("absent"));
    assert_eq!(program.args, vec![
        OsString::from("absent"),
        OsString::from("x")
    ]);
    for directory in ["empty", "bin"] {
        assert_eq!(
            outcomes_of(
                &executed,
                &root.as_path().join(directory).join("absent"),
                AccessKind::Probe
            )?,
            named(&["ENOENT"]),
            "{directory}"
        );
    }
    let command = executed.report.commands.first().ok_or("no command")?;
    assert_eq!(command.program, None);
    assert_eq!(command.complete(), Some(true));
    let text = executed.report.text(ReportMode::Hierarchical);
    assert!(
        text.contains("  outcome: not found: program `absent` was not found in PATH\n"),
        "{text}"
    );
    assert!(text.contains("  process 1, not started\n"), "{text}");
    Ok(())
}

/// Without `PATH` in the caller's environment, a name without `/` is `not_found`,
/// with a message that says that `PATH` is not set, and nothing is checked; a name
/// with `/` still runs.
#[test]
fn without_path_a_name_without_a_slash_is_not_found() -> TestResult {
    let (_dir, root) = programs()?;
    let tracer = ending(Termination::Exited(0), Reported::default());
    let executed = run(&tracer, request(&root, &["tool"], None));
    let CommandOutcome::NotFound(message) = &executed.outcome else {
        panic!("expected not_found, got {:?}", executed.outcome);
    };
    assert!(message.contains("PATH is not set"), "{message}");
    assert_eq!(executed.status, STATUS_NOT_FOUND);
    assert_eq!(tracer.requests(), Vec::<RunRequest>::new());
    let process = first_process(&executed)?;
    assert_eq!((process.pid, process.exit), (None, None));
    assert!(
        process
            .programs
            .iter()
            .all(|program| program.observations.is_empty())
    );

    let by_path = run(&tracer, request(&root, &["./bin/tool"], None));
    assert_eq!(by_path.status, 0);
    let [sent]: [RunRequest; 1] = tracer
        .requests()
        .try_into()
        .map_err(|_| "expected one request")?;
    assert_eq!(sent.program, root.join("bin/tool"));
    Ok(())
}

/// A lookup that finds only a file without execute permission is
/// `not_executable`, with status 126, and no process starts.
#[test]
fn a_program_that_the_lookup_finds_not_executable_is_not_executable() -> TestResult {
    let (_dir, root) = programs()?;
    let tracer = ending(Termination::Exited(0), Reported::default());
    let executed = run(&tracer, request(&root, &["tool"], Some(&["empty", "text"])));
    assert!(matches!(executed.outcome, CommandOutcome::NotExecutable(_)));
    assert_eq!(executed.status, STATUS_NOT_EXECUTABLE);
    assert_eq!(tracer.requests(), Vec::<RunRequest>::new());
    assert_eq!(
        outcomes_of(
            &executed,
            &root.as_path().join("text/tool"),
            AccessKind::Probe
        )?,
        named(&["ok"])
    );
    Ok(())
}

/// Each outcome of the tracer gives its exit status and its report: an exit code,
/// a signal as 128 plus its number, a missing program 127, a program that cannot be
/// run or started 126, and a tracer failure 125, whose report is incomplete with a
/// `tracer_failed` reason; failed starts keep the tree that the tracer gives.
#[test]
fn each_outcome_has_its_status_and_report() -> TestResult {
    let (_dir, root) = programs()?;
    let tool = request(&root, &["tool"], Some(&["bin"]));
    let signaled = run(
        &ending(Termination::Signaled(15), Reported::default()),
        tool.clone(),
    );
    assert_eq!(signaled.outcome, CommandOutcome::Signaled(15));
    assert_eq!(signaled.status, 143);
    assert_eq!(
        first_process(&signaled)?.exit,
        Some(Termination::Signaled(15))
    );

    let start_tree = |request: &RunRequest| {
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Exec);
        ProcessTree::of_request(request, &accesses, None)
    };
    let failures: [(FailedStart, &str, u8); 3] = [
        (
            |request, tree| {
                TraceError::ProgramNotFound {
                    program: request.program.clone(),
                    accesses: tree.accesses(),
                    tree: Some(tree),
                }
            },
            "not_found",
            STATUS_NOT_FOUND,
        ),
        (
            |request, tree| {
                TraceError::ProgramNotExecutable {
                    program: request.program.clone(),
                    source: io::Error::from(io::ErrorKind::PermissionDenied),
                    accesses: tree.accesses(),
                    tree: Some(tree),
                }
            },
            "not_executable",
            STATUS_NOT_EXECUTABLE,
        ),
        (
            |request, tree| {
                TraceError::StartFailed {
                    program: request.program.clone(),
                    source: io::Error::from(io::ErrorKind::OutOfMemory),
                    accesses: tree.accesses(),
                    tree: Some(tree),
                }
            },
            "start_failed",
            STATUS_NOT_EXECUTABLE,
        ),
    ];
    for (error, kind, status) in failures {
        let tracer = Fake::new(move |request| Err(error(request, start_tree(request))));
        let executed = run(&tracer, tool.clone());
        assert_eq!(executed.outcome.kind(), kind);
        assert_eq!(executed.status, status, "{kind}");
        let process = first_process(&executed)?;
        assert_eq!((process.pid, process.exit), (None, None), "{kind}");
        assert_eq!(
            outcomes_of(
                &executed,
                &root.as_path().join("bin/tool"),
                AccessKind::Exec
            )?,
            named(&["ok"]),
            "{kind}"
        );
        assert!(executed.report.complete(), "{kind}");
    }

    let tracer = Fake::new(|_| {
        Err(TraceError::Tracer {
            message: "cannot trace".to_owned(),
            source: None,
        })
    });
    let failed = run(&tracer, tool);
    assert_eq!(failed.outcome.kind(), "failed");
    assert_eq!(failed.status, STATUS_FAILED);
    assert!(!failed.report.complete());
    let command = failed.report.commands.first().ok_or("no command")?;
    assert_eq!(command.complete(), Some(false));
    assert!(
        command
            .incomplete
            .iter()
            .any(|reason| reason.kind == ReasonKind::TracerFailed && reason.process == Some(1)),
        "{command:?}"
    );
    Ok(())
}

/// The exit status of each outcome, with an exit code's low eight bits.
#[test]
fn exit_statuses_follow_the_outcome() {
    let cases = [
        (CommandOutcome::Exited(0), 0),
        (CommandOutcome::Exited(3), 3),
        (CommandOutcome::Exited(255), 255),
        (CommandOutcome::Exited(259), 3),
        (CommandOutcome::Signaled(2), 130),
        (CommandOutcome::Signaled(9), 137),
        (CommandOutcome::Signaled(15), 143),
        (CommandOutcome::NotFound(String::new()), 127),
        (CommandOutcome::NotExecutable(String::new()), 126),
        (CommandOutcome::StartFailed(String::new()), 126),
        (CommandOutcome::Failed(String::new()), 125),
    ];
    for (outcome, status) in cases {
        assert_eq!(exit_status(&outcome), status, "{outcome:?}");
    }
}

/// A command that kizz did not run because it failed itself is `failed`, with
/// status 125 and an incomplete report whose command holds the placeholder process.
#[test]
fn a_command_that_kizz_did_not_run_is_failed() -> TestResult {
    let (_dir, root) = programs()?;
    let executed = not_executed(
        request(&root, &["tool"], Some(&["bin"])),
        "cannot handle signals".to_owned(),
    );
    assert_eq!(
        executed.outcome,
        CommandOutcome::Failed("cannot handle signals".to_owned())
    );
    assert_eq!(executed.status, STATUS_FAILED);
    assert!(!executed.report.complete());
    let process = first_process(&executed)?;
    assert_eq!((process.pid, process.exit), (None, None));
    Ok(())
}

/// The JSON of `executed`'s report in `mode`, parsed.
fn json(
    executed: &Executed,
    mode: ReportMode,
) -> TestResult<Value> {
    Ok(serde_json::from_str(&executed.report.json(mode))?)
}

/// The report of a run that `reported` makes incomplete, and that exited with 0:
/// the status stays 0; `complete` is false at the top and for the command, in the
/// model and in both JSON modes; the command's one reason, in JSON, is `kind`,
/// with `number` and `detail`, on process 1 in a hierarchical report; and the text
/// reads `trace: incomplete`, with `line` under the command.
fn check_incomplete(
    reported: Reported,
    kind: &str,
    number: Option<u32>,
    detail: &str,
    line: &str,
) -> TestResult {
    let (_dir, root) = programs()?;
    let tracer = ending(Termination::Exited(0), reported);
    let executed = run(&tracer, request(&root, &["tool"], Some(&["bin"])));
    assert_eq!(executed.outcome, CommandOutcome::Exited(0));
    assert_eq!(executed.status, 0);
    assert!(!executed.report.complete());
    let command = executed.report.commands.first().ok_or("no command")?;
    assert_eq!(command.complete(), Some(false));
    for mode in [ReportMode::Hierarchical, ReportMode::Compact] {
        let value = json(&executed, mode)?;
        assert_eq!(value.get("complete"), Some(&Value::Bool(false)), "{mode:?}");
        let command = value.pointer("/commands/0").ok_or("no command")?;
        assert_eq!(
            command.get("complete"),
            Some(&Value::Bool(false)),
            "{mode:?}"
        );
        let reasons = command
            .get("incomplete")
            .and_then(Value::as_array)
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
        let text = executed.report.text(mode);
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

/// Decision 1: a fake run that reports the unknown system call 400 and exits with
/// 0 gives status 0 and a report that is incomplete at the top and for the
/// command, with an `unknown_system_call` reason with the number 400 on process 1.
#[test]
fn a_fake_run_with_an_unknown_system_call_gives_an_incomplete_report() -> TestResult {
    check_incomplete(
        Reported {
            unknown: BTreeSet::from([400]),
            ..Reported::default()
        },
        "unknown_system_call",
        Some(400),
        "the tracer does not know system call 400",
        "unknown system call, process 1: the tracer does not know system call 400",
    )
}

/// Decision 1: a fake run that reports a reason that the trace is incomplete and
/// exits with 0 gives status 0 and a report that is incomplete at the top and for
/// the command, with the reason's kind and detail on process 1.
#[test]
fn a_fake_run_with_a_reason_gives_an_incomplete_report() -> TestResult {
    check_incomplete(
        Reported {
            incomplete: Some(IncompleteReason::Other {
                detail: "the tracer lost a process".to_owned(),
            }),
            ..Reported::default()
        },
        "other",
        None,
        "the tracer lost a process",
        "other, process 1: the tracer lost a process",
    )
}

/// Decision 1: a fake run that reports a connection gives a report that lists it
/// under process 1's program and among the compact report's connections.
#[test]
fn a_fake_run_with_a_connection_lists_it() -> TestResult {
    let (_dir, root) = programs()?;
    let connection = Connection::Ip("127.0.0.1:80".parse()?);
    let tracer = ending(Termination::Exited(0), Reported {
        connections: BTreeSet::from([connection.clone()]),
        ..Reported::default()
    });
    let executed = run(&tracer, request(&root, &["tool"], Some(&["bin"])));
    let program = first_process(&executed)?
        .programs
        .first()
        .ok_or("no program")?;
    assert_eq!(program.connections, BTreeSet::from([connection.clone()]));
    assert_eq!(executed.report.connections, BTreeSet::from([connection]));
    assert_eq!(executed.report.totals.connections, 1);
    assert!(executed.report.complete());
    let hierarchical = json(&executed, ReportMode::Hierarchical)?;
    assert_eq!(
        hierarchical["commands"][0]["processes"][0]["programs"][0]["connections"][0]["address"],
        Value::from("127.0.0.1:80")
    );
    let compact = json(&executed, ReportMode::Compact)?;
    assert_eq!(
        compact["connections"][0]["address"],
        Value::from("127.0.0.1:80")
    );
    let text = executed.report.text(ReportMode::Compact);
    assert!(text.contains("connections:\n  ip 127.0.0.1:80\n"), "{text}");
    Ok(())
}

/// A run whose tree leaves out the run's reason, unknown system calls and
/// connections, as the Linux tracer's tree does for a reason that it attributes to
/// no program, or that gives no tree, still gives a report that says what the run
/// reported: everything is put on the first program of process 1, never claiming
/// a complete trace that the tracer reported otherwise (E7). The tree is the one
/// that [`ProcessTree::of_run`] builds from the run before the run reports them.
#[test]
fn a_tree_that_leaves_out_what_the_run_reports_still_gives_it() -> TestResult {
    let (_dir, root) = programs()?;
    let reason = IncompleteReason::UntraceableSystemCall {
        detail: "process 1 made `chroot`".to_owned(),
    };
    let connection = Connection::UnixAbstract(b"kizz".to_vec());
    for with_tree in [true, false] {
        let reason = reason.clone();
        let reported = connection.clone();
        let tracer = Fake::new(move |request| {
            let mut accesses = Accesses::new();
            accesses.record(request.program.clone(), AccessKind::Exec);
            let mut result = finished(
                request,
                Termination::Exited(0),
                accesses,
                Reported::default(),
            );
            if !with_tree {
                result.tree = None;
            }
            result.incomplete = Some(reason.clone());
            result.unknown_system_calls = BTreeSet::from([400]);
            result.connections = BTreeSet::from([reported.clone()]);
            Ok(result)
        });
        let executed = run(&tracer, request(&root, &["tool"], Some(&["bin"])));
        assert!(!executed.report.complete(), "{with_tree}");
        let command = executed.report.commands.first().ok_or("no command")?;
        let kinds: BTreeSet<(ReasonKind, Option<u32>, Option<u32>)> = command
            .incomplete
            .iter()
            .map(|reason| (reason.kind, reason.number, reason.process))
            .collect();
        assert_eq!(
            kinds,
            BTreeSet::from([
                (ReasonKind::UntraceableSystemCall, None, Some(1)),
                (ReasonKind::UnknownSystemCall, Some(400), Some(1)),
            ]),
            "{with_tree}"
        );
        assert_eq!(
            executed.report.connections,
            BTreeSet::from([connection.clone()]),
            "{with_tree}"
        );
        assert_eq!(
            outcomes_of(
                &executed,
                &root.as_path().join("bin/tool"),
                AccessKind::Exec
            )?,
            named(&["ok"]),
            "{with_tree}"
        );
    }
    Ok(())
}
