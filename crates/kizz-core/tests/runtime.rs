// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Starlark runtime of task programs, with a fake step executor.
//!
//! Each test writes its `kizz.md` and modules into a temporary directory, parses
//! the task block, loads and runs its program, and checks the step specifications
//! the fake executor received, the lines the program printed, and the failure;
//! for equivalent forms of `run()`'s list and tuple arguments, it also checks the
//! step keys that `kizz_core::step_key` derives from the specifications.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, borrow cells, lock and print"
)]

use std::{
    cell::RefCell,
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use kizz_core::{
    PrintSink, ProgramHash, StepError, StepExecutor, StepOutcome, StepSpec, TaskFailure, TaskInfo,
    TaskProgram, TaskRun, UncheckedExit, parse_tasks,
};
use kizz_trace::{AbsolutePath, AccessKind, Accesses, CaptureMode, CapturedOutput, Termination};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The script of a fake executor: the outcome of each step, from its specification.
type Script = dyn Fn(&StepSpec) -> Result<StepOutcome, StepError>;

/// A step executor that records every step and answers from a script.
struct FakeExecutor {
    /// The steps received, in order.
    steps: RefCell<Vec<StepSpec>>,
    /// The outcome of each step.
    script: Box<Script>,
}

impl FakeExecutor {
    /// An executor whose steps all exit with status 0 and write their arguments to
    /// standard output.
    fn new() -> Self {
        Self::scripted(|step| {
            Ok(outcome(
                Termination::Exited(0),
                step.capture,
                step.argv.join(" ").as_bytes(),
                b"",
            ))
        })
    }

    /// An executor that answers with `script`.
    fn scripted(script: impl Fn(&StepSpec) -> Result<StepOutcome, StepError> + 'static) -> Self {
        Self {
            steps: RefCell::new(Vec::new()),
            script: Box::new(script),
        }
    }

    /// The steps received, in order.
    fn steps(&self) -> Vec<StepSpec> {
        self.steps.borrow().clone()
    }
}

impl StepExecutor for FakeExecutor {
    fn execute(
        &self,
        step: &StepSpec,
    ) -> Result<StepOutcome, StepError> {
        self.steps.borrow_mut().push(step.clone());
        (self.script)(step)
    }
}

/// The first argument of `step`, after the program.
fn first_argument(step: &StepSpec) -> &str {
    step.argv.get(1).map_or("", String::as_str)
}

/// An outcome with the output in the shape of `capture`: `stdout` and `stderr`
/// separately, or both in order.
fn outcome(
    termination: Termination,
    capture: CaptureMode,
    stdout: &[u8],
    stderr: &[u8],
) -> StepOutcome {
    let output = match capture {
        CaptureMode::Split => {
            CapturedOutput::Split {
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
            }
        },
        CaptureMode::Merged => {
            CapturedOutput::Merged {
                output: [stdout, stderr].concat(),
            }
        },
    };
    StepOutcome {
        termination,
        output,
        cached: false,
    }
}

/// Collects the lines programs print.
#[derive(Default)]
struct Printed {
    /// The lines, in order.
    lines: RefCell<Vec<String>>,
}

impl Printed {
    /// The lines printed so far.
    fn lines(&self) -> Vec<String> {
        self.lines.borrow().clone()
    }
}

impl PrintSink for Printed {
    fn print_line(
        &self,
        line: &str,
    ) {
        self.lines.borrow_mut().push(line.to_owned());
    }
}

/// A project directory with one `kizz.md`.
struct Fixture {
    /// The project directory.
    dir: TempDir,
}

impl Fixture {
    /// An empty project directory.
    fn new() -> TestResult<Self> {
        Ok(Self {
            dir: tempfile::tempdir()?,
        })
    }

    /// The project directory.
    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write `content` to `relative` in the project, creating parent directories.
    fn write(
        &self,
        relative: &str,
        content: &str,
    ) -> TestResult<PathBuf> {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(path)
    }

    /// Write `markdown` as the project's `kizz.md` and return its first task,
    /// selected by `ci`.
    fn task(
        &self,
        markdown: &str,
    ) -> TestResult<TaskInfo> {
        let file = self.write("kizz.md", markdown)?;
        let parsed = parse_tasks(&file, Path::new("kizz.md"), markdown);
        if !parsed.errors.is_empty() {
            return Err(format!("configuration errors: {:?}", parsed.errors).into());
        }
        let task = parsed.tasks.first().ok_or("no task in the fixture")?;
        Ok(TaskInfo::new(task, &["ci".to_owned()])?)
    }

    /// A task with the heading `Build it` and no name, tagged `ci check`, whose block
    /// is `source`.
    fn block(
        &self,
        source: &str,
    ) -> TestResult<TaskInfo> {
        self.task(&format!(
            "# Build it\n\n```starlark ci check\n{source}```\n"
        ))
    }
}

/// Load the program of `task`, which must load.
fn load(task: TaskInfo) -> TestResult<(TaskProgram, Accesses)> {
    let load = TaskProgram::load(task, &Printed::default());
    Ok((load.program?, load.accesses))
}

/// Load and run the program of `task`, which must load, returning its result and
/// printed lines.
fn run(
    task: TaskInfo,
    executor: &FakeExecutor,
) -> TestResult<(Result<(), TaskFailure>, Vec<String>)> {
    let printed = Printed::default();
    let program = TaskProgram::load(task, &printed).program?;
    let result = program.run(&BTreeMap::new(), executor, &printed).result;
    Ok((result, printed.lines()))
}

/// The message of a program failure.
fn program_message(result: Result<(), TaskFailure>) -> TestResult<String> {
    match result {
        Err(TaskFailure::Program { message }) => Ok(message),
        other => Err(format!("expected a program failure, got {other:?}").into()),
    }
}

/// The program hash of `task`.
fn hash(task: TestResult<TaskInfo>) -> TestResult<ProgramHash> {
    Ok(load(task?)?.0.hash())
}

/// With `expect = None`, `run()` returns whatever the exit status, and the program
/// sees `rc` and `signal`.
#[test]
fn without_expect_run_always_returns() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::scripted(|step| {
        let termination = if step.argv.first().is_some_and(|program| program == "fails") {
            Termination::Exited(3)
        } else {
            Termination::Signaled(9)
        };
        Ok(outcome(termination, step.capture, b"", b""))
    });
    let task = fixture.block(
        "a = run(\"fails\")\nprint(a.rc, a.signal)\nb = run(\"killed\", expect = None)\nprint(b.rc, \
         b.signal)\n",
    )?;
    let (result, printed) = run(task, &executor)?;
    result?;
    assert_eq!(printed, ["Build it: 3 None", "Build it: None 9"]);
    Ok(())
}

