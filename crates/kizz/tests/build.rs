// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of `kizz build`, which run the built binary with the real Linux
//! tracer on fixture projects.
//!
//! Each test writes a project into one temporary directory and gives every run of
//! `kizz` a cache in another. Every run gets an explicit environment that holds only
//! `KIZZ_CACHE_DIR`, set to that cache, and `PATH`, which the tasks inherit to find
//! their programs. The golden tests run with `--color never --no-banner`.
//!
//! Every task block in these projects starts with `env.inherit("PATH")`, so with a
//! one-line body, the blocks of a `kizz.md` open on lines 3, 10, 17, and so on.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{
    error::Error,
    ffi::CString,
    fs,
    io::{self, BufRead as _, BufReader, Lines},
    net::{Ipv4Addr, TcpListener},
    os::unix::{
        ffi::OsStrExt as _,
        fs::{MetadataExt as _, PermissionsExt as _, symlink},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    time::SystemTime,
};

use kizz_store::{
    Change, ChangedRead, Connection, LOCK_FILE_NAME, NotCachedReason, PathState, StepRecord, Store,
    Times,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The global options of every run: no color and no banner.
const PLAIN: [&str; 3] = ["--color", "never", "--no-banner"];

/// The `PATH` every run of `kizz` gets, and its tasks inherit.
const PATH: &str = "/usr/bin:/bin";

/// The start of every terminal escape sequence.
const ESCAPE: &str = "\u{1b}[";

/// The one-line reason of a command that never ran.
const NEVER_RUN: &str = "never run: there is no record for this step";

/// The variable that makes [`helper_process`] act as a helper program, which a
/// task runs, instead of doing nothing.
const HELPER_VARIABLE: &str = "KIZZ_BUILD_TEST_HELPER";

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

/// The line `kizz build` prints for a command that ran.
fn ran(
    command: &str,
    task: &str,
    reason: &str,
) -> String {
    format!("ran `{command}` in {task} step 1: {reason}")
}

/// The line `kizz build` prints when pass `pass` begins.
fn pass(pass: usize) -> String {
    format!(
        "pass {pass}: a command ran in pass {}, so every selected task is checked again",
        pass.saturating_sub(1)
    )
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

    /// Write the `kizz.md` of the directory `dir` with one task of target `ci` for
    /// each heading and body of `tasks`, in order; each body follows
    /// `env.inherit("PATH")`.
    fn kizz_md(
        &self,
        dir: &str,
        tasks: &[(&str, &str)],
    ) -> TestResult {
        let mut content = String::new();
        for (heading, body) in tasks {
            content.push_str("# ");
            content.push_str(heading);
            content.push_str("\n\n```starlark ci\n");
            content.push_str("env.inherit(\"PATH\")\n");
            content.push_str(body);
            content.push_str("\n```\n\n");
        }
        let file = if dir.is_empty() {
            "kizz.md".to_owned()
        } else {
            format!("{dir}/kizz.md")
        };
        self.write(&file, &content)
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

    /// Start `kizz build ci` in the project, with piped output, and wait until its
    /// task's command prints `started`, which must be the first line of output.
    /// Returns the process and the rest of its standard output, which the caller
    /// keeps reading, so `kizz` can still write to it.
    fn start_build(&self) -> TestResult<(Child, Lines<BufReader<ChildStdout>>)> {
        Self::start(self.command("", &["build", "ci"]))
    }

    /// Start `command`, a run of `kizz build`, as [`Self::start_build`] does.
    fn start(mut command: Command) -> TestResult<(Child, Lines<BufReader<ChildStdout>>)> {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("no standard output")?;
        let mut lines = BufReader::new(stdout).lines();
        let first = lines
            .next()
            .ok_or("kizz ended before its command started")??;
        assert_eq!(first, "started");
        Ok((child, lines))
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

/// Two tasks in the wrong order settle in two passes on the first build and in one
/// pass on the second, as the summaries report; the final pass, which runs nothing,
/// is not counted. With `--verbose`, the cached commands are listed in the learned
/// order.
#[test]
fn two_tasks_in_the_wrong_order_settle_in_two_passes_then_one() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Use", "run(\"cat\", \"out.txt\")"),
        ("Make", "run(\"sh\", \"-c\", \"echo made > out.txt\")"),
    ])?;

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            &ran("cat out.txt", "kizz.md:3 (Use)", NEVER_RUN),
            &ran("sh -c echo made > out.txt", "kizz.md:10 (Make)", NEVER_RUN),
            &pass(2),
            &ran(
                "cat out.txt",
                "kizz.md:3 (Use)",
                "`out.txt` changed (appeared)"
            ),
            &pass(3),
            "build succeeded: 2 tasks, settled in 2 passes; 3 commands ran, 0 cached",
        ])
    );
    assert_eq!(first.stderr, "", "{first:?}");

    let second = project.run("", &["--verbose", "build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert_eq!(
        second.stdout,
        text(&[
            "cached `sh -c echo made > out.txt` in kizz.md:10 (Make) step 1",
            "cached `cat out.txt` in kizz.md:3 (Use) step 1",
            "build succeeded: 2 tasks, settled in 1 pass; 0 commands ran, 2 cached",
        ])
    );
    assert_eq!(second.stderr, "", "{second:?}");

    // Without `--verbose`, cached commands are only counted.
    let third = project.run("", &["build", "ci"])?;
    assert_eq!(third.status, Some(0), "{third:?}");
    assert_eq!(
        third.stdout,
        text(&["build succeeded: 2 tasks, settled in 1 pass; 0 commands ran, 2 cached"])
    );
    assert!(!project.has_lock_file());
    Ok(())
}

