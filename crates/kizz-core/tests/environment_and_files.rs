// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The task environment, dotenv files, and the `fs` functions, with a fake step
//! executor.
//!
//! Each test writes its `kizz.md`, dotenv files, and data files into a temporary
//! directory, gives the program an explicit caller environment, and checks the
//! step specifications the fake executor received, the lines the program printed,
//! the files it wrote, and the accesses recorded against the task.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use kizz_core::{
    PrintSink, StepError, StepExecutor, StepOutcome, StepSpec, TaskFailure, TaskInfo, TaskProgram,
    TaskRun, parse_tasks,
};
use kizz_trace::{AccessKind, CapturedOutput, Termination};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// An environment.
type Env = BTreeMap<String, String>;

/// The environment with the variables of `pairs`.
fn env(pairs: &[(&str, &str)]) -> Env {
    pairs
        .iter()
        .map(|&(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// A step executor that records every step and answers with exit status 0 and
/// the step's arguments after the program, joined with spaces, on standard output.
#[derive(Default)]
struct FakeExecutor {
    /// The steps received, in order.
    steps: RefCell<Vec<StepSpec>>,
}

impl FakeExecutor {
    /// The environments of the steps received, in order.
    fn envs(&self) -> Vec<Env> {
        self.steps
            .borrow()
            .iter()
            .map(|step| step.env.clone())
            .collect()
    }
}

impl StepExecutor for FakeExecutor {
    fn execute(
        &self,
        step: &StepSpec,
    ) -> Result<StepOutcome, StepError> {
        self.steps.borrow_mut().push(step.clone());
        let stdout = step.argv.get(1..).unwrap_or_default().join(" ");
        Ok(StepOutcome {
            termination: Termination::Exited(0),
            output: CapturedOutput::Split {
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            },
            cached: false,
        })
    }
}

/// Collects the lines programs print.
#[derive(Default)]
struct Printed {
    /// The lines, in order.
    lines: RefCell<Vec<String>>,
}

impl Printed {
    /// The lines printed so far, without the task name prefix.
    fn lines(&self) -> Vec<String> {
        self.lines
            .borrow()
            .iter()
            .map(|line| line.strip_prefix("Task: ").unwrap_or(line).to_owned())
            .collect()
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

/// What one task run produced.
struct Ran {
    /// The accesses and the result.
    run: TaskRun,
    /// The steps the executor received.
    executor: FakeExecutor,
    /// The lines the program printed, without the task name prefix.
    printed: Vec<String>,
}

impl Ran {
    /// The message of the program failure, or an error when the task did not fail
    /// that way.
    fn failure(&self) -> TestResult<String> {
        match &self.run.result {
            Err(TaskFailure::Program { message }) => Ok(message.clone()),
            other => Err(format!("expected a program failure, got {other:?}").into()),
        }
    }

    /// The recorded accesses, as paths relative to `root` with their kinds.
    fn accesses(
        &self,
        root: &Path,
    ) -> Vec<(String, String)> {
        self.run
            .accesses
            .iter()
            .map(|(path, kinds)| {
                let relative = path.as_path().strip_prefix(root).unwrap_or(path.as_path());
                (relative.display().to_string(), kinds.to_string())
            })
            .collect()
    }
}

/// A temporary directory whose `project` subdirectory holds one `kizz.md`, so the
/// tests can reach a path outside the project with `../`.
struct Fixture {
    /// The temporary directory.
    dir: TempDir,
}

impl Fixture {
    /// An empty fixture.
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        fs::create_dir(dir.path().join("project"))?;
        Ok(Self { dir })
    }

    /// The temporary directory.
    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The project directory, the task directory.
    fn project(&self) -> PathBuf {
        self.root().join("project")
    }

    /// Write `content` to `relative` in the project, creating parent directories.
    fn write(
        &self,
        relative: &str,
        content: &str,
    ) -> TestResult<PathBuf> {
        let path = self.project().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(path)
    }

    /// Read `relative` in the project.
    fn read(
        &self,
        relative: &str,
    ) -> TestResult<String> {
        Ok(fs::read_to_string(self.project().join(relative))?)
    }

    /// The task named `Task` whose block is `source`.
    fn task(
        &self,
        source: &str,
    ) -> TestResult<TaskInfo> {
        let markdown = format!("# Task\n\n```starlark ci\n{source}```\n");
        let file = self.write("kizz.md", &markdown)?;
        let parsed = parse_tasks(&file, Path::new("kizz.md"), &markdown);
        if !parsed.errors.is_empty() {
            return Err(format!("configuration errors: {:?}", parsed.errors).into());
        }
        let task = parsed.tasks.first().ok_or("no task in the fixture")?;
        Ok(TaskInfo::new(task, &["ci".to_owned()])?)
    }

    /// Load the block `source`, which must load, and run it with the caller
    /// environment `caller`.
    fn run(
        &self,
        source: &str,
        caller: &Env,
    ) -> TestResult<Ran> {
        let printed = Printed::default();
        let executor = FakeExecutor::default();
        let program = TaskProgram::load(self.task(source)?, &printed).program?;
        let run = program.run(caller, &executor, &printed);
        Ok(Ran {
            run,
            executor,
            printed: printed.lines(),
        })
    }
}

/// The task environment starts empty, whatever the caller environment holds.
#[test]
fn the_environment_starts_empty() -> TestResult {
    let fixture = Fixture::new()?;
    let caller = env(&[("PATH", "/bin"), ("HOME", "/home/me")]);
    let ran = fixture.run("run(\"tool\")\nprint(env.dict())\n", &caller)?;
    ran.run.result.clone()?;
    assert_eq!(ran.executor.envs(), [Env::new()]);
    assert_eq!(ran.printed, ["{}"]);
    Ok(())
}

/// `env.inherit` copies caller variables into the task environment and skips the
/// unset ones, and `env.caller` reads the caller environment without changing the
/// task environment.
#[test]
fn inherit_and_caller_read_the_caller_environment() -> TestResult {
    let fixture = Fixture::new()?;
    let caller = env(&[("PATH", "/bin"), ("HOME", "/home/me"), ("SECRET", "s")]);
    let ran = fixture.run(
        concat!(
            "print(env.caller(\"SECRET\"), env.caller(\"MISSING\"), env.caller(\"MISSING\", ",
            "\"fallback\"), env.caller(\"HOME\", default = \"x\"))\n",
            "run(\"one\")\n",
            "env.inherit(\"PATH\", \"MISSING\", \"HOME\")\n",
            "run(\"two\")\n",
            "env.inherit()\n",
            "print(env.get(\"MISSING\"))\n",
        ),
        &caller,
    )?;
    ran.run.result.clone()?;
    assert_eq!(ran.printed, ["s None fallback /home/me", "None"]);
    assert_eq!(ran.executor.envs(), [
        Env::new(),
        env(&[("HOME", "/home/me"), ("PATH", "/bin")]),
    ]);
    Ok(())
}

/// `env.read` returns a dotenv file's variables without changing the task
/// environment, and records the file as read.
#[test]
fn read_leaves_the_task_environment_unchanged() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write(
        "config/app.env",
        "# settings\nexport MODE=release\nLEVEL='3'\n",
    )?;
    let ran = fixture.run(
        "values = env.read(\"config/app.env\")\nprint(values)\nrun(\"tool\")\nprint(env.dict())\n",
        &Env::new(),
    )?;
    ran.run.result.clone()?;
    assert_eq!(ran.printed, [
        "{\"LEVEL\": \"3\", \"MODE\": \"release\"}",
        "{}"
    ]);
    assert_eq!(ran.executor.envs(), [Env::new()]);
    assert_eq!(ran.accesses(fixture.root()), [(
        "project/config/app.env".to_owned(),
        "read".to_owned()
    )]);
    Ok(())
}

/// `env.load_file` fills in `defaults` for variables the file does not set, adds the
/// result to the task environment, returns it, and records the file; with
/// `required = False` a missing file means only the defaults apply, and by default
/// a missing file fails the task.
#[test]
fn load_file_with_defaults() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write(".env", "A=from-file\nC=\"line\\nbreak\"\n")?;
    let ran = fixture.run(
        concat!(
            "env.set(\"KEEP\", \"k\")\n",
            "env.set(\"A\", \"overwritten\")\n",
            "loaded = env.load_file(\".env\", defaults = {\"A\": \"default\", \"B\": \"default\"})\n",
            "print(loaded)\n",
            "run(\"one\")\n",
            "only = env.load_file(\"missing.env\", {\"D\": \"d\"}, required = False)\n",
            "print(only)\n",
            "run(\"two\")\n",
        ),
        &Env::new(),
    )?;
    ran.run.result.clone()?;
    assert_eq!(ran.printed, [
        "{\"A\": \"from-file\", \"B\": \"default\", \"C\": \"line\\nbreak\"}",
        "{\"D\": \"d\"}",
    ]);
    let first = env(&[
        ("A", "from-file"),
        ("B", "default"),
        ("C", "line\nbreak"),
        ("KEEP", "k"),
    ]);
    let mut second = first.clone();
    second.insert("D".to_owned(), "d".to_owned());
    assert_eq!(ran.executor.envs(), [first, second]);
    assert_eq!(ran.accesses(fixture.root()), [
        ("project/.env".to_owned(), "read".to_owned()),
        ("project/missing.env".to_owned(), "probe".to_owned()),
    ]);

    let ran = fixture.run(
        "env.load_file(\"missing.env\", {\"D\": \"d\"})\n",
        &Env::new(),
    )?;
    let message = ran.failure()?;
    assert!(
        message.contains("env.load_file(): cannot read") && message.contains("project/missing.env"),
        "{message}"
    );
    assert_eq!(ran.accesses(fixture.root()), [(
        "project/missing.env".to_owned(),
        "probe".to_owned()
    )]);
    Ok(())
}

/// `env` has no attribute named `load`, so the dotenv function is reachable only
/// as `env.load_file`.
#[test]
fn env_has_no_load_attribute() -> TestResult {
    let fixture = Fixture::new()?;
    let ran = fixture.run(
        "print(hasattr(env, \"load\"), hasattr(env, \"load_file\"))\nprint(\"load\" in dir(env), \
         \"load_file\" in dir(env))\n",
        &Env::new(),
    )?;
    ran.run.result.clone()?;
    assert_eq!(ran.printed, ["False True", "False True"]);

    let ran = fixture.run("getattr(env, \"load\")\n", &Env::new())?;
    let message = ran.failure()?;
    assert!(
        message.contains("Operation `.load` not supported on type `namespace`"),
        "{message}"
    );
    Ok(())
}

/// `env.set`, `env.update`, `env.unset`, `env.get`, `env.dict`, and `env.clear`
/// work on the task environment, and each step sees the environment at its call.
#[test]
fn set_update_unset_get_dict_and_clear() -> TestResult {
    let fixture = Fixture::new()?;
    let ran = fixture.run(
        concat!(
            "env.set(\"A\", \"1\")\n",
            "run(\"one\")\n",
            "env.update({\"B\": \"2\", \"A\": \"one\"})\n",
            "run(\"two\")\n",
            "env.unset(\"A\")\n",
            "env.unset(\"NEVER_SET\")\n",
            "print(env.get(\"A\"), env.get(\"A\", \"gone\"), env.get(\"B\"))\n",
            "snapshot = env.dict()\n",
            "snapshot[\"C\"] = \"not in the task\"\n",
            "run(\"three\")\n",
            "env.clear()\n",
            "run(\"four\")\n",
            "print(env.dict(), snapshot)\n",
        ),
        &env(&[("A", "caller")]),
    )?;
    ran.run.result.clone()?;
    assert_eq!(ran.printed, [
        "None gone 2",
        "{} {\"B\": \"2\", \"C\": \"not in the task\"}"
    ]);
    assert_eq!(ran.executor.envs(), [
        env(&[("A", "1")]),
        env(&[("A", "one"), ("B", "2")]),
        env(&[("B", "2")]),
        Env::new(),
    ]);
    Ok(())
}

/// Invalid variables fail the task.
#[test]
fn invalid_variables_fail_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    for (source, part) in [
        (
            "env.set(\"\", \"x\")\n",
            "env.set(): invalid variable \"\": the name is empty",
        ),
        ("env.set(\"A=B\", \"x\")\n", "the name contains ="),
        ("env.update({\"A\": \"x\\0y\"})\n", "NUL"),
        (
            "env.set(\"A\", 1)\n",
            "Type of parameter `value` doesn't match",
        ),
        (
            "env.load_file(\"x.env\", {\"=\": \"x\"}, required = False)\n",
            "the name contains =",
        ),
    ] {
        let message = fixture.run(source, &Env::new())?.failure()?;
        assert!(message.contains(part), "{part:?} in {message}");
    }
    Ok(())
}

/// Captured output passed through `env.set` reaches the next step's environment.
#[test]
fn captured_output_through_env_set_reaches_the_next_step() -> TestResult {
    let fixture = Fixture::new()?;
    let ran = fixture.run(
        concat!(
            "version = run(\"describe\", \"v1.2.3\").stdout\n",
            "env.set(\"VERSION\", version)\n",
            "run(\"build\")\n",
        ),
        &Env::new(),
    )?;
    ran.run.result.clone()?;
    assert_eq!(ran.executor.envs(), [
        Env::new(),
        env(&[("VERSION", "v1.2.3")])
    ]);
    Ok(())
}

/// A dotenv syntax error fails the task with the file and the line (PI6), and
/// nothing is added to the task environment.
#[test]
fn dotenv_syntax_errors_fail_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    let file = fixture.write("bad.env", "GOOD=1\n# fine\n\nnot an assignment\n")?;
    for source in [
        "env.load_file(\"bad.env\")\n",
        "env.load_file(\"bad.env\", required = False)\n",
        "env.read(\"bad.env\")\n",
    ] {
        let ran = fixture.run(&format!("{source}run(\"not reached\")\n"), &Env::new())?;
        let message = ran.failure()?;
        let location = format!("{}:4: expected NAME=value", file.display());
        assert!(message.contains(&location), "{location:?} in {message}");
        assert!(message.contains("Traceback"), "{message}");
        assert_eq!(ran.executor.envs(), Vec::<Env>::new());
    }
    fixture.write("escape.env", "A=\"\\q\"\n")?;
    let message = fixture
        .run("env.load_file(\"escape.env\")\n", &Env::new())?
        .failure()?;
    assert!(
        message.contains("escape.env:1: `\\q` is not a supported escape"),
        "{message}"
    );
    Ok(())
}