/// With an `expect` list, an exit code in the list returns, and the task fails on
/// any other exit code or a signal, with the command, its exit status, and its
/// captured output.
#[test]
fn expect_fails_the_task_only_on_unexpected_exit_codes() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::scripted(|step| {
        let termination = match first_argument(step) {
            "one" => Termination::Exited(1),
            "two" => Termination::Exited(2),
            _ => Termination::Signaled(15),
        };
        Ok(outcome(
            termination,
            step.capture,
            b"partial out\n",
            b"bad \xff err\n",
        ))
    });

    let task = fixture.block("r = run(\"tool\", \"one\", expect = [0, 1])\nprint(r.rc)\n")?;
    let (result, printed) = run(task, &executor)?;
    result?;
    assert_eq!(printed, ["Build it: 1"]);

    let task = fixture.block(
        "run(\"tool\", \"one\", expect = [0, 1])\nrun(\"tool\", \"two\", expect = [0, \
         1])\nprint(\"not reached\")\n",
    )?;
    let (result, printed) = run(task, &executor)?;
    assert_eq!(printed, Vec::<String>::new());
    let Err(TaskFailure::UnexpectedExit { exit, message }) = result else {
        panic!("expected an unexpected exit, got {result:?}");
    };
    assert_eq!(exit.argv, ["tool", "two"]);
    assert_eq!(exit.termination, Termination::Exited(2));
    assert_eq!(exit.expect, [0, 1]);
    assert_eq!(exit.output, CapturedOutput::Split {
        stdout: b"partial out\n".to_vec(),
        stderr: b"bad \xff err\n".to_vec(),
    });
    assert!(
        message.contains("run(): command `tool two`: exit code 2, which `expect` does not accept"),
        "{message}"
    );
    let shown = TaskFailure::UnexpectedExit { exit, message }.to_string();
    let replaced = format!("--- stderr ---\nbad {} err\n", char::REPLACEMENT_CHARACTER);
    for part in [
        "command `tool two`: exit code 2, but `expect` accepts exit code 0 or 1",
        "--- stdout ---\npartial out\n",
        &replaced,
    ] {
        assert!(shown.contains(part), "{part:?} in {shown}");
    }

    let task = fixture.block("run(\"tool\", \"signal\", expect = [0, 1])\n")?;
    let (result, _) = run(task, &executor)?;
    let Err(TaskFailure::UnexpectedExit { exit, .. }) = result else {
        panic!("expected an unexpected exit, got {result:?}");
    };
    assert_eq!(exit.termination, Termination::Signaled(15));
    Ok(())
}

/// Captured output passed through arguments, `stdin`, or the environment reaches
/// the next step's specification.
#[test]
fn captured_output_reaches_the_next_step() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::scripted(|step| {
        let stdout: &[u8] = if step.argv.first().is_some_and(|program| program == "git") {
            b"v1.2.3\n"
        } else {
            b""
        };
        Ok(outcome(Termination::Exited(0), step.capture, stdout, b""))
    });
    let task = fixture.block(
        "version = run(\"git\", \"describe\").stdout.strip()\nrun(\"echo\", version)\nrun(\"cat\", \
         stdin = version)\nrun(\"env\", env = {\"VERSION\": version})\n",
    )?;
    let (result, _) = run(task, &executor)?;
    result?;
    let steps = executor.steps();
    assert_eq!(steps.len(), 4);
    assert_eq!(steps[1].argv, ["echo", "v1.2.3"]);
    assert_eq!(steps[2].stdin.as_deref(), Some("v1.2.3"));
    assert_eq!(
        steps[3].env,
        BTreeMap::from([("VERSION".to_owned(), "v1.2.3".to_owned())])
    );
    Ok(())
}

/// The step specification carries every `run()` parameter, `cwd` resolves against
/// the task directory, and every task starts with an empty environment.
#[test]
fn the_step_specification_carries_every_parameter() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::new();
    let task = fixture.block(
        "run(\"prog\")\nrun(\"prog\", \"a b\", \"c\", stdin = \"input\", env = {\"A\": \"1\", \
         \"B\": None}, cwd = \"sub/dir\", capture = \"merged\", stream = True, always = True, \
         ignore = [\"*.log\", \"tmp/**\"], track = [\"/proc/cpuinfo\"], timestamps = True, machine \
         = [\"memory\", \"kernel\", \"memory\"], expect = [0, 2])\nrun(\"prog\", cwd = \
         \"/elsewhere\")\n",
    )?;
    let dir = AbsolutePath::new(fixture.path())?;
    let (result, _) = run(task, &executor)?;
    result?;
    assert_eq!(executor.steps(), [
        StepSpec {
            argv: vec!["prog".to_owned()],
            stdin: None,
            env: BTreeMap::new(),
            cwd: dir.clone(),
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            untraced: false,
            network: false,
            ignore: vec![],
            track: vec![],
            timestamps: false,
            machine: vec![],
            expect: None,
        },
        StepSpec {
            argv: vec!["prog".to_owned(), "a b".to_owned(), "c".to_owned()],
            stdin: Some("input".to_owned()),
            env: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
            cwd: dir.join("sub/dir"),
            capture: CaptureMode::Merged,
            stream: true,
            always: true,
            untraced: false,
            network: false,
            ignore: vec!["*.log".to_owned(), "tmp/**".to_owned()],
            track: vec!["/proc/cpuinfo".to_owned()],
            timestamps: true,
            machine: vec!["kernel".to_owned(), "memory".to_owned()],
            expect: Some(vec![0, 2]),
        },
        StepSpec {
            argv: vec!["prog".to_owned()],
            stdin: None,
            env: BTreeMap::new(),
            cwd: AbsolutePath::new("/elsewhere")?,
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            untraced: false,
            network: false,
            ignore: vec![],
            track: vec![],
            timestamps: false,
            machine: vec![],
            expect: None,
        },
    ]);
    Ok(())
}

