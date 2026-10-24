// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Starlark runtime of task programs, with a fake step executor.
//!
//! Each test writes its `kizz.md` and modules into a temporary directory, parses
//! the task block, loads and runs its program, and checks the step specifications
//! the fake executor received, the lines the program printed, and the failure.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use kizz_core::{
    PrintSink, ProgramHash, StepError, StepExecutor, StepOutcome, StepSpec, TaskFailure, TaskInfo,
    TaskProgram, parse_tasks,
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

    /// A task named `Build it`, tagged `ci check`, whose block is `source`.
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
         ignore = [\"*.log\", \"tmp/**\"], track = [\"/proc/cpuinfo\"], expect = [0, 2])\nrun(\"prog\", \
         cwd = \"/elsewhere\")\n",
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
            ignore: vec![],
            track: vec![],
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
            ignore: vec!["*.log".to_owned(), "tmp/**".to_owned()],
            track: vec!["/proc/cpuinfo".to_owned()],
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
            ignore: vec![],
            track: vec![],
            expect: None,
        },
    ]);
    Ok(())
}

/// Invalid `run()` calls fail the task: no program, an unknown capture mode, an
/// unknown parameter, and a step the executor cannot run.
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
/// and `print` output is prefixed with the task name, one line at a time.
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

/// `task` describes the running task, in the block and in module functions.
#[test]
fn task_values_match_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("lib/info.star", "def heading():\n    return task.name\n")?;
    let task = fixture.block(
        "load(\"lib/info.star\", \"heading\")\nprint(task.dir)\nprint(task.file)\nprint(task.line, \
         task.name, heading())\nprint(task.targets, task.selected)\n",
    )?;
    let (result, printed) = run(task, &FakeExecutor::new())?;
    result?;
    let dir = fixture.path().display().to_string();
    assert_eq!(printed, [
        format!("Build it: {dir}"),
        format!("Build it: {dir}/kizz.md"),
        "Build it: 3 Build it Build it".to_owned(),
        "Build it: [\"ci\", \"check\"] [\"ci\"]".to_owned(),
    ]);

    let task = fixture.task("```starlark ci\nprint(task.name)\n```\n")?;
    let (result, printed) = run(task, &FakeExecutor::new())?;
    result?;
    assert_eq!(printed, ["kizz.md:1: None"]);
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