/// `--explain` prints the full reason for every command, cached ones included.
#[test]
fn explain_prints_the_full_reason_for_every_command() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Use", "run(\"cat\", \"out.txt\")"),
        ("Make", "run(\"sh\", \"-c\", \"echo made > out.txt\")"),
    ])?;

    let first = project.run("", &["build", "--explain", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            "ran `cat out.txt` in kizz.md:3 (Use) step 1",
            "  ran because:",
            &format!("    {NEVER_RUN}"),
            "ran `sh -c echo made > out.txt` in kizz.md:10 (Make) step 1",
            "  ran because:",
            &format!("    {NEVER_RUN}"),
            &pass(2),
            "ran `cat out.txt` in kizz.md:3 (Use) step 1",
            "  ran because:",
            "    1 input changed:",
            "      `out.txt`: appeared; last written by step 1 of kizz.md:10 (Make) in this build",
            &pass(3),
            "build succeeded: 2 tasks, settled in 2 passes; 3 commands ran, 0 cached",
        ])
    );

    let second = project.run("", &["build", "--explain", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert_eq!(
        second.stdout,
        text(&[
            "cached `sh -c echo made > out.txt` in kizz.md:10 (Make) step 1",
            "  cached: no recorded input changed",
            "cached `cat out.txt` in kizz.md:3 (Use) step 1",
            "  cached: no recorded input changed",
            "build succeeded: 2 tasks, settled in 1 pass; 0 commands ran, 2 cached",
        ])
    );
    assert_eq!(second.stderr, "", "{second:?}");
    Ok(())
}

/// A later task's program that changes a cached command's input is work: pass 2
/// starts with a line that says a task program changed a file in pass 1, runs
/// `cat` again, and names the program as the input's last writer; the summary
/// reports the passes, and the next build, in the learned order, settles in one.
#[test]
fn a_task_program_that_changes_an_input_runs_the_command_again() -> TestResult {
    let project = Project::new()?;
    project.write("in.txt", "old\n")?;
    project.kizz_md("", &[("Use", "run(\"cat\", \"in.txt\")")])?;
    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            &ran("cat in.txt", "kizz.md:3 (Use)", NEVER_RUN),
            &pass(2),
            "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
        ])
    );

    project.kizz_md("", &[
        ("Use", "run(\"cat\", \"in.txt\")"),
        ("Update", "fs.write(\"in.txt\", \"newer text\\n\")"),
    ])?;
    let second = project.run("", &["build", "--explain", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert_eq!(
        second.stdout,
        text(&[
            "cached `cat in.txt` in kizz.md:3 (Use) step 1",
            "  cached: no recorded input changed",
            "pass 2: a task program changed a file in pass 1, so every selected task is checked \
             again",
            "ran `cat in.txt` in kizz.md:3 (Use) step 1",
            "  ran because:",
            "    1 input changed:",
            "      `in.txt`: size changed from 4 to 11 bytes; last written by the program of \
             kizz.md:10 (Update) in this build",
            &pass(3),
            "build succeeded: 2 tasks, settled in 2 passes; 1 command ran, 0 cached",
        ])
    );
    assert_eq!(second.stderr, "", "{second:?}");

    let third = project.run("", &["build", "ci"])?;
    assert_eq!(third.status, Some(0), "{third:?}");
    assert_eq!(
        third.stdout,
        text(&["build succeeded: 2 tasks, settled in 1 pass; 0 commands ran, 1 cached"])
    );
    assert_eq!(third.stderr, "", "{third:?}");
    assert!(!project.has_lock_file());
    Ok(())
}

/// Building in a subdirectory and then in its parent directory reuses the
/// subdirectory's results.
#[test]
fn building_in_a_subdirectory_then_its_parent_reuses_the_results() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("sub", &[(
        "Sub",
        "run(\"sh\", \"-c\", \"echo sub > sub.txt\")",
    )])?;
    project.kizz_md("", &[("Top", "run(\"cat\", \"sub/sub.txt\")")])?;

    let sub = project.run("sub", &["build", "ci"])?;
    assert_eq!(sub.status, Some(0), "{sub:?}");
    assert_eq!(
        sub.stdout,
        text(&[
            &ran("sh -c echo sub > sub.txt", "kizz.md:3 (Sub)", NEVER_RUN),
            &pass(2),
            "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
        ])
    );

    let parent = project.run("", &["--verbose", "build", "ci"])?;
    assert_eq!(parent.status, Some(0), "{parent:?}");
    assert_eq!(
        parent.stdout,
        text(&[
            &ran("cat sub/sub.txt", "kizz.md:3 (Top)", NEVER_RUN),
            "cached `sh -c echo sub > sub.txt` in sub/kizz.md:3 (Sub) step 1",
            &pass(2),
            "build succeeded: 2 tasks, settled in 1 pass; 1 command ran, 1 cached",
        ])
    );
    assert_eq!(parent.stderr, "", "{parent:?}");
    Ok(())
}

/// A second build waits for the first, says at once what it waits for, and runs
/// once the first build ends.
#[test]
fn a_second_build_waits_for_the_first_and_says_what_it_waits_for() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Hold",
        "run(\"sh\", \"-c\", \"echo started; i=0; while [ ! -e go ] && [ $i -lt 600 ]; do sleep \
         0.1; i=$((i + 1)); done\", stream = True)",
    )])?;
    let (first, first_lines) = project.start_build()?;

    let mut second = project
        .command("", &["build", "ci"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = second.stderr.take().ok_or("no standard error")?;
    let mut lines = BufReader::new(stderr).lines();
    let waiting = lines
        .next()
        .ok_or("the second build ended without waiting")??;
    assert!(
        waiting.starts_with(&format!(
            "waiting for the kizz cache, which is in use by process {} ",
            first.id()
        )),
        "{waiting}"
    );
    assert!(
        waiting.ends_with(" --color never --no-banner build ci"),
        "{waiting}"
    );

    project.write("go", "")?;
    let first_rest: Vec<String> = first_lines.collect::<Result<_, _>>()?;
    let first = first.wait_with_output()?;
    assert_eq!(first.status.code(), Some(0), "{first:?} {first_rest:?}");
    assert_eq!(
        first_rest.last().map(String::as_str),
        Some("build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached"),
        "{first_rest:?}"
    );
    let second = second.wait_with_output()?;
    let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
    assert_eq!(second.status.code(), Some(0), "{second:?} {rest:?}");
    assert_eq!(rest, Vec::<String>::new());
    assert_eq!(
        String::from_utf8_lossy(&second.stdout),
        text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"])
    );
    assert!(!project.has_lock_file());
    Ok(())
}

/// Ctrl-C stops the running command and the build: kizz exits with status 130,
/// leaves no lock file, and leaves no cache entry for the interrupted command, only
/// the marker that is never a cache hit.
#[test]
fn an_interrupted_build_leaves_no_lock_file_and_no_cache_entry() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Wait",
        "run(\"sh\", \"-c\", \"echo started; sleep 30\", stream = True)",
    )])?;
    let (child, lines) = project.start_build()?;
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
        ran(
            "sh -c echo started; sleep 30",
            "kizz.md:3 (Wait)",
            NEVER_RUN
        ),
        "build interrupted: 1 task, stopped after 1 pass; 1 command ran, 0 cached".to_owned(),
    ]);
    assert!(
        stderr.contains("error: the build was interrupted"),
        "{stderr}"
    );
    assert!(!project.has_lock_file());

    let records = project.step_records()?;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = records.first().ok_or("no record")?;
    assert_eq!(record.spec.argv, ["sh", "-c", "echo started; sleep 30"]);
    assert!(
        matches!(record.not_cached, Some(NotCachedReason::Interrupted { .. })),
        "{record:?}"
    );
    Ok(())
}

/// Where the test sends the `SIGINT` of an interruption.
#[derive(Debug, Clone, Copy)]
enum Interrupt {
    /// To the `kizz` process only.
    Process,
    /// To the whole process group of `kizz`, as a terminal sends Ctrl-C; `kizz` runs
    /// in a new process group that it leads, so the signal never reaches the test.
    Group,
}

/// The command of a task that handles `SIGINT`: each `SIGINT` whose handler runs
/// adds a line to `interrupts`, and the handler then takes a few tenths of a second
/// and exits with code 3. Without an interrupt the command runs until the file
/// `stop` exists.
const HANDLES_INTERRUPTS: &str = "trap 'echo interrupt >> interrupts; sleep 0.3; exit 3' INT; echo \
                                  started; while [ ! -e stop ]; do sleep 0.05; done";