/// Invalid `run()` calls fail the task: no program, an unknown capture mode, an
/// unknown parameter, a `timestamps` value that is not a boolean, an unknown
/// `machine` part, and a step the executor cannot run.
#[test]
fn invalid_run_calls_fail_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    for (source, expected) in [
        ("run()\n", "run() needs a program"),
        (
            "run(\"p\", capture = \"both\")\n",
            "capture must be \"split\" or \"merged\"",
        ),
        ("run(\"p\", shell = True)\n", "shell"),
        (
            "run(\"p\", timestamps = \"yes\")\n",
            "Type of parameter `timestamps` doesn't match, expected `bool`",
        ),
        (
            "run(\"p\", machine = [\"kernel\", \"cpu\"])\n",
            "run(): machine has no part \"cpu\"; the parts are kernel, limits, affinity, \
             memory, filesystem and disk",
        ),
        ("run(\"missing\")\n", "run(): no such program: missing"),
    ] {
        let executor = FakeExecutor::scripted(|step| {
            Err(StepError::new(format!(
                "no such program: {}",
                step.argv.join(" ")
            )))
        });
        let message = program_message(run(fixture.block(source)?, &executor)?.0)?;
        assert!(message.contains(expected), "{expected:?} in {message}");
    }
    Ok(())
}

/// `timestamps = "yes"` fails the task with Starlark's argument check, which names
/// the parameter, before any step runs.
#[test]
fn a_timestamps_value_that_is_not_a_boolean_fails_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::new();
    let task = fixture.block("run(\"p\", timestamps = \"yes\")\n")?;
    let message = program_message(run(task, &executor)?.0)?;
    assert!(
        message.contains("Type of parameter `timestamps` doesn't match, expected `bool`"),
        "{message}"
    );
    assert_eq!(executor.steps(), []);
    Ok(())
}

/// An unknown `machine` part fails the task with an error that lists every part,
/// before any step runs, and so does a value that is not a list of strings.
#[test]
fn an_unknown_machine_part_fails_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    for (source, expected) in [
        (
            "run(\"p\", machine = [\"Kernel\"])\n",
            "run(): machine has no part \"Kernel\"; the parts are kernel, limits, affinity, \
             memory, filesystem and disk",
        ),
        (
            "run(\"p\", machine = \"kernel\")\n",
            "Type of parameter `machine` doesn't match",
        ),
    ] {
        let executor = FakeExecutor::new();
        let message = program_message(run(fixture.block(source)?, &executor)?.0)?;
        assert!(message.contains(expected), "{expected:?} in {message}");
        assert_eq!(executor.steps(), []);
    }
    Ok(())
}

/// The result has `stdout` and `stderr` only with `split`, `output` only with
/// `merged`, and `cached`; output is decoded as UTF-8 with invalid bytes replaced.
#[test]
fn result_attributes_follow_the_capture_mode() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::scripted(|step| {
        let mut outcome = outcome(
            Termination::Exited(0),
            step.capture,
            b"ok \xff\xfe\n",
            b"warn\n",
        );
        outcome.cached = step.argv.first().is_some_and(|program| program == "cached");
        Ok(outcome)
    });
    let task = fixture.block(
        "s = run(\"cached\")\nprint(hasattr(s, \"stdout\"), hasattr(s, \"stderr\"), hasattr(s, \
         \"output\"), s.cached)\nprint(s.stdout.replace(\"\\n\", \"|\"), s.stderr.replace(\"\\n\", \
         \"|\"))\nm = run(\"fresh\", capture = \"merged\")\nprint(hasattr(m, \"stdout\"), \
         hasattr(m, \"stderr\"), hasattr(m, \"output\"), m.cached)\nprint(m.output.replace(\"\\n\", \
         \"|\"))\n",
    )?;
    let (result, printed) = run(task, &executor)?;
    result?;
    let invalid = char::REPLACEMENT_CHARACTER.to_string().repeat(2);
    assert_eq!(printed, [
        "Build it: True True False True".to_owned(),
        format!("Build it: ok {invalid}| warn|"),
        "Build it: False False True False".to_owned(),
        format!("Build it: ok {invalid}|warn|"),
    ]);
    Ok(())
}

/// Top-level `if` and `for` work, `json.encode` and `json.decode` are available,
/// and `print` output is prefixed with the task's heading when it has no name, or
/// with its location when it has neither, one line at a time.
#[test]
fn extended_dialect_json_and_prefixed_print() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::new();
    let task = fixture.block(
        "for name in [\"a\", \"b\"]:\n    if name == \"b\":\n        print(\"got\", \
         name)\nprint(json.encode({\"k\": [1, True]}))\nprint(json.decode('{\"x\": \
         2}')[\"x\"])\nprint(\"two\\nlines\")\n",
    )?;
    let (result, printed) = run(task, &executor)?;
    result?;
    assert_eq!(printed, [
        "Build it: got b",
        "Build it: {\"k\":[1,true]}",
        "Build it: 2",
        "Build it: two",
        "Build it: lines",
    ]);

    let task = fixture.task("```starlark ci\nprint(\"no heading\")\n```\n")?;
    let (result, printed) = run(task, &executor)?;
    result?;
    assert_eq!(printed, ["kizz.md:1: no heading"]);
    Ok(())
}

/// `fail()` fails the task with its message and the Starlark stack.
#[test]
fn fail_fails_the_task_with_message_and_stack() -> TestResult {
    let fixture = Fixture::new()?;
    let task = fixture.block(
        "def check(size):\n    if size > 1:\n        fail(\"too big: \" + str(size))\n\ncheck(2)\n",
    )?;
    let message = program_message(run(task, &FakeExecutor::new())?.0)?;
    for part in ["too big: 2", "Traceback", "in check", "kizz.md:8"] {
        assert!(message.contains(part), "{part:?} in {message}");
    }
    Ok(())
}

/// A Starlark error fails the task with its message and the Starlark stack, with
/// line numbers of the `kizz.md`.
#[test]
fn starlark_errors_fail_the_task_with_message_and_stack() -> TestResult {
    let fixture = Fixture::new()?;
    let task = fixture.block("def pick(items):\n    return items[3]\n\npick([1])\n")?;
    let message = program_message(run(task, &FakeExecutor::new())?.0)?;
    for part in [
        "out of bound",
        "Traceback",
        "in <module>",
        "return items[3]",
        "kizz.md:5",
        "kizz.md:7",
    ] {
        assert!(message.contains(part), "{part:?} in {message}");
    }
    Ok(())
}

