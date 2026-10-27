// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of `kizz run`, which run the built binary with the real Linux
//! tracer on fixture projects.
//!
//! Each test writes a project into one temporary directory and gives every run of
//! `kizz` a cache in another. Every run gets an explicit environment that holds only
//! `KIZZ_CACHE_DIR`, set to that cache, and `PATH`, which the tasks inherit to find
//! their programs, and runs with `--color never --no-banner`.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{
    error::Error,
    fs,
    io::{self, BufRead as _, BufReader, Lines},
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
};

use kizz_store::{LOCK_FILE_NAME, NotCachedReason, RunReason, StepRecord, Store};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The first line of every task's body but [`GREET`]'s, which writes it out, so
/// its commands find their programs.
const INHERIT_PATH: &str = "env.inherit(\"PATH\")";

/// The global options of every run: no color and no banner.
const PLAIN: [&str; 3] = ["--color", "never", "--no-banner"];

/// The `PATH` every run of `kizz` gets, and its tasks inherit.
const PATH: &str = "/usr/bin:/bin";

/// The one-line reason of a command that never ran.
const NEVER_RUN: &str = "never run: there is no record for this step";

/// A task named `greet`, whose first command prints its arguments and whose
/// second writes to standard error; its block opens on line 3.
const GREET: &str = "# Greet

```starlark name=greet
env.inherit(\"PATH\")
run(\"echo\", \"args:\", *task.args)
run(\"sh\", \"-c\", \"echo to-error >&2\")
```
";

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

/// A project and a cache, each in a temporary directory.
struct Project {
    /// The project directory.
    _dir: TempDir,
    /// The project directory, with every symbolic link resolved, as `kizz` sees its
    /// working directory.
    root: PathBuf,
    /// The cache directory.
    cache: TempDir,
}