/// While a module loads, outside any task, the `env` functions that need the task
/// or caller environment are errors, while `env.read` and the `fs` functions work
/// and their accesses are recorded against the task.
#[test]
fn env_and_fs_while_a_module_loads() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("lib/eager.star", "env.set(\"A\", \"1\")\n")?;
    let task = fixture.task("load(\"lib/eager.star\", \"x\")\n")?;
    let Err(failure) = TaskProgram::load(task, &Printed::default()).program else {
        return Err("env.set() while a module loads must fail the task".into());
    };
    let message = failure.to_string();
    assert!(
        message.contains("env.set() cannot be called while the module")
            && message.contains("lib/eager.star is being loaded, outside any task"),
        "{message}"
    );

    fixture.write("data.txt", "hello")?;
    fixture.write("vars.env", "V=1\n")?;
    fixture.write(
        "lib/data.star",
        "text = fs.read(\"data.txt\")\nvars = env.read(\"vars.env\")\nhere = \
         fs.exists(\"nothing\")\n\ndef set_from_module():\n    env.set(\"FROM\", text)\n",
    )?;
    let task = fixture.task(
        "load(\"lib/data.star\", \"text\", \"vars\", \"here\", \
         \"set_from_module\")\nset_from_module()\nprint(text, vars, here)\nrun(\"tool\")\n",
    )?;
    let printed = Printed::default();
    let executor = FakeExecutor::default();
    let load = TaskProgram::load(task, &printed);
    let loaded: Vec<(PathBuf, String)> = load
        .accesses
        .iter()
        .map(|(path, kinds)| (path.as_path().to_path_buf(), kinds.to_string()))
        .collect();
    assert_eq!(loaded, [
        (fixture.project().join("data.txt"), "read".to_owned()),
        (fixture.project().join("lib/data.star"), "read".to_owned()),
        (fixture.project().join("nothing"), "probe".to_owned()),
        (fixture.project().join("vars.env"), "read".to_owned()),
    ]);
    load.program?.run(&Env::new(), &executor, &printed).result?;
    assert_eq!(printed.lines(), ["hello {\"V\": \"1\"} False"]);
    assert_eq!(executor.envs(), [env(&[("FROM", "hello")])]);
    Ok(())
}