/// `task` describes the running task, in the block and in module functions: a task
/// without `name=` has the name `None` and its heading as `task.heading`, and
/// `task.args` is an empty list under a build.
#[test]
fn task_values_match_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("lib/info.star", "def heading():\n    return task.heading\n")?;
    let task = fixture.block(
        "load(\"lib/info.star\", \"heading\")\nprint(task.dir)\nprint(task.file)\nprint(task.line, \
         task.name, task.heading, heading())\nprint(task.targets, task.selected, task.args)\n",
    )?;
    let (result, printed) = run(task, &FakeExecutor::new())?;
    result?;
    let dir = fixture.path().display().to_string();
    assert_eq!(printed, [
        format!("Build it: {dir}"),
        format!("Build it: {dir}/kizz.md"),
        "Build it: 3 None Build it Build it".to_owned(),
        "Build it: [\"ci\", \"check\"] [\"ci\"] []".to_owned(),
    ]);

    let task = fixture.task("```starlark ci\nprint(task.name, task.heading)\n```\n")?;
    let (result, printed) = run(task, &FakeExecutor::new())?;
    result?;
    assert_eq!(printed, ["kizz.md:1: None None"]);
    Ok(())
}

/// `task.name` is the name that `name=` gives, in the block and in module
/// functions, and `print` output is prefixed with the name, not the heading; a
/// named task without a heading has the heading `None`.
#[test]
fn task_names_come_from_the_name_option() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("lib/info.star", "def name():\n    return task.name\n")?;
    let task = fixture.task(
        "# Lint everything\n\n```starlark ci name=lint cache=never\nload(\"lib/info.star\", \
         \"name\")\nprint(task.name, name(), task.heading)\nprint(task.targets, task.args)\n```\n",
    )?;
    assert_eq!(task.name.as_deref(), Some("lint"));
    assert_eq!(task.heading.as_deref(), Some("Lint everything"));
    assert!(task.cache_never);
    assert_eq!(task.args, Vec::<String>::new());
    let (result, printed) = run(task, &FakeExecutor::new())?;
    result?;
    assert_eq!(printed, [
        "lint: lint lint Lint everything",
        "lint: [\"ci\"] []",
    ]);

    let task = fixture.task("```starlark name=deploy\nprint(task.name, task.heading)\n```\n")?;
    let (result, printed) = run(task, &FakeExecutor::new())?;
    result?;
    assert_eq!(printed, ["deploy: deploy None"]);
    Ok(())
}

/// `task.args` is a list of strings that a program can read like any list, and
/// is empty under a build, which runs a task with no arguments.
#[test]
fn task_args_are_an_empty_list_under_a_build() -> TestResult {
    let fixture = Fixture::new()?;
    let task =
        fixture.block("print(type(task.args), len(task.args))\nrun(\"tool\", *task.args)\n")?;
    assert_eq!(task.args, Vec::<String>::new());
    let executor = FakeExecutor::new();
    let (result, printed) = run(task, &executor)?;
    result?;
    assert_eq!(printed, ["Build it: list 0"]);
    let argv: Vec<Vec<String>> = executor.steps().into_iter().map(|step| step.argv).collect();
    assert_eq!(argv, [vec!["tool".to_owned()]]);
    Ok(())
}

/// Task options and arguments are not part of the task program, so adding `name=`
/// or `cache=never` to a block, or giving it arguments, keeps its program hash.
#[test]
fn task_options_and_arguments_keep_the_program_hash() -> TestResult {
    let fixture = Fixture::new()?;
    let source = "run(\"tool\")\n";
    let plain = hash(fixture.task(&format!("# T\n\n```starlark ci\n{source}```\n")))?;
    for info in [
        "ci name=tool",
        "ci cache=never",
        "name=tool cache=never",
        "cache=never ci name=x",
    ] {
        let options = hash(fixture.task(&format!("# T\n\n```starlark {info}\n{source}```\n")))?;
        assert_eq!(options, plain, "{info}");
    }
    let mut task = fixture.task(&format!("# T\n\n```starlark ci\n{source}```\n"))?;
    task.args = vec!["a".to_owned(), "b".to_owned()];
    assert_eq!(hash(Ok(task))?, plain);
    Ok(())
}

/// `load` takes paths relative to the loading file, absolute paths, and nested
/// loads, and every module read is recorded as an access of the task.
#[test]
fn relative_absolute_and_nested_loads() -> TestResult {
    let fixture = Fixture::new()?;
    let outside = tempfile::tempdir()?;
    let absolute = outside.path().join("abs.star");
    fs::write(&absolute, "greeting = \"hello\"\n")?;
    let a = fixture.write(
        "lib/a.star",
        "load(\"../shared/b.star\", \"b\")\nload(\"c.star\", \"c\")\na = b + c\n",
    )?;
    fixture.write("shared/b.star", "b = 40\n")?;
    let c = fixture.write("lib/c.star", "c = 2\n")?;
    let task = fixture.block(&format!(
        "load(\"lib/a.star\", \"a\")\nload(\"{}\", \"greeting\")\nprint(greeting, a)\n",
        absolute.display()
    ))?;
    let printed = Printed::default();
    let load = TaskProgram::load(task, &printed);
    load.program?
        .run(&BTreeMap::new(), &FakeExecutor::new(), &printed)
        .result?;
    assert_eq!(printed.lines(), ["Build it: hello 42"]);
    let read: Vec<(PathBuf, String)> = load
        .accesses
        .iter()
        .map(|(path, kinds)| (path.as_path().to_path_buf(), kinds.to_string()))
        .collect();
    let lib = fixture.path().join("lib");
    let mut expected = vec![
        (a, AccessKind::Read.name().to_owned()),
        (
            lib.join("../shared/b.star"),
            AccessKind::Read.name().to_owned(),
        ),
        (c, AccessKind::Read.name().to_owned()),
        (absolute, AccessKind::Read.name().to_owned()),
    ];
    expected.sort();
    assert_eq!(read, expected);
    Ok(())
}