/// Interrupt a build whose running command handles `SIGINT`, sending the signal as
/// `interrupt` says: the command receives exactly one `SIGINT`, kizz exits with
/// status 130 and leaves no lock file, and the step's only record is the
/// interrupted marker, with the exit code the command chose.
fn interrupt_a_command_that_handles_it(interrupt: Interrupt) -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Trap",
        &format!("run(\"sh\", \"-c\", \"{HANDLES_INTERRUPTS}\", stream = True)"),
    )])?;
    let mut command = project.command("", &["build", "ci"]);
    if matches!(interrupt, Interrupt::Group) {
        command.process_group(0);
    }
    let (child, lines) = Project::start(command)?;
    let pid = libc::pid_t::try_from(child.id())?;
    let target = match interrupt {
        Interrupt::Process => pid,
        Interrupt::Group => pid.checked_neg().ok_or("no process group")?,
    };
    // SAFETY: `kill` only sends a signal, here to the `kizz` process this test
    // started and has not yet waited for, or to the process group it leads, so the
    // process ID still names it.
    let sent = unsafe { libc::kill(target, libc::SIGINT) };
    assert_eq!(sent, 0, "{}", io::Error::last_os_error());

    let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{rest:?} {stderr}");
    assert_eq!(rest, [
        ran(
            &format!("sh -c {HANDLES_INTERRUPTS}"),
            "kizz.md:3 (Trap)",
            NEVER_RUN
        ),
        "build interrupted: 1 task, stopped after 1 pass; 1 command ran, 0 cached".to_owned(),
    ]);
    assert_eq!(
        fs::read_to_string(project.root.join("interrupts"))?,
        "interrupt\n"
    );
    assert!(!project.has_lock_file());

    let records = project.step_records()?;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = records.first().ok_or("no record")?;
    assert_eq!(record.spec.argv, ["sh", "-c", HANDLES_INTERRUPTS]);
    assert_eq!(
        record.not_cached,
        Some(NotCachedReason::InterruptedExited { code: 3 })
    );

    // Once `stop` exists the command ends at once, and the next build runs it again,
    // saying why.
    project.write("stop", "")?;
    let next = project.run("", &["build", "ci"])?;
    assert_eq!(next.status, Some(0), "{next:?}");
    let reason = "the previous run could not be cached: interrupted, and exited with code 3";
    assert!(
        next.stdout.contains(&ran(
            &format!("sh -c {HANDLES_INTERRUPTS}"),
            "kizz.md:3 (Trap)",
            reason
        )),
        "{next:?}"
    );
    assert_eq!(
        fs::read_to_string(project.root.join("interrupts"))?,
        "interrupt\n"
    );
    Ok(())
}

/// Ctrl-C sent to the `kizz` process only reaches the running command exactly once.
#[test]
fn the_running_command_receives_an_interrupt_sent_to_kizz_exactly_once() -> TestResult {
    interrupt_a_command_that_handles_it(Interrupt::Process)
}

/// Ctrl-C sent to the whole process group of `kizz`, as a terminal sends it, reaches
/// the running command exactly once.
#[test]
fn the_running_command_receives_an_interrupt_sent_to_the_group_exactly_once() -> TestResult {
    interrupt_a_command_that_handles_it(Interrupt::Group)
}

/// A task that fails because a file it looks for does not exist yet prints its
/// error and the failing command's captured output, and runs again, and succeeds,
/// once a later task creates the file.
#[test]
fn a_task_that_fails_for_a_missing_file_runs_again_once_a_later_task_creates_it() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Check", "run(\"cat\", \"gen.txt\", expect = [0])"),
        ("Gen", "run(\"sh\", \"-c\", \"echo gen > gen.txt\")"),
    ])?;

    let run = project.run("", &["build", "ci"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        text(&[
            &ran("cat gen.txt", "kizz.md:3 (Check)", NEVER_RUN),
            &ran("sh -c echo gen > gen.txt", "kizz.md:10 (Gen)", NEVER_RUN),
            &pass(2),
            &ran(
                "cat gen.txt",
                "kizz.md:3 (Check)",
                "`gen.txt` changed (appeared)"
            ),
            &pass(3),
            "build succeeded: 2 tasks, settled in 2 passes; 3 commands ran, 0 cached",
        ])
    );
    for expected in [
        "error: kizz.md:3 (Check) failed:\n",
        "  command `cat gen.txt`: exit code 1, but `expect` accepts exit code 0\n",
        "  --- stderr ---\n  cat: gen.txt: No such file or directory\n",
    ] {
        assert!(run.stderr.contains(expected), "{expected:?}: {run:?}");
    }
    Ok(())
}

/// A task whose program a later task builds fails, then runs again, and succeeds,
/// once the program exists.
#[test]
fn a_task_whose_program_a_later_task_builds_runs_again_once_it_exists() -> TestResult {
    let project = Project::new()?;
    project.write("gen.sh", "#!/bin/sh\necho generated\n")?;
    fs::set_permissions(
        project.root.join("gen.sh"),
        fs::Permissions::from_mode(0o755),
    )?;
    fs::create_dir_all(project.root.join("bin"))?;
    // The first block has a two-line body, so the second opens on line 11.
    project.kizz_md("", &[
        (
            "Use",
            "env.set(\"PATH\", task.dir + \"/bin\")\nrun(\"gen\")",
        ),
        ("Compile", "run(\"cp\", \"gen.sh\", \"bin/gen\")"),
    ])?;

    let run = project.run("", &["build", "ci"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        text(&[
            &ran("gen", "kizz.md:3 (Use)", NEVER_RUN),
            &ran("cp gen.sh bin/gen", "kizz.md:11 (Compile)", NEVER_RUN),
            &pass(2),
            &ran("gen", "kizz.md:3 (Use)", "`bin/gen` changed (appeared)"),
            &pass(3),
            "build succeeded: 2 tasks, settled in 2 passes; 3 commands ran, 0 cached",
        ])
    );
    assert!(
        run.stderr.contains("error: kizz.md:3 (Use) failed:\n"),
        "{run:?}"
    );
    assert_eq!(run.stderr.matches(" failed:\n").count(), 1, "{run:?}");
    Ok(())
}

/// A task that keeps failing with unchanged inputs runs once, then fails from the
/// cache; the build stops with exit status 1 after a pass that runs nothing and
/// lists the failed task with the paths it looked for and did not find.
#[test]
fn a_task_that_keeps_failing_runs_once_then_fails_from_the_cache() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Check", "run(\"cat\", \"never.txt\", expect = [0])"),
        ("Fine", "run(\"true\")"),
    ])?;

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(1), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            &ran("cat never.txt", "kizz.md:3 (Check)", NEVER_RUN),
            &ran("true", "kizz.md:10 (Fine)", NEVER_RUN),
            &pass(2),
            "build failed: 2 tasks, 1 failed, settled in 1 pass; 2 commands ran, 0 cached",
        ])
    );
    // The error is printed once, when the task ran, and the summary lists the task
    // with the paths it looked for.
    assert_eq!(
        first
            .stderr
            .matches("error: kizz.md:3 (Check) failed:\n")
            .count(),
        1,
        "{first:?}"
    );
    for expected in [
        "  --- stderr ---\n  cat: never.txt: No such file or directory\n",
        "failed: kizz.md:3 (Check)\n  it looked for these paths and did not find them:\n",
        "\n    `never.txt`\n",
    ] {
        assert!(first.stderr.contains(expected), "{expected:?}: {first:?}");
    }

    // A second build has nothing new to try: it fails from the cache at once.
    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(1), "{second:?}");
    assert_eq!(
        second.stdout,
        text(&["build failed: 2 tasks, 1 failed, settled in 1 pass; 0 commands ran, 2 cached"])
    );
    assert!(
        second.stderr.contains("error: kizz.md:3 (Check) failed:\n"),
        "{second:?}"
    );
    assert!(!project.has_lock_file());
    Ok(())
}