/// JSON written with `fs.write_json` reads back equal with `fs.read_json`, in a
/// stable form: sorted keys, two-space indentation by default, and a trailing
/// newline.
#[test]
fn json_round_trips() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write(
        "input.json",
        "{\"z\": [1, 2.5, -3, 18446744073709551615], \"a\": {\"t\": true, \"n\": null}, \"s\": \
         \"\\u00e9\\n\"}",
    )?;
    let ran = fixture.run(
        "value = {\"name\": \"kizz\", \"list\": [1, -2, 3.5, True, None, \"x\"], \"nested\": \
         {\"b\": {}, \"a\": []}, \"big\": 9007199254740993}\nfs.write_json(\"out/value.json\", \
         value)\nback = fs.read_json(\"out/value.json\")\nprint(back == value)\ndata = \
         fs.read_json(\"input.json\")\nprint(data[\"z\"], data[\"a\"][\"n\"], data[\"s\"] == \"\\u00e9\\n\")\n\
         fs.write_json(\"copy.json\", data, indent = 4)\nprint(fs.read_json(\"copy.json\") == data)\n",
        &Env::new(),
    )?;
    ran.run.result.clone()?;
    assert_eq!(ran.printed, [
        "True",
        "[1, 2.5, -3, 18446744073709551615] None True",
        "True"
    ]);
    assert_eq!(
        fixture.read("out/value.json")?,
        "{\n  \"big\": 9007199254740993,\n  \"list\": [\n    1,\n    -2,\n    3.5,\n    \
         true,\n    null,\n    \"x\"\n  ],\n  \"name\": \"kizz\",\n  \"nested\": {\n    \"a\": [],\n    \
         \"b\": {}\n  }\n}\n"
    );
    assert_eq!(
        fixture.read("copy.json")?,
        "{\n    \"a\": {\n        \"n\": null,\n        \"t\": true\n    },\n    \"s\": \"\u{e9}\\n\",\n    \
         \"z\": [\n        1,\n        2.5,\n        -3,\n        18446744073709551615\n    \
         ]\n}\n"
    );
    Ok(())
}