/// A module that is missing fails the task, and its path is recorded as a probe;
/// a module that loads itself fails the task too.
#[test]
fn missing_and_cyclic_modules_fail_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    let task = fixture.block("load(\"lib/missing.star\", \"x\")\n")?;
    let load = TaskProgram::load(task, &Printed::default());
    let Err(failure) = load.program else {
        panic!("a missing module must fail the task");
    };
    assert!(
        failure.to_string().contains("lib/missing.star"),
        "{failure}"
    );
    let missing = AbsolutePath::new(fixture.path().join("lib/missing.star"))?;
    assert_eq!(
        load.accesses.get(&missing).map(|kinds| kinds.to_string()),
        Some(AccessKind::Probe.name().to_owned())
    );

    fixture.write("one.star", "load(\"two.star\", \"two\")\none = 1\n")?;
    fixture.write("two.star", "load(\"one.star\", \"one\")\ntwo = 2\n")?;
    let task = fixture.block("load(\"one.star\", \"one\")\n")?;
    let Err(failure) = TaskProgram::load(task, &Printed::default()).program else {
        panic!("a load cycle must fail the task");
    };
    assert!(failure.to_string().contains("loads itself"), "{failure}");
    Ok(())
}

/// The same sources always give the same program hash, and editing the block, a
/// loaded module, or a module that module loads changes it.
#[test]
fn the_program_hash_covers_the_block_and_every_loaded_module() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("lib/a.star", "load(\"b.star\", \"b\")\na = b\n")?;
    fixture.write("lib/b.star", "b = 1\n")?;
    let source = "load(\"lib/a.star\", \"a\")\nprint(a)\n";

    let first = hash(fixture.block(source))?;
    assert_eq!(hash(fixture.block(source))?, first);

    assert_eq!(
        hash(Fixture::new()?.block("print(1)\n"))?,
        hash(Fixture::new()?.block("print(1)\n"))?,
        "a block without modules hashes the same in any directory"
    );

    assert_ne!(
        hash(fixture.block("load(\"lib/a.star\", \"a\")\nprint(a, a)\n"))?,
        first
    );

    fixture.write("lib/a.star", "load(\"b.star\", \"b\")\na = b + 0\n")?;
    let edited_module = hash(fixture.block(source))?;
    assert_ne!(edited_module, first);

    fixture.write("lib/b.star", "b = 2\n")?;
    let edited_nested = hash(fixture.block(source))?;
    assert_ne!(edited_nested, edited_module);
    assert_ne!(edited_nested, first);

    assert_eq!(first.to_string().len(), 64);
    assert_eq!(first.as_bytes().len(), 32);
    Ok(())
}

/// Calling `run()` while a module loads is an error, and no step runs; functions
/// from modules that call `run()` act on the task that calls them.
#[test]
fn run_while_a_module_loads_is_an_error() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("lib/eager.star", "run(\"too\", \"early\")\n")?;
    let executor = FakeExecutor::new();
    let task = fixture.block("load(\"lib/eager.star\", \"x\")\n")?;
    let Err(failure) = TaskProgram::load(task, &Printed::default()).program else {
        panic!("run() while a module loads must fail the task");
    };
    let message = failure.to_string();
    assert!(
        message.contains("run() cannot be called while the module")
            && message.contains("lib/eager.star is being loaded, outside any task"),
        "{message}"
    );
    assert_eq!(executor.steps(), Vec::<StepSpec>::new());

    fixture.write(
        "lib/build.star",
        "def build(target):\n    return run(\"make\", target, cwd = \"out\", expect = [0])\n",
    )?;
    let task =
        fixture.block("load(\"lib/build.star\", \"build\")\nprint(build(\"all\").stdout)\n")?;
    let (result, printed) = run(task, &executor)?;
    result?;
    assert_eq!(printed, ["Build it: make all"]);
    let steps = executor.steps();
    assert_eq!(steps.len(), 1);
    assert_eq!(
        steps[0].cwd,
        AbsolutePath::new(fixture.path().join("out"))?,
        "cwd resolves against the task directory, not the module's"
    );
    Ok(())
}

/// An executor whose steps end by their program: `fails` exits with code 3,
/// `killed` is killed by signal 9, `cached` exits with code 2 from the cache, and
/// every other program exits with code 0. Each step writes `out` to standard
/// output and `err` to standard error.
fn exits() -> FakeExecutor {
    FakeExecutor::scripted(|step| {
        let program = step.argv.first().map_or("", String::as_str);
        let termination = match program {
            "fails" => Termination::Exited(3),
            "killed" => Termination::Signaled(9),
            "cached" => Termination::Exited(2),
            _ => Termination::Exited(0),
        };
        let mut outcome = outcome(termination, step.capture, b"out\n", b"err\n");
        outcome.cached = program == "cached";
        Ok(outcome)
    })
}

/// Load and run the program of `task`, which must load, returning the whole run
/// and the printed lines.
fn run_whole(
    task: TaskInfo,
    executor: &FakeExecutor,
) -> TestResult<(TaskRun, Vec<String>)> {
    let printed = Printed::default();
    let program = TaskProgram::load(task, &printed).program?;
    let run = program.run(&BTreeMap::new(), executor, &printed);
    Ok((run, printed.lines()))
}

/// The position, command, and termination of each unchecked step of `run`.
fn unchecked(run: &TaskRun) -> Vec<(u32, String, Termination)> {
    run.unchecked
        .iter()
        .map(|exit| (exit.position, exit.command(), exit.termination))
        .collect()
}

/// A non-zero exit whose result the program ignores is unchecked, and so is a
/// command killed by a signal; a zero exit never is. Each unchecked step keeps its
/// position, command, termination, and raw output, and the run still succeeds.
#[test]
fn ignored_non_zero_exits_and_signals_are_unchecked() -> TestResult {
    let fixture = Fixture::new()?;
    let task = fixture.block(
        "a = run(\"fails\", \"one\")\nb = run(\"killed\", capture = \"merged\")\nc = \
         run(\"ok\")\n",
    )?;
    let (run, _) = run_whole(task, &exits())?;
    run.result?;
    assert_eq!(run.unchecked, [
        UncheckedExit {
            position: 0,
            argv: vec!["fails".to_owned(), "one".to_owned()],
            termination: Termination::Exited(3),
            output: CapturedOutput::Split {
                stdout: b"out\n".to_vec(),
                stderr: b"err\n".to_vec(),
            },
            cached: false,
        },
        UncheckedExit {
            position: 1,
            argv: vec!["killed".to_owned()],
            termination: Termination::Signaled(9),
            output: CapturedOutput::Merged {
                output: b"out\nerr\n".to_vec(),
            },
            cached: false,
        },
    ]);
    Ok(())
}