/// A task that runs `./tool.sh`, an executable script, is cached until
/// `chmod a-x tool.sh` leaves its bytes as they were: the next build runs the step
/// again, its reason names the permission change, and the task fails because the
/// program is not executable.
#[test]
fn a_script_that_loses_its_execute_bits_runs_again_and_fails() -> TestResult {
    let project = Project::new()?;
    project.write("tool.sh", "#!/bin/sh\necho tool\n")?;
    let tool = project.root.join("tool.sh");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755))?;
    project.kizz_md("", &[("Tool", "run(\"./tool.sh\", expect = [0])")])?;

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            &ran("./tool.sh", "kizz.md:3 (Tool)", NEVER_RUN),
            &pass(2),
            "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
        ])
    );
    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert_eq!(
        second.stdout,
        text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"])
    );

    // `chmod a-x`: the mode loses every execute bit, and the bytes stay the same.
    let mode = fs::metadata(&tool)?.permissions().mode();
    fs::set_permissions(&tool, fs::Permissions::from_mode(mode & !0o111))?;
    let third = project.run("", &["build", "ci"])?;
    assert_eq!(third.status, Some(1), "{third:?}");
    assert_eq!(
        third.stdout,
        text(&[
            &ran(
                "./tool.sh",
                "kizz.md:3 (Tool)",
                "`tool.sh` changed (permissions changed from 0755 to 0644)"
            ),
            &pass(2),
            "build failed: 1 task, 1 failed, settled in 1 pass; 1 command ran, 0 cached",
        ])
    );
    for expected in ["error: kizz.md:3 (Tool) failed:\n", "is not executable"] {
        assert!(third.stderr.contains(expected), "{expected:?}: {third:?}");
    }
    assert!(!project.has_lock_file());
    Ok(())
}

/// The modification and change times of `path`.
fn times(path: &Path) -> TestResult<Times> {
    let metadata = fs::metadata(path)?;
    let nanos = |seconds: i64, nanoseconds: i64| {
        seconds
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_add(nanoseconds))
            .ok_or("time out of range")
    };
    Ok(Times {
        mtime_nanos: nanos(metadata.mtime(), metadata.mtime_nsec())?,
        ctime_nanos: nanos(metadata.ctime(), metadata.ctime_nsec())?,
    })
}

/// Give `path` the current time as its modification time, as `touch` does.
fn touch(path: &Path) -> TestResult {
    fs::File::open(path)?.set_times(fs::FileTimes::new().set_modified(SystemTime::now()))?;
    Ok(())
}

/// `run("sh", "-c", "cat in.txt > out.txt", timestamps = True)` runs again after
/// `touch in.txt`, with the old and new times of `in.txt` as its reason, and the
/// same task without the option stays cached.
#[test]
fn a_command_run_with_timestamps_runs_again_after_touch() -> TestResult {
    let copy = "sh -c cat in.txt > out.txt";
    for timestamps in [true, false] {
        let project = Project::new()?;
        project.write("in.txt", "input\n")?;
        let option = if timestamps {
            ", timestamps = True"
        } else {
            ""
        };
        project.kizz_md("", &[(
            "Copy",
            &format!("run(\"sh\", \"-c\", \"cat in.txt > out.txt\"{option})"),
        )])?;
        let first = project.run("", &["build", "ci"])?;
        assert_eq!(first.status, Some(0), "{first:?}");
        assert_eq!(
            first.stdout,
            text(&[
                &ran(copy, "kizz.md:3 (Copy)", NEVER_RUN),
                &pass(2),
                "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
            ])
        );
        let cached =
            text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"]);
        let second = project.run("", &["build", "ci"])?;
        assert_eq!(
            (second.status, &second.stdout),
            (Some(0), &cached),
            "{second:?}"
        );

        let input = project.root.join("in.txt");
        let old = times(&input)?;
        touch(&input)?;
        let new = times(&input)?;
        assert_ne!(old, new);
        let third = project.run("", &["build", "ci"])?;
        assert_eq!(third.status, Some(0), "{third:?}");
        if timestamps {
            let change = Change::Timestamps { old, new };
            assert_eq!(
                third.stdout,
                text(&[
                    &ran(
                        copy,
                        "kizz.md:3 (Copy)",
                        &format!("`in.txt` changed ({change})")
                    ),
                    &pass(2),
                    "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
                ])
            );
            assert!(change.to_string().starts_with("timestamps changed: "));
        } else {
            assert_eq!(third.stdout, cached, "{third:?}");
        }
        assert_eq!(fs::read_to_string(project.root.join("out.txt"))?, "input\n");
        let fourth = project.run("", &["build", "ci"])?;
        assert_eq!(
            (fourth.status, &fourth.stdout),
            (Some(0), &cached),
            "{fourth:?}"
        );
        assert!(!project.has_lock_file());
    }
    Ok(())
}

/// `kizz build` without a target lists the available targets and exits with
/// status 2, and says so when there is no task.
#[test]
fn build_without_a_target_lists_the_available_targets() -> TestResult {
    let project = Project::new()?;
    project.write(
        "kizz.md",
        "# One\n\n```starlark ci check\nx = 1\n```\n\n# Two\n\n```starlark lint\nx = 1\n```\n",
    )?;
    let run = project.run("", &["build"])?;
    assert_eq!(run.status, Some(2), "{run:?}");
    assert_eq!(run.stdout, "", "{run:?}");
    assert_eq!(
        run.stderr,
        "error: no target given; give one target, or several joined with `+`; the available \
         targets are: check, ci, lint\n"
    );
    assert!(!project.has_lock_file());

    let empty = Project::new()?;
    let run = empty.run("", &["build"])?;
    assert_eq!(run.status, Some(2), "{run:?}");
    assert_eq!(
        run.stderr,
        "error: no target given; give one target, or several joined with `+`; there is no task \
         under `.`\n"
    );
    Ok(())
}

/// A malformed target expression is a usage error with status 2.
#[test]
fn a_malformed_target_expression_is_a_usage_error() -> TestResult {
    let project = Project::new()?;
    let run = project.run("", &["build", "ci+"])?;
    assert_eq!(run.status, Some(2), "{run:?}");
    assert!(
        run.stderr
            .starts_with("error: invalid target expression `ci+`"),
        "{run:?}"
    );
    Ok(())
}

/// Warnings are loud, bold and colored, and repeated in the final summary.
#[test]
fn warnings_are_loud_and_repeated_in_the_summary() -> TestResult {
    let project = Project::new()?;
    project.write(
        "kizz.md",
        "# Untagged\n\n```starlark\nx = 1\n```\n\n# Task\n\n```starlark ci\nx = 1\n```\n",
    )?;
    let run = project.run("", &["build", "ci"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 0 cached"])
    );
    let first = run.stderr.lines().next().unwrap_or_default();
    let message = first
        .strip_prefix("warning: ")
        .ok_or_else(|| format!("no warning first: {run:?}"))?;
    assert!(message.contains("kizz.md:3 (Untagged)"), "{run:?}");
    assert_eq!(
        run.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n")
    );

    // The same build with `--color always`, and otherwise the same environment.
    let styled = Command::new(env!("CARGO_BIN_EXE_kizz"))
        .args(["--color", "always", "--no-banner", "build", "ci"])
        .env_clear()
        .env("KIZZ_CACHE_DIR", project.cache.path())
        .env("PATH", PATH)
        .current_dir(&project.root)
        .stdin(Stdio::null())
        .output()?;
    let stderr = String::from_utf8_lossy(&styled.stderr);
    assert_eq!(styled.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains(ESCAPE), "{stderr}");
    assert!(stderr.contains(message), "{stderr}");
    Ok(())
}

/// A target expression that selects no task builds nothing and warns about it,
/// listing the available targets.
#[test]
fn a_target_that_selects_no_task_warns() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[("Task", "x = 1")])?;
    let run = project.run("", &["build", "nothing"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        text(&["build succeeded: 0 tasks, settled in 1 pass; 0 commands ran, 0 cached"])
    );
    let message = "`nothing` selects no task under `.`; the available targets are: ci";
    assert_eq!(
        run.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n")
    );
    Ok(())
}