/// Values that have no JSON form, a negative indent, and invalid JSON fail the
/// task.
#[test]
fn invalid_json_fails_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("broken.json", "{\"a\": ")?;
    for (source, part) in [
        (
            "fs.write_json(\"f.json\", {\"f\": len})\n",
            "fs.write_json():",
        ),
        (
            "cycle = []\ncycle.append(cycle)\nfs.write_json(\"f.json\", cycle)\n",
            "fs.write_json():",
        ),
        (
            "fs.write_json(\"f.json\", [], indent = -1)\n",
            "indent must be 0 or more, not -1",
        ),
        (
            "fs.read_json(\"broken.json\")\n",
            "does not hold valid JSON",
        ),
    ] {
        let message = fixture.run(source, &Env::new())?.failure()?;
        assert!(message.contains(part), "{part:?} in {message}");
    }
    assert!(!fixture.project().join("f.json").exists());
    Ok(())
}

/// Set the modification time of `path` to a fixed time in the past and return it.
fn age(path: &Path) -> TestResult<SystemTime> {
    let old = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(1_000_000_000))
        .ok_or("time overflow")?;
    fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(old)?;
    Ok(fs::metadata(path)?.modified()?)
}

/// A write with unchanged content leaves the file untouched, for `fs.write` and
/// `fs.write_json`; a write with new content replaces it.
#[test]
fn unchanged_writes_leave_the_file_untouched() -> TestResult {
    let fixture = Fixture::new()?;
    let text = fixture.write("same.txt", "unchanged\n")?;
    let json = fixture.write("same.json", "{\n  \"a\": 1\n}\n")?;
    let changed = fixture.write("changed.txt", "old\n")?;
    let text_time = age(&text)?;
    let json_time = age(&json)?;
    let changed_time = age(&changed)?;
    let ran = fixture.run(
        "fs.write(\"same.txt\", \"unchanged\\n\")\nfs.write_json(\"same.json\", {\"a\": \
         1})\nfs.write(\"changed.txt\", \"new\\n\")\n",
        &Env::new(),
    )?;
    ran.run.result.clone()?;
    assert_eq!(fs::metadata(&text)?.modified()?, text_time);
    assert_eq!(fs::metadata(&json)?.modified()?, json_time);
    assert_ne!(fs::metadata(&changed)?.modified()?, changed_time);
    assert_eq!(fixture.read("changed.txt")?, "new\n");
    assert_eq!(ran.accesses(fixture.root()), [
        ("project/changed.txt".to_owned(), "write".to_owned()),
        ("project/same.json".to_owned(), "write".to_owned()),
        ("project/same.txt".to_owned(), "write".to_owned()),
    ]);
    Ok(())
}