/// Reading `rc`, `signal`, `getattr(result, "rc")` or `getattr(result, "signal")`
/// checks a result; reading only `stdout`, `stderr`, `output` or `cached`, asking
/// `hasattr`, listing `dir()`, or printing the result does not.
#[test]
fn reading_rc_or_signal_checks_and_reading_output_does_not() -> TestResult {
    let fixture = Fixture::new()?;
    let task = fixture.block(
        "a = run(\"fails\", \"rc\")\nb = run(\"killed\", \"signal\")\nc = run(\"fails\", \
         \"getattr\")\nd = run(\"killed\", \"getattr-signal\")\nread = [a.rc, b.signal, \
         getattr(c, \"rc\"), getattr(d, \"signal\")]\nprint(read)\ne = run(\"fails\", \
         \"stdout\")\nprint(e.stdout.strip(), e.stderr.strip(), e.cached, hasattr(e, \"rc\"), \
         hasattr(e, \"signal\"), dir(e))\nprint(str(e).startswith(\"run_result(\"))\nf = \
         run(\"fails\", \"merged\", capture = \"merged\")\nprint(f.output.strip())\n",
    )?;
    let (run, printed) = run_whole(task, &exits())?;
    run.result.clone()?;
    assert_eq!(printed, [
        "Build it: [3, 9, 3, 9]",
        "Build it: out err False True True [\"cached\", \"rc\", \"signal\", \"stderr\", \
         \"stdout\"]",
        "Build it: True",
        "Build it: out",
        "Build it: err",
    ]);
    assert_eq!(unchecked(&run), [
        (4, "fails stdout".to_owned(), Termination::Exited(3)),
        (5, "fails merged".to_owned(), Termination::Exited(3)),
    ]);
    Ok(())
}

/// With `expect = [0, 1]`, nothing is unchecked, whatever the exit: an accepted
/// code returns, and any other code or a signal fails the task as an unexpected
/// exit, never as an unchecked one.
#[test]
fn expect_leaves_nothing_unchecked() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = FakeExecutor::scripted(|step| {
        let termination = match first_argument(step) {
            "one" => Termination::Exited(1),
            "three" => Termination::Exited(3),
            "killed" => Termination::Signaled(15),
            _ => Termination::Exited(0),
        };
        Ok(outcome(termination, step.capture, b"", b""))
    });
    let accepted = fixture.block(
        "run(\"tool\", \"zero\", expect = [0, 1])\nrun(\"tool\", \"one\", expect = [0, 1])\n",
    )?;
    let (run, _) = run_whole(accepted, &executor)?;
    run.result?;
    assert!(run.unchecked.is_empty(), "{:?}", run.unchecked);
    for argument in ["three", "killed"] {
        let task = fixture.block(&format!("run(\"tool\", \"{argument}\", expect = [0, 1])\n"))?;
        let (run, _) = run_whole(task, &executor)?;
        assert!(
            matches!(run.result, Err(TaskFailure::UnexpectedExit { .. })),
            "{argument}: {:?}",
            run.result
        );
        assert!(run.unchecked.is_empty(), "{argument}: {:?}", run.unchecked);
    }
    Ok(())
}

/// A module function that returns a result to its caller, which reads `rc`,
/// checks it; the same function whose caller reads only the output leaves it
/// unchecked.
#[test]
fn a_result_returned_by_a_module_function_is_checked_where_it_is_read() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write(
        "lib/tools.star",
        "def tool(name):\n    return run(\"fails\", name)\n",
    )?;
    let task = fixture.block(
        "load(\"lib/tools.star\", \"tool\")\nif tool(\"checked\").rc != 3:\n    fail(\"rc\")\nprint(tool(\"ignored\").stdout.strip())\n",
    )?;
    let (run, printed) = run_whole(task, &exits())?;
    run.result.clone()?;
    assert_eq!(printed, ["Build it: out"]);
    assert_eq!(unchecked(&run), [(
        1,
        "fails ignored".to_owned(),
        Termination::Exited(3)
    )]);
    Ok(())
}

/// A result from the cache counts like any other, and a program that fails still
/// reports what it left unchecked before it failed.
#[test]
fn cached_results_and_failed_programs_report_unchecked_steps() -> TestResult {
    let fixture = Fixture::new()?;
    let task = fixture.block("a = run(\"cached\")\nfail(\"stop\")\n")?;
    let (run, _) = run_whole(task, &exits())?;
    let message = program_message(run.result.clone())?;
    assert!(message.contains("stop"), "{message}");
    assert_eq!(run.unchecked.len(), 1, "{:?}", run.unchecked);
    let exit = &run.unchecked[0];
    assert_eq!(exit.termination, Termination::Exited(2));
    assert!(exit.cached);
    Ok(())
}

/// An unchecked step and the failure of `kizz build --fail-unchecked` show each
/// command, its exit status, and its captured output, as an unexpected exit does;
/// the failure's first line names the command that was given `--fail-unchecked`,
/// `kizz build` or `kizz run`, and only its first line depends on it.
#[test]
fn unchecked_failures_show_each_command_status_and_output() {
    let exits = vec![
        UncheckedExit {
            position: 0,
            argv: vec!["false".to_owned()],
            termination: Termination::Exited(1),
            output: CapturedOutput::Split {
                stdout: b"out".to_vec(),
                stderr: Vec::new(),
            },
            cached: false,
        },
        UncheckedExit {
            position: 2,
            argv: vec!["sleep".to_owned(), "9".to_owned()],
            termination: Termination::Signaled(9),
            output: CapturedOutput::Merged {
                output: b"partial\n".to_vec(),
            },
            cached: true,
        },
    ];
    let steps = "command `false`: exit code 1, which the task program never checked: its `run()` \
                 has no `expect`, and the program never read the result's `rc` or \
                 `signal`\n--- stdout ---\nout\n--- stderr ---\ncommand `sleep 9`: killed by \
                 signal 9, which the task program never checked: its `run()` has no `expect`, \
                 and the program never read the result's `rc` or `signal`\n--- output \
                 ---\npartial\n";
    let shown = TaskFailure::Unchecked {
        exits: exits.clone(),
        invoked: "kizz build",
    }
    .to_string();
    assert_eq!(
        shown,
        format!(
            "the task program left 2 non-zero exits unchecked, and `kizz build --fail-unchecked` \
             makes that fail the task\n{steps}"
        )
    );
    let shown = TaskFailure::Unchecked {
        exits,
        invoked: "kizz run",
    }
    .to_string();
    assert_eq!(
        shown,
        format!(
            "the task program left 2 non-zero exits unchecked, and `kizz run --fail-unchecked` \
             makes that fail the task\n{steps}"
        )
    );
}