/// The text of `lines`, each ended with a newline.
fn text(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The line `kizz run` prints for step `step`, whose command is `command`, as it
/// starts to run for `reason`.
fn run_line(
    command: &str,
    step: usize,
    reason: &str,
) -> String {
    format!("run `{command}` (step {step}): {reason}")
}

/// The line `kizz run` prints for step `step`, whose command is `command`, when it
/// comes from the cache.
fn cached_line(
    command: &str,
    step: usize,
) -> String {
    format!("cached `{command}` (step {step})")
}

impl Project {
    /// An empty project and an empty cache.
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        Ok(Self {
            _dir: dir,
            root,
            cache: tempfile::tempdir()?,
        })
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

    /// Write a `kizz.md` at `relative` with one task under the heading `heading`,
    /// whose info string is `starlark` and `info`, and whose body follows
    /// `env.inherit("PATH")`; the block opens on line 3.
    fn task(
        &self,
        relative: &str,
        heading: &str,
        info: &str,
        body: &str,
    ) -> TestResult {
        self.write(
            relative,
            &format!("# {heading}\n\n```starlark {info}\n{INHERIT_PATH}\n{body}\n```\n"),
        )
    }

    /// The command that runs `kizz --color never --no-banner` with `args` in the
    /// directory `dir` of the project, with the project's cache, `PATH`, and no other
    /// environment.
    fn command(
        &self,
        dir: &str,
        args: &[&str],
    ) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kizz"));
        command
            .args(PLAIN)
            .args(args)
            .env_clear()
            .env("KIZZ_CACHE_DIR", self.cache.path())
            .env("PATH", PATH)
            .current_dir(self.root.join(dir))
            .stdin(Stdio::null());
        command
    }

    /// Run `kizz` with `args` in the directory `dir` of the project.
    fn run(
        &self,
        dir: &str,
        args: &[&str],
    ) -> TestResult<Run> {
        let output = self.command(dir, args).output()?;
        Ok(Run {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Start `kizz` with `args` in the project, with piped output, and wait until
    /// its task's command prints `started`. Returns the process, the lines before
    /// `started`, and the rest of its standard output, which the caller keeps
    /// reading, so `kizz` can still write to it.
    fn start(
        &self,
        args: &[&str],
    ) -> TestResult<(Child, Vec<String>, Lines<BufReader<ChildStdout>>)> {
        let mut child = self
            .command("", args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("no standard output")?;
        let mut lines = BufReader::new(stdout).lines();
        let mut before = Vec::new();
        loop {
            let line = lines
                .next()
                .ok_or("kizz ended before its command started")??;
            if line == "started" {
                break;
            }
            before.push(line);
        }
        Ok((child, before, lines))
    }

    /// Every step record in the cache, with the database closed again.
    fn step_records(&self) -> TestResult<Vec<StepRecord>> {
        let store = Store::open(self.cache.path())?;
        let records = {
            let txn = store.read()?;
            txn.scan::<StepRecord>(&[])?
        };
        store.close()?;
        Ok(records.into_iter().map(|(_, record)| record).collect())
    }

    /// Whether the lock file is in the cache.
    fn has_lock_file(&self) -> bool {
        self.cache.path().join(LOCK_FILE_NAME).exists()
    }
}

/// `kizz run NAME -- a b` prints each command's line before its output, live, and
/// the arguments reach the first command through the program's `task.args`; a
/// second run prints each command as cached and replays its stored output,
/// standard error on standard error; `--no-cache` runs every command again with
/// its reason, and `--explain` prints the full reasons, also of cached commands;
/// other arguments run the command that they change, with the changed words as its
/// reason, and leave the other one cached.
#[test]
fn run_shows_each_command_and_its_output_then_replays_it_from_the_cache() -> TestResult {
    let project = Project::new()?;
    project.write("kizz.md", GREET)?;
    let echo = "echo args: a b";
    let error = "sh -c echo to-error >&2";

    let first = project.run("", &["run", "greet", "--", "a", "b"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            &run_line(echo, 1, NEVER_RUN),
            "args: a b",
            &run_line(error, 2, NEVER_RUN),
            "run succeeded: kizz.md:3 (Greet); 2 commands ran, 0 cached",
        ])
    );
    assert_eq!(first.stderr, "to-error\n", "{first:?}");

    let second = project.run("", &["run", "greet", "--", "a", "b"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert_eq!(
        second.stdout,
        text(&[
            &cached_line(echo, 1),
            "args: a b",
            &cached_line(error, 2),
            "run succeeded: kizz.md:3 (Greet); 0 commands ran, 2 cached",
        ])
    );
    assert_eq!(second.stderr, "to-error\n", "{second:?}");

    let forced = project.run("", &[
        "run",
        "--no-cache",
        "--explain",
        "greet",
        "--",
        "a",
        "b",
    ])?;
    assert_eq!(forced.status, Some(0), "{forced:?}");
    assert_eq!(
        forced.stdout,
        text(&[
            &format!("run `{echo}` (step 1)"),
            "  ran because:",
            "    `kizz run --no-cache` was given",
            "args: a b",
            &format!("run `{error}` (step 2)"),
            "  ran because:",
            "    `kizz run --no-cache` was given",
            "run succeeded: kizz.md:3 (Greet); 2 commands ran, 0 cached",
        ])
    );
    assert_eq!(forced.stderr, "to-error\n", "{forced:?}");
    let records = project.step_records()?;
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(
        records
            .iter()
            .all(|record| record.reason == RunReason::RunNoCache && record.not_cached.is_none()),
        "{records:?}"
    );

    let explained = project.run("", &["run", "--explain", "greet", "--", "a", "b"])?;
    assert_eq!(explained.status, Some(0), "{explained:?}");
    assert_eq!(
        explained.stdout,
        text(&[
            &cached_line(echo, 1),
            "  cached: no recorded input changed",
            "args: a b",
            &cached_line(error, 2),
            "  cached: no recorded input changed",
            "run succeeded: kizz.md:3 (Greet); 0 commands ran, 2 cached",
        ])
    );

    let other = project.run("", &["run", "greet", "--", "c"])?;
    assert_eq!(other.status, Some(0), "{other:?}");
    assert_eq!(
        other.stdout,
        text(&[
            &run_line(
                "echo args: c",
                1,
                "the arguments changed: word 3: `a` became `c`, word 4 removed: `b`"
            ),
            "args: c",
            &cached_line(error, 2),
            "run succeeded: kizz.md:3 (Greet); 1 command ran, 1 cached",
        ])
    );
    Ok(())
}

/// The task the fewest directory levels below the current directory wins, and
/// only the tasks under the current directory count; a task without targets is
/// never built, but `kizz run` runs it.
#[test]
fn the_nearest_task_of_the_name_runs() -> TestResult {
    let project = Project::new()?;
    project.task(
        "sub/kizz.md",
        "Near",
        "name=pick",
        "run(\"echo\", \"near\")",
    )?;
    project.task(
        "sub/deeper/down/kizz.md",
        "Far",
        "name=pick",
        "run(\"echo\", \"far\")",
    )?;

    let near = project.run("", &["run", "pick"])?;
    assert_eq!(near.status, Some(0), "{near:?}");
    assert_eq!(
        near.stdout,
        text(&[
            &run_line("echo near", 1, NEVER_RUN),
            "near",
            "run succeeded: sub/kizz.md:3 (Near); 1 command ran, 0 cached",
        ])
    );

    let far = project.run("sub/deeper", &["run", "pick"])?;
    assert_eq!(far.status, Some(0), "{far:?}");
    assert_eq!(
        far.stdout,
        text(&[
            &run_line("echo far", 1, NEVER_RUN),
            "far",
            "run succeeded: down/kizz.md:3 (Far); 1 command ran, 0 cached",
        ])
    );
    Ok(())
}

/// A task that fails exits with status 1; a word after the name without `--`, an
/// unknown name, a tie, and a configuration error elsewhere in the tree are usage
/// errors with status 2, which run nothing.
#[test]
fn exit_statuses_follow_kizz_build() -> TestResult {
    let project = Project::new()?;
    project.write("kizz.md", GREET)?;
    project.task(
        "fail/kizz.md",
        "Fail",
        "name=fail",
        "run(\"false\", expect = [0])",
    )?;
    project.task("a/kizz.md", "A", "name=dup", "run(\"echo\", \"a\")")?;
    project.task("b/kizz.md", "B", "name=dup", "run(\"echo\", \"b\")")?;

    let failed = project.run("", &["run", "fail"])?;
    assert_eq!(failed.status, Some(1), "{failed:?}");
    assert!(
        failed
            .stdout
            .ends_with("run failed: fail/kizz.md:3 (Fail); 1 command ran, 0 cached\n"),
        "{failed:?}"
    );
    assert!(
        failed
            .stderr
            .contains("error: fail/kizz.md:3 (Fail) failed:"),
        "{failed:?}"
    );

    let word = project.run("", &["run", "greet", "extra"])?;
    assert_eq!(word.status, Some(2), "{word:?}");
    assert!(word.stderr.contains("right side of"), "{word:?}");
    assert_eq!(word.stdout, "", "{word:?}");

    let unknown = project.run("", &["run", "nothing"])?;
    assert_eq!(unknown.status, Some(2), "{unknown:?}");
    assert!(
        unknown.stderr.contains(
            "error: no task named `nothing` under `.`; the task names under `.` are: dup, fail, \
             greet"
        ),
        "{unknown:?}"
    );
    assert_eq!(unknown.stdout, "", "{unknown:?}");

    let tie = project.run("", &["run", "dup"])?;
    assert_eq!(tie.status, Some(2), "{tie:?}");
    assert!(
        tie.stderr.contains(
            "error: 2 tasks named `dup` are 1 directory level below `.`, so kizz cannot choose \
             one: a/kizz.md:3 (A), b/kizz.md:3 (B)"
        ),
        "{tie:?}"
    );
    assert_eq!(tie.stdout, "", "{tie:?}");

    project.task("broken/kizz.md", "Broken", "bogus=1", "x = 1")?;
    let broken = project.run("", &["run", "greet"])?;
    assert_eq!(broken.status, Some(2), "{broken:?}");
    assert!(
        broken.stderr.contains("unknown task option `bogus=1`"),
        "{broken:?}"
    );
    assert_eq!(broken.stdout, "", "{broken:?}");
    assert!(!project.has_lock_file());
    Ok(())
}

/// A task that ignores the non-zero exit of `run("false")` runs with status 0 and
/// a warning that names `kizz run --fail-unchecked`; with `--fail-unchecked`, the
/// task fails and `kizz run` exits with status 1, and the task's failure, like the
/// warning, names `kizz run --fail-unchecked`, never `kizz build`.
#[test]
fn fail_unchecked_names_kizz_run_and_exits_with_1() -> TestResult {
    let project = Project::new()?;
    project.task("kizz.md", "Ignore", "name=ignore", "run(\"false\")")?;

    let warned = project.run("", &["run", "ignore"])?;
    assert_eq!(warned.status, Some(0), "{warned:?}");
    assert!(
        warned
            .stderr
            .contains("; `kizz run --fail-unchecked` makes this fail the task\n"),
        "{warned:?}"
    );
    assert!(!warned.stderr.contains("kizz build"), "{warned:?}");

    for args in [
        &["run", "--fail-unchecked", "ignore"][..],
        &["run", "--no-cache", "--fail-unchecked", "ignore"][..],
    ] {
        let failed = project.run("", args)?;
        assert_eq!(failed.status, Some(1), "{args:?}: {failed:?}");
        for part in [
            "error: kizz.md:3 (Ignore) failed:\n  the task program left a non-zero exit \
             unchecked, and `kizz run --fail-unchecked` makes that fail the task\n  command \
             `false`: exit code 1, which the task program never checked: its `run()` has no \
             `expect`, and the program never read the result's `rc` or `signal`\n",
            "; the task fails because `kizz run --fail-unchecked` was given\n",
        ] {
            assert!(
                failed.stderr.contains(part),
                "{part:?} in {args:?}: {failed:?}"
            );
        }
        assert!(
            !failed.stderr.contains("kizz build"),
            "{args:?}: {failed:?}"
        );
        assert!(
            failed.stdout.contains("run failed: kizz.md:3 (Ignore); "),
            "{args:?}: {failed:?}"
        );
    }
    Ok(())
}

/// Ctrl-C stops the running command and the run: kizz exits with status 130,
/// leaves no lock file, and leaves no cache entry for the interrupted command, only
/// the marker that is never a cache hit.
#[test]
fn an_interrupted_run_exits_with_130_and_leaves_no_lock_file() -> TestResult {
    let project = Project::new()?;
    project.task(
        "kizz.md",
        "Wait",
        "name=wait",
        "run(\"sh\", \"-c\", \"echo started; sleep 30\")",
    )?;
    let (child, before, lines) = project.start(&["run", "wait"])?;
    assert_eq!(before, [run_line(
        "sh -c echo started; sleep 30",
        1,
        NEVER_RUN
    )]);
    let pid = libc::pid_t::try_from(child.id())?;
    // SAFETY: `kill` only sends a signal, here to the `kizz` process this test started
    // and has not yet waited for, so the process ID still names it.
    let sent = unsafe { libc::kill(pid, libc::SIGINT) };
    assert_eq!(sent, 0, "{}", io::Error::last_os_error());

    let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{rest:?} {stderr}");
    assert_eq!(rest, [
        "run interrupted: kizz.md:3 (Wait); 1 command ran, 0 cached"
    ]);
    assert!(
        stderr.contains("error: the run was interrupted"),
        "{stderr}"
    );
    assert!(!project.has_lock_file());

    let records = project.step_records()?;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = records.first().ok_or("no record")?;
    assert!(
        matches!(record.not_cached, Some(NotCachedReason::Interrupted { .. })),
        "{record:?}"
    );
    Ok(())
}

/// A build started while a run holds the cache lock waits, says at once what it
/// waits for, and runs once the run ends, finding the run's result in the cache.
#[test]
fn a_build_waits_for_a_run_and_says_what_it_waits_for() -> TestResult {
    let project = Project::new()?;
    project.task(
        "kizz.md",
        "Hold",
        "ci name=hold",
        "run(\"sh\", \"-c\", \"echo started; i=0; while [ ! -e go ] && [ $i -lt 600 ]; do sleep \
         0.1; i=$((i + 1)); done\")",
    )?;
    let (first, _, first_lines) = project.start(&["run", "hold"])?;

    let mut second = project
        .command("", &["build", "ci"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = second.stderr.take().ok_or("no standard error")?;
    let mut lines = BufReader::new(stderr).lines();
    let waiting = lines.next().ok_or("the build ended without waiting")??;
    assert!(
        waiting.starts_with(&format!(
            "waiting for the kizz cache, which is in use by process {} ",
            first.id()
        )),
        "{waiting}"
    );
    assert!(
        waiting.ends_with(" --color never --no-banner run hold"),
        "{waiting}"
    );

    project.write("go", "")?;
    let first_rest: Vec<String> = first_lines.collect::<Result<_, _>>()?;
    let first = first.wait_with_output()?;
    assert_eq!(first.status.code(), Some(0), "{first:?} {first_rest:?}");
    assert_eq!(
        first_rest.last().map(String::as_str),
        Some("run succeeded: kizz.md:3 (Hold); 1 command ran, 0 cached"),
        "{first_rest:?}"
    );
    let second = second.wait_with_output()?;
    let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
    assert_eq!(second.status.code(), Some(0), "{second:?} {rest:?}");
    assert_eq!(
        String::from_utf8_lossy(&second.stdout),
        text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"])
    );
    assert!(!project.has_lock_file());
    Ok(())
}