/// Every `fs` function's accesses appear in the task's record, missing files and
/// directory listings included, and paths resolve against the task directory,
/// with absolute paths and paths outside the tree allowed.
#[test]
fn every_fs_access_is_recorded() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write("src/b.rs", "")?;
    fixture.write("src/a.rs", "")?;
    fixture.write("src/nested/c.rs", "")?;
    fixture.write("src/notes.md", "")?;
    fixture.write("text.txt", "plain \u{e9}\n")?;
    fixture.write("data.json", "[1]")?;
    fs::write(fixture.root().join("outside.txt"), "outside")?;
    let absolute = fixture.root().join("outside.txt");
    let ran = fixture.run(
        &format!(
            "print(fs.read(\"text.txt\") == \"plain \\u00e9\\n\")\nprint(fs.read(\"../outside.txt\"), \
             fs.read(\"{}\"))\nprint(fs.read_json(\"data.json\"))\nfs.write(\"gen/deep/out.txt\", \
             \"x\")\nprint(fs.exists(\"text.txt\"), fs.exists(\"absent\"), \
             fs.exists(\"src\"))\nprint(fs.list(\"src\"))\nprint(fs.glob(\"src/**/*.rs\"))\n\
             print(fs.glob(\"*.missing\"))\nprint(fs.list(\"no-such-dir\"))\n",
            absolute.display()
        ),
        &Env::new(),
    )?;
    let message = ran.failure()?;
    assert!(
        message.contains("fs.list(): cannot list") && message.contains("no-such-dir"),
        "{message}"
    );
    assert_eq!(ran.printed, [
        "True",
        "outside outside",
        "[1]",
        "True False True",
        "[\"a.rs\", \"b.rs\", \"nested\", \"notes.md\"]",
        "[\"src/a.rs\", \"src/b.rs\", \"src/nested/c.rs\"]",
        "[]",
    ]);
    let kinds = |kind: AccessKind| kind.name().to_owned();
    assert_eq!(ran.accesses(fixture.root()), [
        ("outside.txt".to_owned(), kinds(AccessKind::Read)),
        ("project".to_owned(), kinds(AccessKind::List)),
        ("project/../outside.txt".to_owned(), kinds(AccessKind::Read)),
        ("project/absent".to_owned(), kinds(AccessKind::Probe)),
        ("project/data.json".to_owned(), kinds(AccessKind::Read)),
        ("project/gen".to_owned(), kinds(AccessKind::Write)),
        ("project/gen/deep".to_owned(), kinds(AccessKind::Write)),
        (
            "project/gen/deep/out.txt".to_owned(),
            kinds(AccessKind::Write)
        ),
        ("project/no-such-dir".to_owned(), kinds(AccessKind::Probe)),
        ("project/src".to_owned(), "probe+list".to_owned()),
        ("project/src/nested".to_owned(), kinds(AccessKind::List)),
        ("project/text.txt".to_owned(), "read+probe".to_owned()),
    ]);
    assert_eq!(
        fs::read_to_string(fixture.project().join("gen/deep/out.txt"))?,
        "x"
    );

    let ran = fixture.run("fs.read(\"missing.txt\")\n", &Env::new())?;
    let message = ran.failure()?;
    assert!(
        message.contains("fs.read(): cannot read") && message.contains("missing.txt"),
        "{message}"
    );
    assert_eq!(ran.accesses(fixture.root()), [(
        "project/missing.txt".to_owned(),
        kinds(AccessKind::Probe)
    )]);
    assert!(
        !fixture.project().join("missing.txt").exists(),
        "a failed read creates nothing"
    );
    Ok(())
}

/// `fs.read` replaces bytes that are not valid UTF-8, and `fs.list` defaults to
/// the task directory.
#[test]
fn read_replaces_invalid_utf8_and_list_defaults_to_the_task_directory() -> TestResult {
    let fixture = Fixture::new()?;
    fs::write(fixture.project().join("bytes.bin"), [b'a', 0xFF, b'b'])?;
    let program = format!(
        "print(fs.read(\"bytes.bin\") == \"a{}b\")\nprint(fs.list())\n",
        char::REPLACEMENT_CHARACTER
    );
    let ran = fixture.run(&program, &Env::new())?;
    ran.run.result.clone()?;
    assert_eq!(ran.printed, ["True", "[\"bytes.bin\", \"kizz.md\"]"]);
    Ok(())
}