/// An executor for the encoding tests, by the step's first argument: `one` exits
/// with code 1 and writes `out` to standard output, `killed` is killed by signal 9
/// after writing `out` and `err`, and `hit` exits with code 0 from the cache after
/// writing `hit` and `warn`.
fn encoded() -> FakeExecutor {
    FakeExecutor::scripted(|step| {
        let (termination, stdout, stderr): (Termination, &[u8], &[u8]) = match first_argument(step)
        {
            "one" => (Termination::Exited(1), b"out\n", b""),
            "killed" => (Termination::Signaled(9), b"out\n", b"err\n"),
            _ => (Termination::Exited(0), b"hit\n", b"warn\n"),
        };
        let mut outcome = outcome(termination, step.capture, stdout, stderr);
        outcome.cached = first_argument(step) == "hit";
        Ok(outcome)
    })
}

/// The encoding rule's first regression: `json.encode` of a result is exactly the
/// text that the struct `run()` returned before encoded to for the same values,
/// an object of `rc`, `signal`, `stdout` and `stderr` or `output`, and `cached`, in
/// that order, for both capture modes, an exit code and a signal, and a run and a
/// cache hit; `fs.write_json` writes the same members with the same values.
#[test]
fn json_encode_gives_the_text_of_the_former_struct() -> TestResult {
    let fixture = Fixture::new()?;
    let task = fixture.block(
        "a = run(\"tool\", \"one\")\nb = run(\"tool\", \"killed\", capture = \"merged\")\nc = \
         run(\"tool\", \"hit\")\nd = run(\"tool\", \"killed\")\nfor r in [a, b, c, d]:\n    \
         print(json.encode(r))\nfs.write_json(\"a.json\", a)\nfs.write_json(\"b.json\", \
         b)\nfs.write_json(\"c.json\", c)\n",
    )?;
    let (run, printed) = run_whole(task, &encoded())?;
    run.result.clone()?;
    assert_eq!(printed, [
        r#"Build it: {"rc":1,"signal":null,"stdout":"out\n","stderr":"","cached":false}"#,
        r#"Build it: {"rc":null,"signal":9,"output":"out\nerr\n","cached":false}"#,
        r#"Build it: {"rc":0,"signal":null,"stdout":"hit\n","stderr":"warn\n","cached":true}"#,
        r#"Build it: {"rc":null,"signal":9,"stdout":"out\n","stderr":"err\n","cached":false}"#,
    ]);
    let written = |name: &str| -> TestResult<serde_json::Value> {
        Ok(serde_json::from_str(&fs::read_to_string(
            fixture.path().join(name),
        )?)?)
    };
    assert_eq!(
        written("a.json")?,
        serde_json::json!({
            "rc": 1,
            "signal": null,
            "stdout": "out\n",
            "stderr": "",
            "cached": false
        })
    );
    assert_eq!(
        written("b.json")?,
        serde_json::json!({
            "rc": null,
            "signal": 9,
            "output": "out\nerr\n",
            "cached": false
        })
    );
    assert_eq!(
        written("c.json")?,
        serde_json::json!({
            "rc": 0,
            "signal": null,
            "stdout": "hit\n",
            "stderr": "warn\n",
            "cached": true
        })
    );
    assert!(run.unchecked.is_empty(), "{:?}", run.unchecked);
    Ok(())
}

/// The encoding rule's second regression: a non-zero exit, and a signal, whose
/// result the program only encodes, with `json.encode`, also inside a list or a
/// dict, or with `fs.write_json`, also inside a dict, is checked, while the same
/// program without the encoding leaves it unchecked.
#[test]
fn encoding_a_result_checks_it() -> TestResult {
    let fixture = Fixture::new()?;
    let encodings = [
        "json.encode(r)",
        "json.encode([r])",
        "json.encode({\"result\": r})",
        "fs.write_json(\"r.json\", r)",
        "fs.write_json(\"r.json\", {\"all\": [r]})",
    ];
    for program in ["fails", "killed"] {
        for encoding in encodings {
            let task = fixture.block(&format!("r = run(\"{program}\")\n{encoding}\n"))?;
            let (run, _) = run_whole(task, &exits())?;
            run.result?;
            assert!(
                run.unchecked.is_empty(),
                "{program}, {encoding}: {:?}",
                run.unchecked
            );
        }
        let task = fixture.block(&format!(
            "r = run(\"{program}\")\nkept = [r]\nprint(len(kept), r.stdout.strip())\n"
        ))?;
        let (run, printed) = run_whole(task, &exits())?;
        run.result?;
        assert_eq!(printed, ["Build it: 1 out"]);
        assert_eq!(run.unchecked.len(), 1, "{program}: {:?}", run.unchecked);
    }
    Ok(())
}

/// The commands of the steps `executor` received, in order.
fn commands(executor: &FakeExecutor) -> Vec<Vec<String>> {
    executor.steps().into_iter().map(|step| step.argv).collect()
}

/// `words` as an owned command.
fn command(words: &[&str]) -> Vec<String> {
    words.iter().map(|&word| word.to_owned()).collect()
}

/// Lists and tuples among `run()`'s positional arguments expand in place, first,
/// last, in the middle, nested, as the whole command, held in a variable, and as
/// frozen values loaded from a module; empty ones add nothing, each string stays
/// one argument, and the same list used twice adds its strings twice.
#[test]
fn lists_and_tuples_in_run_expand_into_the_command() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write(
        "lib/flags.star",
        "FLAGS = [\"--locked\", (\"--jobs\", \"2\")]\nEMPTY = ()\n",
    )?;
    let task = fixture.block(
        "load(\"lib/flags.star\", \"FLAGS\", \"EMPTY\")\nrun(\"echo\", [\"this\", \"is\", \
         \"test\"])\nrun([\"echo\", \"this\"], \"is\", \"test\")\nrun(\"echo\", [\"this\"], \
         \"is\", (\"test\",))\nrun(\"echo\", [[\"this\"], (\"is\", [])], \"test\")\nrun([\"echo\", \
         \"this\", \"is\", \"test\"])\nrun((\"printf\",), [], (), [[]], \"a b\", \"\")\nverbose = \
         []\nrun(\"tool\", verbose, \"x\")\nverbose = [\"-v\"]\nrun(\"tool\", verbose, \"x\", \
         verbose)\nrun(\"cargo\", \"build\", FLAGS, EMPTY)\n",
    )?;
    let executor = FakeExecutor::new();
    let (result, _) = run(task, &executor)?;
    result?;
    let this_is_test = command(&["echo", "this", "is", "test"]);
    assert_eq!(commands(&executor), [
        this_is_test.clone(),
        this_is_test.clone(),
        this_is_test.clone(),
        this_is_test.clone(),
        this_is_test,
        command(&["printf", "a b", ""]),
        command(&["tool", "x"]),
        command(&["tool", "-v", "x", "-v"]),
        command(&["cargo", "build", "--locked", "--jobs", "2"]),
    ]);
    Ok(())
}