/// Act as a helper program when [`HELPER_VARIABLE`] says so, and do nothing
/// otherwise. In mode `x32`, make one system call through the x32 interface, which
/// the tracer cannot decode. In mode `xattr`, read the file `input.txt` and its
/// extended attribute `user.kizz`, and print both.
#[test]
fn helper_process() {
    let mode = std::env::var_os(HELPER_VARIABLE);
    if mode.as_ref().is_some_and(|mode| mode == "x32") {
        // SAFETY: `getpid` takes no arguments; the kernel either runs it or rejects
        // the x32 number with `ENOSYS`.
        let ret = unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
        eprintln!("x32 getpid returned {ret}");
    }
    if mode.as_ref().is_some_and(|mode| mode == "xattr") {
        let content = fs::read_to_string("input.txt").unwrap();
        let path = CString::new("input.txt").unwrap();
        let name = CString::new("user.kizz").unwrap();
        let mut value = [0_u8; 64];
        // SAFETY: `path` and `name` are NUL-terminated strings, and `value` has room
        // for `value.len()` bytes, which is all the kernel writes.
        let size = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        let size = usize::try_from(size).unwrap();
        println!(
            "{} {}",
            content.trim_end(),
            String::from_utf8_lossy(&value[..size])
        );
    }
}

/// Set the extended attribute `name` of the file at `path` to `value`, as the test
/// itself, outside any build.
fn set_attribute(
    path: &Path,
    name: &str,
    value: &[u8],
) -> TestResult {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let name = CString::new(name)?;
    // SAFETY: `path` and `name` are NUL-terminated strings, and `value` holds
    // `value.len()` bytes, which the kernel only reads.
    let ret = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// With the real tracer, a command that read a file and one of its extended
/// attributes stays cached while nothing changes, runs again when only that
/// attribute's value changes, with the reason naming the attribute, and is cached
/// again afterwards.
#[test]
fn a_command_that_read_an_extended_attribute_runs_again_when_it_changes() -> TestResult {
    let project = Project::new()?;
    project.write("input.txt", "input\n")?;
    let input = project.root.join("input.txt");
    set_attribute(&input, "user.kizz", b"one")?;
    let helper = std::env::current_exe()?;
    let helper = helper
        .to_str()
        .ok_or("the test executable's path is not UTF-8")?;
    project.kizz_md("", &[(
        "Attr",
        &format!(
            "run(\"{helper}\", \"helper_process\", \"--exact\", \"--test-threads=1\", env = \
             {{\"{HELPER_VARIABLE}\": \"xattr\"}})"
        ),
    )])?;
    let command = format!("{helper} helper_process --exact --test-threads=1");
    let cached = text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"]);

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert!(
        first.stdout.starts_with(&format!(
            "{}\n",
            ran(&command, "kizz.md:3 (Attr)", NEVER_RUN)
        )),
        "{first:?}"
    );
    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.stdout, cached, "{second:?}");

    set_attribute(&input, "user.kizz", b"two")?;
    let third = project.run("", &["build", "ci"])?;
    assert_eq!(third.status, Some(0), "{third:?}");
    assert!(
        third.stdout.starts_with(&format!(
            "{}\n",
            ran(
                &command,
                "kizz.md:3 (Attr)",
                "`input.txt` changed (extended attributes changed: `user.kizz` changed)"
            )
        )),
        "{third:?}"
    );
    let fourth = project.run("", &["build", "ci"])?;
    assert_eq!(fourth.stdout, cached, "{fourth:?}");
    Ok(())
}

/// A command that the tracer traces incompletely gets a loud warning, repeated in
/// the final summary; its result is not cached, so the next build runs it again
/// and says why.
#[test]
fn an_incompletely_traced_command_is_a_loud_warning() -> TestResult {
    let project = Project::new()?;
    let helper = std::env::current_exe()?;
    let helper = helper
        .to_str()
        .ok_or("the test executable's path is not UTF-8")?;
    project.kizz_md("", &[(
        "Odd",
        &format!(
            "run(\"{helper}\", \"helper_process\", \"--exact\", \"--test-threads=1\", env = \
             {{\"{HELPER_VARIABLE}\": \"x32\"}})"
        ),
    )])?;
    let command = format!("{helper} helper_process --exact --test-threads=1");

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert!(
        first.stdout.starts_with(&format!(
            "{}\n",
            ran(&command, "kizz.md:3 (Odd)", NEVER_RUN)
        )),
        "{first:?}"
    );
    let line = first.stderr.lines().next().unwrap_or_default();
    let message = line
        .strip_prefix("warning: ")
        .ok_or_else(|| format!("no warning first: {first:?}"))?;
    assert!(
        message.starts_with(&format!(
            "`{command}` in . was traced incompletely, so its result was not cached and it runs \
             again on the next build: a system call used a non-native interface: "
        )),
        "{message}"
    );
    assert!(message.contains("x32 interface"), "{message}");
    assert_eq!(
        first.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n")
    );

    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert!(
        second.stdout.starts_with(&format!(
            "ran `{command}` in kizz.md:3 (Odd) step 1: the previous run could not be cached: \
             incompletely traced: "
        )),
        "{second:?}"
    );
    assert!(second.stderr.contains("1 warning:\n"), "{second:?}");
    Ok(())
}

/// Tasks that keep changing each other's inputs stop at the pass limit, one more
/// than the number of tasks; the build names the inputs that kept changing and who
/// writes them, and exits with status 1.
#[test]
fn a_build_that_does_not_settle_names_the_changing_inputs() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        (
            "Ping",
            "run(\"sh\", \"-c\", \"cat b.txt; date +%s%N > a.txt\")",
        ),
        (
            "Pong",
            "run(\"sh\", \"-c\", \"cat a.txt; date +%s%N > b.txt\")",
        ),
    ])?;
    let run = project.run("", &["build", "ci"])?;
    assert_eq!(run.status, Some(1), "{run:?}");
    let ping = "sh -c cat b.txt; date +%s%N > a.txt";
    let reply = "sh -c cat a.txt; date +%s%N > b.txt";
    assert_eq!(
        run.stdout,
        text(&[
            &ran(ping, "kizz.md:3 (Ping)", NEVER_RUN),
            &ran(reply, "kizz.md:10 (Pong)", NEVER_RUN),
            &pass(2),
            &ran(ping, "kizz.md:3 (Ping)", "`b.txt` changed (appeared)"),
            &ran(
                reply,
                "kizz.md:10 (Pong)",
                "`a.txt` changed (content changed)"
            ),
            &pass(3),
            &ran(
                ping,
                "kizz.md:3 (Ping)",
                "`b.txt` changed (content changed)"
            ),
            &ran(
                reply,
                "kizz.md:10 (Pong)",
                "`a.txt` changed (content changed)"
            ),
            "build did not settle: 2 tasks, stopped after 3 passes; 6 commands ran, 0 cached",
        ])
    );
    assert_eq!(
        run.stderr,
        text(&[
            "error: the build did not settle: its last allowed pass, pass 3, still ran a command",
            "  these inputs kept changing:",
            "  `a.txt`, read by step 1 of kizz.md:10 (Pong), last written by step 1 of kizz.md:3 \
             (Ping) in this build",
            "  `b.txt`, read by step 1 of kizz.md:3 (Ping), last written by step 1 of kizz.md:10 \
             (Pong) in this build",
        ])
    );
    assert!(!project.has_lock_file());
    Ok(())
}

