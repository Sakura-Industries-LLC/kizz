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

use std::{
    error::Error,
    fs,
    io::{self, BufRead as _, BufReader, Lines},
    os::unix::{
        fs::{PermissionsExt as _, symlink},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
};

use kizz_store::{LOCK_FILE_NAME, NotCachedReason, PathState, StepRecord, Store};
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
/// the tracer cannot decode.
#[test]
fn helper_process() {
    if std::env::var_os(HELPER_VARIABLE).is_some_and(|mode| mode == "x32") {
        // SAFETY: `getpid` takes no arguments; the kernel either runs it or rejects
        // the x32 number with `ENOSYS`.
        let ret = unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
        eprintln!("x32 getpid returned {ret}");
    }
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