/// `run("cargo", "test", task.args)` passes the task's arguments through, each as
/// one argument, and adds nothing when `task.args` is empty, as under a build.
#[test]
fn task_args_pass_through_as_one_list() -> TestResult {
    let fixture = Fixture::new()?;
    let source = "run(\"cargo\", \"test\", task.args)\n";
    let mut task = fixture.block(source)?;
    task.args = vec!["--release".to_owned(), "a b".to_owned()];
    let executor = FakeExecutor::new();
    let (result, _) = run(task, &executor)?;
    result?;
    assert_eq!(commands(&executor), [command(&[
        "cargo",
        "test",
        "--release",
        "a b"
    ])]);

    let executor = FakeExecutor::new();
    let (result, _) = run(fixture.block(source)?, &executor)?;
    result?;
    assert_eq!(commands(&executor), [command(&["cargo", "test"])]);
    Ok(())
}

/// Each error of the expansion fails the task with its exact message, and the
/// failing call starts no step: a wrong type at the top and nested, for `int`,
/// `bool`, `NoneType`, `dict`, `range` and `run_result`, a list or tuple that
/// contains itself, and more than a million values; a call whose arguments hold no
/// string at all fails as a call without a program.
#[test]
fn invalid_run_arguments_fail_the_task_and_start_no_step() -> TestResult {
    let fixture = Fixture::new()?;
    let no_program = "run() needs a program: run(\"program\", \"argument\", ...)";
    for (source, message) in [
        (
            "run(\"p\", \"a\", 5)\n",
            "run(): argument 3 is of type int, not a string, list or tuple",
        ),
        (
            "run(\"p\", [True])\n",
            "run(): argument 2 at [0] is of type bool, not a string, list or tuple",
        ),
        (
            "run(\"p\", [\"a\", [None]])\n",
            "run(): argument 2 at [1][0] is of type NoneType, not a string, list or tuple",
        ),
        (
            "run({\"p\": 1})\n",
            "run(): argument 1 is of type dict, not a string, list or tuple",
        ),
        (
            "run(\"p\", (\"a\", range(3)))\n",
            "run(): argument 2 at [1] is of type range, not a string, list or tuple",
        ),
        (
            "run([], 5)\n",
            "run(): argument 2 is of type int, not a string, list or tuple",
        ),
        (
            "a = [\"x\"]\na.append(a)\nrun(\"p\", a)\n",
            "run(): argument 2 at [1] is a list that contains itself",
        ),
        (
            "a = []\nb = [a]\na.append(b)\nrun(\"p\", a)\n",
            "run(): argument 2 at [0][0] is a list that contains itself",
        ),
        (
            "l = []\nt = (\"a\", l)\nl.append(t)\nrun(\"p\", t)\n",
            "run(): argument 2 at [1][0] is a tuple that contains itself",
        ),
        (
            "a = [\"x\"]\nfor _ in range(21):\n    a = [a, a]\nrun(\"p\", a)\n",
            "run(): the arguments expand to more than 1000000 strings, lists and tuples",
        ),
        ("run([])\n", no_program),
        ("run([], ())\n", no_program),
        ("run([[]])\n", no_program),
    ] {
        let executor = FakeExecutor::new();
        let message_seen = program_message(run(fixture.block(source)?, &executor)?.0)?;
        assert!(
            message_seen.contains(message),
            "{message:?} in {message_seen}"
        );
        assert_eq!(executor.steps(), [], "{source}");
    }

    let executor = FakeExecutor::new();
    let task = fixture.block("r = run(\"first\")\nrun(\"p\", [r])\n")?;
    let message = program_message(run(task, &executor)?.0)?;
    assert!(
        message.contains(
            "run(): argument 2 at [0] is of type run_result, not a string, list or tuple"
        ),
        "{message}"
    );
    assert_eq!(commands(&executor), [command(&["first"])]);
    Ok(())
}

/// `run("echo", ["this", "is", "test"])`, `run(["echo", "this"], "is", "test")`,
/// `run(("echo",), [["this"], ("is", [])], "test")` and
/// `run("echo", "this", "is", "test")` give equal step specifications, so one
/// program hash and occurrence give them one step key.
#[test]
fn equivalent_forms_give_one_step_specification_and_key() -> TestResult {
    let fixture = Fixture::new()?;
    let mut specs = Vec::new();
    for source in [
        "run(\"echo\", [\"this\", \"is\", \"test\"])\n",
        "run([\"echo\", \"this\"], \"is\", \"test\")\n",
        "run((\"echo\",), [[\"this\"], (\"is\", [])], \"test\")\n",
        "run(\"echo\", \"this\", \"is\", \"test\")\n",
    ] {
        let executor = FakeExecutor::new();
        let (result, _) = run(fixture.block(source)?, &executor)?;
        result?;
        let [spec] = <[StepSpec; 1]>::try_from(executor.steps())
            .map_err(|steps| format!("one step, not {steps:?}"))?;
        specs.push(spec);
    }
    let program = hash(fixture.block("run(\"echo\", \"this\", \"is\", \"test\")\n"))?;
    let first = specs.first().ok_or("no specification")?;
    assert_eq!(first.argv, command(&["echo", "this", "is", "test"]));
    let key = kizz_core::step_key(program, first, 0)?;
    for spec in &specs {
        assert_eq!(spec, first);
        assert_eq!(kizz_core::step_key(program, spec, 0)?, key);
    }
    Ok(())
}