/// A command that reads `/dev/stdin` and `/dev/fd/3`, which lead into `/proc/self`,
/// is cached on the next build: only the `/proc` objects they reach are untracked,
/// not the state of whatever `kizz` itself has open under those numbers, and the
/// links `/dev/stdin` and `/dev/fd` are recorded with their targets.
#[test]
fn a_command_that_reads_dev_fd_paths_is_cached_on_the_next_build() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Descriptors",
        "run(\"sh\", \"-c\", \"cat /dev/stdin; exec 3</dev/null; cat /dev/fd/3\", expect = [0])",
    )])?;
    let command = "sh -c cat /dev/stdin; exec 3</dev/null; cat /dev/fd/3";

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            &ran(command, "kizz.md:3 (Descriptors)", NEVER_RUN),
            &pass(2),
            "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
        ])
    );
    let records = project.step_records()?;
    let [record] = records.as_slice() else {
        return Err(format!("expected one step record: {records:?}").into());
    };
    let state = |path: &str| {
        record
            .observations
            .iter()
            .find(|observation| observation.path == Path::new(path))
            .map(|observation| observation.state.clone())
            .ok_or_else(|| format!("no observation of {path}: {record:?}"))
    };
    let link_into_proc = |link: &str| -> TestResult<PathState> {
        Ok(PathState::Symlink {
            target: fs::read_link(link)?,
            pointee: Box::new(PathState::Untracked),
        })
    };
    assert_eq!(state("/dev/fd/3")?, PathState::Untracked);
    assert_eq!(state("/dev/fd")?, link_into_proc("/dev/fd")?);
    assert_eq!(state("/dev/stdin")?, link_into_proc("/dev/stdin")?);

    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert_eq!(
        second.stdout,
        text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"])
    );
    assert!(!project.has_lock_file());
    Ok(())
}

/// Run `kizz build ci` twice in `project`, whose one task runs `command`: the first
/// build must run it again for `reason`, and the second must be a cache hit.
fn reruns_then_hits(
    project: &Project,
    command: &str,
    reason: &str,
) -> TestResult {
    let rerun = project.run("", &["build", "ci"])?;
    assert_eq!(rerun.status, Some(0), "{rerun:?}");
    assert_eq!(
        rerun.stdout,
        text(&[
            &ran(command, "kizz.md:3 (Links)", reason),
            &pass(2),
            "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
        ])
    );
    let hit = project.run("", &["build", "ci"])?;
    assert_eq!(hit.status, Some(0), "{hit:?}");
    assert_eq!(
        hit.stdout,
        text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"])
    );
    Ok(())
}

/// Replace the symbolic link `link` with one to `target`.
fn relink(
    target: &str,
    link: &Path,
) -> TestResult {
    fs::remove_file(link)?;
    symlink(target, link)?;
    Ok(())
}

/// Links that lead into `/proc` stay ordinary dependencies with the real tracer: a
/// command that reads through a final link and a directory link into `/proc` is a
/// cache hit while they are unchanged, and it runs again when the final link is
/// retargeted to a regular file, retargeted back, or replaced by a regular file, and
/// when the directory link is retargeted to a regular directory. The links live
/// outside the project, because discovery follows directory links and would walk
/// all of `/proc`.
#[test]
fn retargeting_or_replacing_a_link_into_proc_runs_the_command_again() -> TestResult {
    let project = Project::new()?;
    let links_dir = tempfile::tempdir()?;
    let links = links_dir.path().canonicalize()?;
    let file_link = links.join("version-link");
    let dir_link = links.join("proc-link");
    fs::write(links.join("input.txt"), "regular input\n")?;
    fs::create_dir(links.join("inputs"))?;
    fs::write(links.join("inputs/version"), "regular version\n")?;
    symlink("/proc/version", &file_link)?;
    symlink("/proc", &dir_link)?;
    let read_file = file_link.display().to_string();
    let read_dir = dir_link.join("version").display().to_string();
    project.kizz_md("", &[(
        "Links",
        &format!("run(\"cat\", \"{read_file}\", \"{read_dir}\")"),
    )])?;
    let command = format!("cat {read_file} {read_dir}");

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert_eq!(
        first.stdout,
        text(&[
            &ran(&command, "kizz.md:3 (Links)", NEVER_RUN),
            &pass(2),
            "build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached",
        ])
    );
    let unchanged = project.run("", &["build", "ci"])?;
    assert_eq!(unchanged.status, Some(0), "{unchanged:?}");
    assert_eq!(
        unchanged.stdout,
        text(&["build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached"])
    );

    relink("input.txt", &file_link)?;
    reruns_then_hits(
        &project,
        &command,
        &format!("`{read_file}` changed (link target changed from /proc/version to input.txt)"),
    )?;
    relink("/proc/version", &file_link)?;
    reruns_then_hits(
        &project,
        &command,
        &format!("`{read_file}` changed (link target changed from input.txt to /proc/version)"),
    )?;
    fs::remove_file(&file_link)?;
    fs::write(file_link, "replaced\n")?;
    reruns_then_hits(
        &project,
        &command,
        &format!("`{read_file}` changed (type changed)"),
    )?;
    relink("inputs", &dir_link)?;
    reruns_then_hits(
        &project,
        &command,
        &format!(
            "`{}` changed (link target changed from /proc to inputs)",
            dir_link.display()
        ),
    )?;
    assert!(!project.has_lock_file());
    Ok(())
}

/// The review's example: a command that reads `input` and then rewrites it. Its
/// result is not cached, without a warning, because the command itself changed
/// the file, and the next build runs it again with a reason that names `input` and
/// says that the command wrote it.
#[test]
fn a_command_that_rewrites_a_file_it_read_runs_again_without_a_warning() -> TestResult {
    let project = Project::new()?;
    project.write("input", "the old input\n")?;
    project.kizz_md("", &[(
        "Rewrite",
        "run(\"sh\", \"-c\", \"cat input > output; printf 'new\\\\n' > input\")",
    )])?;
    let command = "sh -c cat input > output; printf 'new\\n' > input";

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert!(
        first.stdout.starts_with(&format!(
            "{}\n",
            ran(command, "kizz.md:3 (Rewrite)", NEVER_RUN)
        )),
        "{first:?}"
    );
    assert_eq!(first.stderr, "", "{first:?}");
    assert_eq!(
        fs::read_to_string(project.root.join("output"))?,
        "the old input\n"
    );
    let records = project.step_records()?;
    let record = records.first().ok_or("no record")?;
    assert_eq!(
        record.not_cached,
        Some(NotCachedReason::ChangedAfterRead {
            files: vec![ChangedRead {
                path: project.root.join("input"),
                written: true,
            }],
        }),
        "{record:?}"
    );

    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert!(
        second.stdout.starts_with(&format!(
            "{}\n",
            ran(
                command,
                "kizz.md:3 (Rewrite)",
                "the previous run could not be cached: a file changed after the command read \
                 it: `input` (the command wrote it)"
            )
        )),
        "{second:?}"
    );
    assert_eq!(second.stderr, "", "{second:?}");
    Ok(())
}

