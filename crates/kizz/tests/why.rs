// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Golden tests of `kizz why`, which run the built binary on a cache that the tests
//! populate in a temporary directory.
//!
//! Each test writes a project into one temporary directory and fills a cache in
//! another by running builds with the engine of `kizz_core` and a fake tracer, which
//! reports the accesses that a step's arguments name. Every run of the binary gets
//! an explicit environment that holds only `KIZZ_CACHE_DIR`, set to that cache.

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsString,
    fs,
    io::{self, BufRead as _, BufReader},
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    process::{Command, Stdio},
};

use kizz_core::{Engine, ProgramHash, ProgramParts, StepExecutor as _, StepSpec, TaskIdentity};
use kizz_store::{CacheLock, LockDetails, Store, TaskId};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, CapturedOutput, EchoSink,
    OutputStream, RunRequest, RunResult, Termination, TraceError, Tracer,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The global options of every golden run: no color and no banner.
const PLAIN: [&str; 3] = ["--color", "never", "--no-banner"];

/// What the step of the fixture's task reads, checks, and ignores.
const STEP: [&str; 5] = [
    "tool",
    "read:in.txt",
    "kernel:/proc/cpuinfo",
    "probe:/sys/kernel",
    "read:scratch.tmp",
];

/// The report of the fixture's first build.
const FIRST_REPORT: &str = "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool read:in.txt kernel:/proc/cpuinfo probe:/sys/kernel read:scratch.tmp`
    ran because:
      never run: there is no record for this step
    untracked /proc and /sys accesses: `/proc/cpuinfo` (read), `/sys/kernel` (probe)
    ignored accesses: 1
";

/// A tracer failure from `error`.
fn tracer_failure(error: &dyn Error) -> TraceError {
    TraceError::Tracer {
        message: error.to_string(),
        source: None,
    }
}

/// A tracer that runs nothing and reports, for each argument `read:PATH`,
/// `kernel:PATH`, or `probe:PATH`, a read, a read, or a check of the path, relative
/// to the working directory.
struct FakeTracer;

impl Tracer for FakeTracer {
    fn run(
        &self,
        request: &RunRequest,
        _echo: &dyn EchoSink,
        _cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Exec);
        for arg in request.args.iter().skip(1) {
            let arg = arg.to_str().unwrap_or_default();
            let Some((op, path)) = arg.split_once(':') else {
                continue;
            };
            let kind = match op {
                "read" | "kernel" => AccessKind::Read,
                "probe" => AccessKind::Probe,
                _ => continue,
            };
            let path = AbsolutePath::new(request.cwd.as_path().join(path))
                .map_err(|error| tracer_failure(&error))?;
            accesses.record(path, kind);
        }
        Ok(RunResult {
            termination: Termination::Exited(0),
            output: CapturedOutput::empty(request.capture),
            accesses,
            incomplete: None,
        })
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

/// What one run of `kizz` produced.
#[derive(Debug)]
struct Run {
    /// The exit code, or `None` when a signal killed the process.
    status: Option<i32>,
    /// Standard output, with invalid UTF-8 replaced.
    stdout: String,
    /// Standard error, with invalid UTF-8 replaced.
    stderr: String,
}

/// A project with one task and a cache, each in a temporary directory.
struct Fixture {
    /// The project directory.
    project: TempDir,
    /// The project directory, with every symbolic link resolved, as the binary sees
    /// its working directory.
    root: PathBuf,
    /// The cache directory.
    cache: TempDir,
}

impl Fixture {
    /// A new fixture: `kizz.md`, `in.txt`, an executable `bin/tool`, and an empty
    /// directory `sub`.
    fn new() -> TestResult<Self> {
        let project = tempfile::tempdir()?;
        let root = project.path().canonicalize()?;
        let fixture = Self {
            project,
            root,
            cache: tempfile::tempdir()?,
        };
        fixture.write("kizz.md", "# Build\n\n```starlark ci\nrun(\"tool\")\n```\n")?;
        fixture.write("in.txt", "one")?;
        fixture.write("bin/tool", "#!/bin/sh\n")?;
        fs::set_permissions(
            fixture.root.join("bin/tool"),
            fs::Permissions::from_mode(0o755),
        )?;
        fs::create_dir_all(fixture.root.join("sub"))?;
        Ok(fixture)
    }