/// Make a FIFO at `path`.
fn make_fifo(path: &Path) -> TestResult {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: `path` is a NUL-terminated string.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// A command that reads a file and then waits on a FIFO, which the test opens only
/// after it has rewritten that file, so the edit falls inside the run: the result
/// is not cached, and a loud warning, repeated in the summary, names the command
/// and the file and says that something else changed it while the command ran.
#[test]
fn a_file_changed_by_something_else_during_a_run_is_a_loud_warning() -> TestResult {
    let project = Project::new()?;
    project.write("input", "the old input\n")?;
    make_fifo(&project.root.join("fifo"))?;
    let script = "cat input > copy; echo started; cat fifo";
    project.kizz_md("", &[(
        "Wait",
        &format!("run(\"sh\", \"-c\", \"{script}\", stream = True)"),
    )])?;
    let (child, lines) = project.start_build()?;
    // The command has read `input`: edit it, then let the command finish.
    project.write("input", "edited while the command ran\n")?;
    {
        let mut fifo = fs::OpenOptions::new()
            .write(true)
            .open(project.root.join("fifo"))?;
        io::Write::write_all(&mut fifo, b"done\n")?;
    }
    let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{rest:?} {stderr}");
    let command = format!("sh -c {script}");
    let message = format!(
        "something else changed `input` while `{command}` in . ran, after the command read \
         it, so its result was not cached and it runs again on the next build"
    );
    assert_eq!(
        stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n"),
        "{rest:?}"
    );
    let records = project.step_records()?;
    let record = records.first().ok_or("no record")?;
    assert_eq!(
        record.not_cached,
        Some(NotCachedReason::ChangedAfterRead {
            files: vec![ChangedRead {
                path: project.root.join("input"),
                written: false,
            }],
        }),
        "{record:?}"
    );
    assert!(!project.has_lock_file());
    Ok(())
}

/// A command that connects to a loopback address, here `bash` through `/dev/tcp`
/// to a listener that the test opens, is cached, and the build warns once, naming
/// the command and the address, that kizz cannot see what came back, that the
/// result is still cached, and that `always = True` runs a command on every build;
/// the summary repeats the warning. The record lists the connection, the next build
/// is a cache hit without the warning, and `kizz why` lists the connection.
#[test]
fn a_command_that_connects_warns_and_is_still_cached() -> TestResult {
    let project = Project::new()?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");
    project.kizz_md("", &[(
        "Connect",
        &format!("run(\"bash\", \"-c\", \"{script}\", expect = [0])"),
    )])?;
    let command = format!("bash -c {script}");

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert!(
        first.stdout.starts_with(&format!(
            "{}\n",
            ran(&command, "kizz.md:3 (Connect)", NEVER_RUN)
        )),
        "{first:?}"
    );
    let message = format!(
        "`{command}` in . connected to 127.0.0.1:{port}; kizz cannot see what came back, and \
         its result is still cached; `always = True` runs a command on every build"
    );
    assert_eq!(
        first.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n"),
        "{first:?}"
    );
    let records = project.step_records()?;
    let [record] = records.as_slice() else {
        return Err(format!("expected one record, found {records:?}").into());
    };
    assert_eq!(record.not_cached, None, "{record:?}");
    assert_eq!(record.connections, [Connection::Ipv4 {
        address: [127, 0, 0, 1],
        port,
    }]);

    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert_eq!(second.stderr, "", "{second:?}");
    assert!(!second.stdout.contains("ran `"), "{second:?}");
    assert!(second.stdout.contains(", 1 cached"), "{second:?}");

    let why = project.run("", &["why"])?;
    assert_eq!(why.status, Some(0), "{why:?}");
    assert!(
        why.stdout
            .contains(&format!("\n    connections: 127.0.0.1:{port}\n")),
        "{why:?}"
    );
    drop(listener);
    Ok(())
}

/// With the real tracer, a step records every part of the machine state, with the
/// kernel release that the kernel reports; a step that names `kernel` is a cache hit
/// in the next build, since the kernel stayed the same; `kizz why` shows the part
/// that it names, and `kizz --verbose why` shows every part, one line for each.
#[test]
fn the_machine_state_is_recorded_and_shown_by_why() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Kernel",
        "run(\"true\", machine = [\"kernel\"], expect = [0])",
    )])?;
    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    let release = fs::read_to_string("/proc/sys/kernel/osrelease")?
        .trim_end()
        .to_owned();
    let records = project.step_records()?;
    let [record] = records.as_slice() else {
        return Err(format!("expected one record, found {records:?}").into());
    };
    assert_eq!(record.spec.machine, ["kernel"]);
    let machine = &record.machine;
    assert_eq!(
        machine
            .kernel
            .as_ref()
            .map(|kernel| kernel.release.as_str()),
        Some(release.as_str())
    );
    assert!(
        machine
            .limits
            .as_ref()
            .is_some_and(|limits| !limits.is_empty())
    );
    assert!(
        machine
            .affinity
            .as_ref()
            .is_some_and(|cpus| !cpus.is_empty())
    );
    assert!(machine.memory.is_some_and(|memory| memory.total > 0));
    assert!(machine.filesystem.is_some());
    assert!(machine.disk.is_some_and(|disk| disk.size > 0));

    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    assert!(!second.stdout.contains("ran `"), "{second:?}");
    assert!(second.stdout.contains(", 1 cached"), "{second:?}");

    let machine_lines = |stdout: &str| -> Vec<String> {
        stdout
            .lines()
            .filter(|line| line.starts_with("    machine "))
            .map(ToOwned::to_owned)
            .collect()
    };
    let why = project.run("", &["why"])?;
    assert_eq!(why.status, Some(0), "{why:?}");
    let shown = machine_lines(&why.stdout);
    let [kernel] = shown.as_slice() else {
        return Err(format!("expected one machine line, found {shown:?}").into());
    };
    assert!(
        kernel.starts_with(&format!(
            "    machine kernel: kernel release {release}, kernel version "
        )),
        "{why:?}"
    );

    let verbose = project.run("", &["--verbose", "why"])?;
    assert_eq!(verbose.status, Some(0), "{verbose:?}");
    let shown = machine_lines(&verbose.stdout);
    let starts = [
        format!("    machine kernel (checked): kernel release {release}, kernel version "),
        "    machine limits: soft limit of cpu ".to_owned(),
        "    machine affinity: CPUs ".to_owned(),
        "    machine memory: total memory ".to_owned(),
        "    machine filesystem: file system type 0x".to_owned(),
        "    machine disk: file system size ".to_owned(),
    ];
    assert_eq!(shown.len(), starts.len(), "{verbose:?}");
    for (line, start) in shown.iter().zip(&starts) {
        assert!(line.starts_with(start.as_str()), "{start:?} in {verbose:?}");
        assert!(!line.contains("unknown"), "{verbose:?}");
    }
    Ok(())
}

/// The warning `kizz build` prints for the step `run("false")` of the task `task`,
/// as warnings name it, which its program left unchecked; `cached` says that the
/// result came from the cache, and `fail` that `--fail-unchecked` was given.
fn unchecked_false(
    task: &str,
    cached: bool,
    fail: bool,
) -> String {
    format!(
        "{task} left `false` in . unchecked: exit code 1, but its `run()` has no `expect` and \
         the task program never read the result's `rc` or `signal`{}; {}",
        if cached {
            "; the result came from the cache"
        } else {
            ""
        },
        if fail {
            "the task fails because `kizz build --fail-unchecked` was given"
        } else {
            "`kizz build --fail-unchecked` makes this fail the task"
        }
    )
}

/// The warning `kizz build` prints for the step `run("false")` of the task on line
/// 3 of the root `kizz.md`, headed `Ignore`, which its program left unchecked;
/// `cached` says that the result came from the cache, and `fail` that
/// `--fail-unchecked` was given.
fn false_unchecked(
    cached: bool,
    fail: bool,
) -> String {
    unchecked_false("kizz.md:3 (Ignore)", cached, fail)
}