    /// Write `content` to `relative` in the project, creating parent directories.
    fn write(
        &self,
        relative: &str,
        content: &str,
    ) -> TestResult {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    /// The step of the task, with the environment variables `env` besides `PATH`.
    fn step(
        &self,
        env: &[(&str, &str)],
    ) -> TestResult<StepSpec> {
        let mut variables: BTreeMap<String, String> = env
            .iter()
            .map(|&(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        variables.insert(
            "PATH".to_owned(),
            self.root.join("bin").to_string_lossy().into_owned(),
        );
        Ok(StepSpec {
            argv: STEP.iter().map(|&arg| arg.to_owned()).collect(),
            stdin: None,
            env: variables,
            cwd: AbsolutePath::new(&self.root)?,
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            ignore: vec!["*.tmp".to_owned()],
            track: Vec::new(),
            timestamps: false,
            expect: None,
        })
    }

    /// Run one build of the task, whose step has the environment variables `env`
    /// besides `PATH`, on the fixture's cache.
    fn build(
        &self,
        env: &[(&str, &str)],
    ) -> TestResult {
        let identity = TaskIdentity {
            id: TaskId {
                file: self.root.join("kizz.md"),
                index: 0,
            },
            line: 3,
            heading: Some("Build".to_owned()),
            targets: vec!["ci".to_owned()],
            program: ProgramHash::from_bytes([7; 32]),
            parts: ProgramParts::default(),
            dir: AbsolutePath::new(&self.root)?,
        };
        let store = Store::open(self.cache.path())?;
        let engine = Engine::new(&FakeTracer, &store, &NoEcho, Cancellation::new(), None);
        let steps = engine.task_for(identity);
        steps.execute(&self.step(env)?)?;
        steps.finish(true)?;
        drop(steps);
        drop(engine);
        store.close()?;
        Ok(())
    }

    /// The command that runs `kizz` with `args` in the project directory, with the
    /// fixture's cache and no other environment.
    fn command(
        &self,
        args: &[&str],
    ) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kizz"));
        command
            .args(args)
            .env_clear()
            .env("KIZZ_CACHE_DIR", self.cache.path())
            .current_dir(self.project.path());
        command
    }

    /// Run `kizz --color never --no-banner` with `global` options, then `why` with
    /// `args`, in the project directory.
    fn why(
        &self,
        global: &[&str],
        args: &[&str],
    ) -> TestResult<Run> {
        let mut all = PLAIN.to_vec();
        all.extend_from_slice(global);
        all.push("why");
        all.extend_from_slice(args);
        let output = self.command(&all).output()?;
        Ok(Run {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// `kizz why` prints the reasons of the most recent build with each step's untracked
/// `/proc` and `/sys` accesses and its ignored count, for the current directory by
/// default and for a path given.
#[test]
fn why_prints_the_reasons_untracked_accesses_and_ignored_counts() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.build(&[])?;
    let run = fixture.why(&[], &[])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(run.stdout, FIRST_REPORT, "{run:?}");
    assert_eq!(run.stderr, "", "{run:?}");

    let file = fixture.why(&[], &["kizz.md"])?;
    assert_eq!(file.status, Some(0), "{file:?}");
    assert_eq!(
        file.stdout,
        FIRST_REPORT.replace("under `.`", "under `kizz.md`"),
        "{file:?}"
    );

    let sub = fixture.why(&[], &["sub"])?;
    assert_eq!(sub.status, Some(0), "{sub:?}");
    assert_eq!(
        sub.stdout, "No build has recorded a task under `sub`.\n",
        "{sub:?}"
    );

    // The lock file is gone afterwards: the command took the lock and released it.
    assert!(!fixture.cache.path().join("kizz.lock").exists());
    Ok(())
}

/// `kizz why` names the environment variables that changed and shows their values
/// only with `--verbose`.
#[test]
fn why_shows_environment_values_only_with_verbose() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.build(&[("TOKEN", "secret-one")])?;
    fixture.build(&[("TOKEN", "secret-two")])?;
    let expected = |change: &str| {
        format!(
            "The most recent build ran 1 task under `.`.

kizz.md:3 (Build) [ci]
  step 1: `tool read:in.txt kernel:/proc/cpuinfo probe:/sys/kernel read:scratch.tmp`
    ran because:
      the environment changed:
        {change}
    untracked /proc and /sys accesses: `/proc/cpuinfo` (read), `/sys/kernel` (probe)
    ignored accesses: 1
"
        )
    };

    let plain = fixture.why(&[], &[])?;
    assert_eq!(plain.status, Some(0), "{plain:?}");
    assert_eq!(plain.stdout, expected("changed TOKEN"), "{plain:?}");
    assert!(!plain.stdout.contains("secret"), "{plain:?}");

    for verbose in ["-v", "--verbose"] {
        let shown = fixture.why(&[verbose], &[])?;
        assert_eq!(shown.status, Some(0), "{shown:?}");
        assert_eq!(
            shown.stdout,
            expected("changed TOKEN from `secret-one` to `secret-two`"),
            "{shown:?}"
        );
    }
    Ok(())
}

/// `kizz why` reads the cache under the cache lock: while another process holds it,
/// it waits, says what it is waiting for, and prints the report once the lock is
/// released.
#[test]
fn why_waits_for_the_cache_lock() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.build(&[])?;
    let holder = LockDetails {
        command_line: vec!["kizz".to_owned(), "build".to_owned(), "ci".to_owned()],
        ..LockDetails::current()
    };
    let lock = CacheLock::acquire(fixture.cache.path(), &holder, |_| {})?;

    let mut args: Vec<OsString> = PLAIN.iter().map(OsString::from).collect();
    args.push("why".into());
    let mut child = Command::new(env!("CARGO_BIN_EXE_kizz"))
        .args(&args)
        .env_clear()
        .env("KIZZ_CACHE_DIR", fixture.cache.path())
        .current_dir(fixture.project.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().ok_or("no standard error")?;
    let mut lines = BufReader::new(stderr).lines();
    let waiting = lines.next().ok_or("kizz why ended without waiting")??;
    assert!(
        waiting.starts_with("waiting for the kizz cache, which is in use by process "),
        "{waiting}"
    );
    assert!(waiting.ends_with(": kizz build ci"), "{waiting}");

    lock.release()?;
    let output = child.wait_with_output()?;
    let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
    assert_eq!(output.status.code(), Some(0), "{rest:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), FIRST_REPORT);
    assert_eq!(rest, Vec::<String>::new());
    Ok(())
}