/// Only the results that stand when the build settles warn: `Use` runs `cat
/// out.txt` before `Make` writes `out.txt`, so `cat` fails unchecked in pass 1 and
/// succeeds in pass 2, and `Ignore` ignores `run("false")`. The build settles in two
/// passes, and its standard error is exactly the warning for `false`, without the
/// cache note, then the summary with it; the next build prints the same warning
/// with the cache note.
#[test]
fn only_the_unchecked_exits_that_stand_warn() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Use", "run(\"cat\", \"out.txt\")"),
        ("Make", "run(\"sh\", \"-c\", \"echo made > out.txt\")"),
        ("Ignore", "run(\"false\")"),
    ])?;

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    assert!(
        first
            .stdout
            .ends_with("build succeeded: 3 tasks, settled in 2 passes; 4 commands ran, 0 cached\n"),
        "{first:?}"
    );
    let message = unchecked_false("kizz.md:17 (Ignore)", false, false);
    assert_eq!(
        first.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n"),
        "{first:?}"
    );

    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    let message = unchecked_false("kizz.md:17 (Ignore)", true, false);
    assert_eq!(
        second.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n"),
        "{second:?}"
    );
    Ok(())
}

/// When a task that ignores `run("false")` runs before a task that calls `fail()`,
/// standard error holds the later task's failure, then the warning for `false`,
/// which comes after the last pass, then the `failed:` line, then the summary of
/// warnings, in that order.
#[test]
fn the_unchecked_warning_comes_after_the_last_pass() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Ignore", "run(\"false\")"),
        ("Stop", "fail(\"stopped on purpose\")"),
    ])?;
    let run = project.run("", &["build", "ci"])?;
    assert_eq!(run.status, Some(1), "{run:?}");
    let message = false_unchecked(false, false);
    let warning = format!("warning: {message}\n");
    let summary = format!("1 warning:\n  {message}\n");
    let parts = [
        "error: kizz.md:10 (Stop) failed:",
        warning.as_str(),
        "failed: kizz.md:10 (Stop)\n",
        summary.as_str(),
    ];
    let mut from = 0_usize;
    for part in parts {
        let found = run
            .stderr
            .get(from..)
            .and_then(|rest| rest.find(part))
            .ok_or_else(|| format!("{part:?} after byte {from} in {run:?}"))?;
        from = from.saturating_add(found).saturating_add(part.len());
    }
    assert!(run.stderr.ends_with(&summary), "{run:?}");
    assert_eq!(
        run.stderr.matches(&warning).count(),
        1,
        "the warning is printed once, then repeated in the summary: {run:?}"
    );
    Ok(())
}

/// A task that runs `false` and ignores the result builds with exit status 0, with
/// one loud warning that names the task, the command and how it ended, repeated in
/// the summary; the next build, whose result comes from the cache, warns again. The
/// same build with `--fail-unchecked` exits with status 1 and prints the command,
/// its exit status and its captured output, and so does a fresh build with it.
#[test]
fn an_unchecked_false_warns_and_fails_with_fail_unchecked() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[("Ignore", "run(\"false\")")])?;

    let first = project.run("", &["build", "ci"])?;
    assert_eq!(first.status, Some(0), "{first:?}");
    let message = false_unchecked(false, false);
    assert_eq!(
        first.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n"),
        "{first:?}"
    );
    assert!(
        first
            .stdout
            .ends_with("build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached\n"),
        "{first:?}"
    );

    let second = project.run("", &["build", "ci"])?;
    assert_eq!(second.status, Some(0), "{second:?}");
    let message = false_unchecked(true, false);
    assert_eq!(
        second.stderr,
        format!("warning: {message}\n1 warning:\n  {message}\n"),
        "{second:?}"
    );

    let failing = project.run("", &["build", "--fail-unchecked", "ci"])?;
    assert_eq!(failing.status, Some(1), "{failing:?}");
    let fresh = Project::new()?;
    fresh.kizz_md("", &[("Ignore", "run(\"false\")")])?;
    let fresh_run = fresh.run("", &["build", "--fail-unchecked", "ci"])?;
    assert_eq!(fresh_run.status, Some(1), "{fresh_run:?}");
    for (run, cached) in [(&failing, true), (&fresh_run, false)] {
        let message = false_unchecked(cached, true);
        for part in [
            "error: kizz.md:3 (Ignore) failed:\n  the task program left a non-zero exit \
             unchecked, and `kizz build --fail-unchecked` makes that fail the task\n  command \
             `false`: exit code 1, which the task program never checked: its `run()` has no \
             `expect`, and the program never read the result's `rc` or `signal`\n  --- stdout \
             ---\n  --- stderr ---\n"
                .to_owned(),
            format!("warning: {message}\n"),
            "failed: kizz.md:3 (Ignore)\n".to_owned(),
            format!("1 warning:\n  {message}\n"),
        ] {
            assert!(run.stderr.contains(&part), "{part:?} in {run:?}");
        }
        assert!(
            run.stdout
                .contains("build failed: 1 task, 1 failed, settled in 1 pass; "),
            "{run:?}"
        );
    }
    Ok(())
}

/// With `--fail-unchecked`, the failure of a task that leaves a command's non-zero
/// exit unchecked prints the command's captured output.
#[test]
fn fail_unchecked_prints_the_captured_output() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Noisy",
        "run(\"sh\", \"-c\", \"echo printed; echo complained >&2; exit 3\")",
    )])?;
    let run = project.run("", &["build", "--fail-unchecked", "ci"])?;
    assert_eq!(run.status, Some(1), "{run:?}");
    assert!(
        run.stderr.contains(
            "  command `sh -c echo printed; echo complained >&2; exit 3`: exit code 3, which the \
             task program never checked"
        ),
        "{run:?}"
    );
    assert!(
        run.stderr
            .contains("  --- stdout ---\n  printed\n  --- stderr ---\n  complained\n"),
        "{run:?}"
    );
    Ok(())
}

/// `run("false", expect = [1])` accepts the exit, a task that reads `rc` checks
/// it, and a task that writes `json.encode(result)` with `fs.write`, or the result
/// with `fs.write_json`, encodes it, which checks it: none warns, and
/// `--fail-unchecked` fails none. The files hold the result's attributes.
#[test]
fn checked_exits_warn_about_nothing() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Expect", "run(\"false\", expect = [1])"),
        (
            "Read",
            "if run(\"false\", \"read\").rc != 1:\n    fail(\"unexpected\")",
        ),
        (
            "Encode",
            "fs.write(\"encoded.json\", json.encode(run(\"false\", \"encode\")))",
        ),
        (
            "Write",
            "fs.write_json(\"written.json\", run(\"false\", \"write\"))",
        ),
    ])?;
    for args in [
        &["build", "ci"][..],
        &["build", "--fail-unchecked", "ci"][..],
    ] {
        let run = project.run("", args)?;
        assert_eq!(run.status, Some(0), "{args:?}: {run:?}");
        assert_eq!(run.stderr, "", "{args:?}: {run:?}");
    }
    assert_eq!(
        fs::read_to_string(project.root.join("encoded.json"))?,
        r#"{"rc":1,"signal":null,"stdout":"","stderr":"","cached":true}"#
    );
    let written = fs::read_to_string(project.root.join("written.json"))?;
    assert!(written.contains("\"rc\": 1"), "{written}");
    assert!(written.contains("\"signal\": null"), "{written}");
    Ok(())
}
