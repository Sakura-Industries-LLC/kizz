// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of `kizz cache clean` and `kizz cache reset`, which run the
//! built binary on caches that real builds fill.
//!
//! Each test writes its projects into one temporary directory and keeps its cache
//! in another. Every run of `kizz` gets an explicit environment that holds only
//! `KIZZ_CACHE_DIR`, set to that cache, and `PATH`. The tests read the cache with
//! `kizz_store` before and after each cache command, and compare every project file
//! before and after it, since cache commands never change project files.
//!
//! A reset asks for confirmation only on a terminal, so the tests that answer it run
//! `kizz` on a pseudo-terminal that they open with `posix_openpt`: standard input,
//! output, and error are its terminal side, and the test reads the output and types
//! the answer on the other side.

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::{CStr, OsStr},
    fs::{self, File, OpenOptions},
    io::{self, BufRead as _, BufReader, Lines, Read as _, Write as _},
    os::{
        fd::{AsRawFd as _, FromRawFd as _},
        unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _, process::CommandExt as _},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    slice,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, SystemTime},
};

use kizz_store::{
    CacheIdentity, DATABASE_DIR, IdentityKey, IdentityPart, LOCK_FILE_NAME, LastWriter,
    LearnedOrder, PathKey, PositionKey, SCHEMA_VERSION, SchemaKey, SchemaVersion, StepKey,
    StepPosition, StepRecord, Store, StoreError, TaskId, TaskRecord, identity_differences_text,
};
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The global options of every run: no color and no banner.
const PLAIN: [&str; 3] = ["--color", "never", "--no-banner"];

/// The `PATH` every run of `kizz` gets, and its tasks inherit.
const PATH: &str = "/usr/bin:/bin";

/// The question a reset asks before it removes anything.
const QUESTION: &str = "Remove these entries? [y/N] ";

/// The error of a reset without a terminal and without `--yes`.
const NO_TERMINAL: &str = "error: refusing to reset the cache without a terminal to ask for \
                           confirmation; pass --yes to reset without asking";

/// How long a test waits for output from a run on the pseudo-terminal.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A command that waits until the file `go` exists, after printing `started`.
const HOLD: &str = "run(\"sh\", \"-c\", \"echo started; i=0; while [ ! -e go ] && [ $i -lt 600 \
                    ]; do sleep 0.1; i=$((i + 1)); done\", stream = True)";

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

/// Every entry of a cache, family by family, in key order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Entries {
    /// Step records.
    steps: Vec<(StepKey, StepRecord)>,
    /// Step positions.
    positions: Vec<(PositionKey, StepPosition)>,
    /// Task records.
    tasks: Vec<(TaskId, TaskRecord)>,
    /// Learned order.
    orders: Vec<(TaskId, LearnedOrder)>,
    /// Last writers.
    writers: Vec<(PathKey, LastWriter)>,
}

impl Entries {
    /// Every entry of the cache in `cache`, with the database closed again.
    fn read(cache: &Path) -> TestResult<Self> {
        let store = Store::open(cache)?;
        let entries = {
            let txn = store.read()?;
            Self {
                steps: txn.scan(&[])?,
                positions: txn.scan(&[])?,
                tasks: txn.scan(&[])?,
                orders: txn.scan(&[])?,
                writers: txn.scan(&[])?,
            }
        };
        store.close()?;
        Ok(entries)
    }

    /// The entries that `inside` selects: step records that have owners and whose
    /// owners' `kizz.md` it all selects, never by their working directory; last
    /// writers by their path; and the others by their task's `kizz.md`.
    fn select(
        &self,
        inside: impl Fn(&Path) -> bool,
    ) -> Self {
        Self {
            steps: self
                .steps
                .iter()
                .filter(|(_, record)| {
                    !record.owners.is_empty()
                        && record
                            .owners
                            .iter()
                            .all(|owner| inside(owner.file.as_path()))
                })
                .cloned()
                .collect(),
            positions: self
                .positions
                .iter()
                .filter(|(key, _)| inside(key.task.file.as_path()))
                .cloned()
                .collect(),
            tasks: self
                .tasks
                .iter()
                .filter(|(id, _)| inside(id.file.as_path()))
                .cloned()
                .collect(),
            orders: self
                .orders
                .iter()
                .filter(|(id, _)| inside(id.file.as_path()))
                .cloned()
                .collect(),
            writers: self
                .writers
                .iter()
                .filter(|(key, _)| inside(key.0.as_path()))
                .cloned()
                .collect(),
        }
    }

    /// The entries for the tasks and paths under `dir`, which a reset of `dir`
    /// removes: step records only when every owner is a task under it.
    fn under(
        &self,
        dir: &Path,
    ) -> Self {
        self.select(|path| path.starts_with(dir))
    }

    /// The entries for the tasks and paths outside `dir`: step records only when
    /// every owner is a task outside it.
    fn outside(
        &self,
        dir: &Path,
    ) -> Self {
        self.select(|path| !path.starts_with(dir))
    }

    /// The step records that a reset of `dir` keeps but takes the tasks under it
    /// out of: those with owners both under it and outside it.
    fn shared(
        &self,
        dir: &Path,
    ) -> Vec<(StepKey, StepRecord)> {
        self.steps
            .iter()
            .filter(|(_, record)| {
                record
                    .owners
                    .iter()
                    .any(|owner| owner.file.starts_with(dir))
                    && record
                        .owners
                        .iter()
                        .any(|owner| !owner.file.starts_with(dir))
            })
            .cloned()
            .collect()
    }

    /// The entries that remain after a reset of `dir`: every step record that it
    /// does not remove, without the tasks under `dir` among its owners, and the other
    /// entries for tasks and paths outside it.
    fn after_reset(
        &self,
        dir: &Path,
    ) -> Self {
        let removed = self.under(dir);
        Self {
            steps: self
                .steps
                .iter()
                .filter(|entry| !removed.steps.contains(entry))
                .cloned()
                .map(|(key, mut record)| {
                    record.owners.retain(|owner| !owner.file.starts_with(dir));
                    (key, record)
                })
                .collect(),
            ..self.select(|path| !path.starts_with(dir))
        }
    }

    /// What a reset of `dir` says it removes, after the colon of its description,
    /// and what it says it removed, after `removed `: the tasks under `dir` that
    /// have entries or own step records, the entries it removes, and the shared step
    /// records it keeps.
    fn reset_counts(
        &self,
        dir: &Path,
    ) -> (String, String) {
        let removed = self.under(dir);
        let shared = self.shared(dir);
        let mut tasks: Vec<&TaskId> = removed
            .tasks
            .iter()
            .map(|(id, _)| id)
            .chain(removed.orders.iter().map(|(id, _)| id))
            .chain(removed.positions.iter().map(|(key, _)| &key.task))
            .chain(removed.steps.iter().flat_map(|(_, record)| &record.owners))
            .chain(
                shared
                    .iter()
                    .flat_map(|(_, record)| &record.owners)
                    .filter(|owner| owner.file.starts_with(dir)),
            )
            .collect();
        tasks.sort();
        tasks.dedup();
        let kept = if shared.is_empty() {
            String::new()
        } else {
            format!(
                "; {} {} shared with tasks outside it, kept without the tasks under it",
                shared.len(),
                plural(shared.len(), "step record", "step records")
            )
        };
        (
            format!(
                "{} {}; {}{kept}",
                tasks.len(),
                plural(tasks.len(), "task", "tasks"),
                removed.counts()
            ),
            format!("{}{kept}", removed.counts()),
        )
    }

    /// Whether there is no entry at all.
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// The number of tasks and entries, as a reset describes them.
    fn description(&self) -> String {
        let mut tasks: Vec<&TaskId> = self
            .tasks
            .iter()
            .map(|(id, _)| id)
            .chain(self.orders.iter().map(|(id, _)| id))
            .chain(self.positions.iter().map(|(key, _)| &key.task))
            .collect();
        tasks.sort();
        tasks.dedup();
        format!(
            "{} {}; {}",
            tasks.len(),
            plural(tasks.len(), "task", "tasks"),
            self.counts()
        )
    }

    /// The number of entries of each family, as the cache commands report them.
    fn counts(&self) -> String {
        format!(
            "{} {}, {} {}, {} {}, {} {}, {} {}",
            self.steps.len(),
            plural(self.steps.len(), "step record", "step records"),
            self.positions.len(),
            plural(self.positions.len(), "step position", "step positions"),
            self.tasks.len(),
            plural(self.tasks.len(), "task record", "task records"),
            self.orders.len(),
            plural(
                self.orders.len(),
                "learned-order entry",
                "learned-order entries"
            ),
            self.writers.len(),
            plural(
                self.writers.len(),
                "last-writer entry",
                "last-writer entries"
            ),
        )
    }
}

/// `singular` when `number` is 1, and `plural` otherwise.
fn plural<'w>(
    number: usize,
    singular: &'w str,
    plural: &'w str,
) -> &'w str {
    if number == 1 { singular } else { plural }
}

/// Projects and a cache, each in a temporary directory.
struct Project {
    /// The directory of the projects.
    _dir: TempDir,
    /// The directory of the projects, with every symbolic link resolved, as `kizz`
    /// sees it.
    root: PathBuf,
    /// The cache.
    cache: TempDir,
}

impl Project {
    /// An empty directory of projects and an empty cache.
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        Ok(Self {
            _dir: dir,
            root,
            cache: tempfile::tempdir()?,
        })
    }

    /// The path of `relative` in the projects' directory.
    fn path(
        &self,
        relative: &str,
    ) -> PathBuf {
        if relative.is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative)
        }
    }

    /// Write `content` to `relative`, creating parent directories.
    fn write(
        &self,
        relative: &str,
        content: &str,
    ) -> TestResult {
        let path = self.path(relative);
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
    /// directory `dir`, with the cache, `PATH`, and no other environment, and with
    /// no standard input.
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
            .current_dir(self.path(dir))
            .stdin(Stdio::null());
        command
    }

    /// Run `kizz` with `args` in the directory `dir`, without a terminal.
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

    /// Run `kizz build ci` in the directory `dir`, which must succeed.
    fn build(
        &self,
        dir: &str,
    ) -> TestResult {
        let run = self.run(dir, &["build", "ci"])?;
        assert_eq!(run.status, Some(0), "{run:?}");
        Ok(())
    }

    /// Every entry of the cache.
    fn entries(&self) -> TestResult<Entries> {
        Entries::read(self.cache.path())
    }

    /// Every file, directory, and symbolic link in the projects' directory, with
    /// its content or link target and its modification time.
    fn files(&self) -> TestResult<BTreeMap<PathBuf, (Vec<u8>, SystemTime)>> {
        let mut files = BTreeMap::new();
        let mut pending = vec![self.root.clone()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                let meta = fs::symlink_metadata(&path)?;
                let content = if meta.is_dir() {
                    pending.push(path.clone());
                    Vec::new()
                } else if meta.is_symlink() {
                    fs::read_link(&path)?.as_os_str().as_bytes().to_vec()
                } else {
                    fs::read(&path)?
                };
                files.insert(path, (content, meta.modified()?));
            }
        }
        Ok(files)
    }

    /// Start `kizz build ci` in the directory `dir`, with piped output, and wait
    /// until its task's command prints `started`, which must be the first line of
    /// output. Returns the process and the rest of its standard output, which the
    /// caller keeps reading, so `kizz` can still write to it.
    fn start_build(
        &self,
        dir: &str,
    ) -> TestResult<(Child, Lines<BufReader<ChildStdout>>)> {
        let mut child = self
            .command(dir, &["build", "ci"])
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
}

/// Opens a pseudo-terminal and returns its controlling side and its terminal side.
fn open_pty() -> TestResult<(File, File)> {
    // SAFETY: `posix_openpt` takes only flags, and returns a new descriptor or -1.
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `fd` is a new open descriptor that nothing else owns.
    let controller = unsafe { File::from_raw_fd(fd) };
    // SAFETY: both calls take an open pseudo-terminal descriptor and nothing else.
    if unsafe { libc::grantpt(controller.as_raw_fd()) } != 0
        || unsafe { libc::unlockpt(controller.as_raw_fd()) } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let mut name: [libc::c_char; 256] = [0; 256];
    // SAFETY: the buffer is writable for the length passed.
    let error = unsafe { libc::ptsname_r(controller.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error).into());
    }
    // SAFETY: on success `ptsname_r` wrote a string ended by a zero byte into the
    // buffer, which outlives the borrow.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) };
    let terminal = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(OsStr::from_bytes(name.to_bytes()))?;
    Ok((controller, terminal))
}

/// A run of `kizz` on a pseudo-terminal.
struct Session {
    /// The process.
    child: Child,
    /// The controlling side of the pseudo-terminal, where the test types.
    controller: File,
    /// The chunks of output that a reader thread reads from the controlling side.
    chunks: Receiver<Vec<u8>>,
    /// The output so far.
    output: Vec<u8>,
}

impl Session {
    /// Start `command` with a pseudo-terminal as its standard input, output, and
    /// error.
    fn start(mut command: Command) -> TestResult<Self> {
        let (controller, terminal) = open_pty()?;
        command
            .stdin(terminal.try_clone()?)
            .stdout(terminal.try_clone()?)
            .stderr(terminal);
        let child = command.spawn()?;
        // Close this process's copies of the terminal side, so reading ends when
        // `kizz` exits.
        drop(command);
        let (sender, chunks) = mpsc::channel();
        let mut reader = controller.try_clone()?;
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            // Reading fails with `EIO` once no process has the terminal side open.
            while let Ok(read @ 1..) = reader.read(&mut buffer) {
                let Some(chunk) = buffer.get(..read) else {
                    break;
                };
                if sender.send(chunk.to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            controller,
            chunks,
            output: Vec::new(),
        })
    }

    /// The output so far, with the terminal's line endings made plain.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).replace("\r\n", "\n")
    }

    /// Wait until the output contains `marker`.
    fn wait_for(
        &mut self,
        marker: &str,
    ) -> TestResult {
        while !self.text().contains(marker) {
            match self.chunks.recv_timeout(TIMEOUT) {
                Ok(chunk) => self.output.extend(chunk),
                Err(error) => {
                    return Err(format!("no {marker:?} ({error}): {}", self.text()).into());
                },
            }
        }
        Ok(())
    }

    /// Type `line` and a line break.
    fn answer(
        &mut self,
        line: &str,
    ) -> TestResult {
        self.controller.write_all(format!("{line}\n").as_bytes())?;
        Ok(())
    }

    /// Wait for `kizz` to exit and for the rest of its output; return its exit code
    /// and its whole output.
    fn finish(mut self) -> TestResult<(Option<i32>, String)> {
        let status = self.child.wait()?;
        loop {
            match self.chunks.recv_timeout(TIMEOUT) {
                Ok(chunk) => self.output.extend(chunk),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("the output did not end: {}", self.text()).into());
                },
            }
        }
        Ok((status.code(), self.text()))
    }
}

/// After a project directory is deleted, `kizz cache clean` removes its entries,
/// without asking, and keeps every other project's entries and files.
#[test]
fn clean_removes_a_deleted_projects_entries_and_keeps_the_others() -> TestResult {
    let project = Project::new()?;
    for dir in ["one", "two"] {
        project.kizz_md(dir, &[
            ("Use", "run(\"cat\", \"out.txt\")"),
            ("Make", "run(\"sh\", \"-c\", \"echo made > out.txt\")"),
        ])?;
        project.build(dir)?;
    }
    let one = project.path("one");
    let two = project.path("two");
    let before = project.entries()?;
    let deleted = before.under(&one);
    assert_eq!(deleted.tasks.len(), 2, "{deleted:#?}");
    assert_eq!(deleted.positions.len(), 2, "{deleted:#?}");
    assert_eq!(deleted.orders.len(), 1, "{deleted:#?}");
    assert_eq!(deleted.steps.len(), 2, "{deleted:#?}");
    assert!(!deleted.writers.is_empty(), "{deleted:#?}");
    assert_eq!(before.under(&two).tasks.len(), 2, "{before:#?}");

    fs::remove_dir_all(&one)?;
    let files = project.files()?;
    let run = project.run("", &["cache", "clean"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(run.stderr, "", "{run:?}");
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{run:?}");
    assert!(
        lines
            .first()
            .is_some_and(|line| line.starts_with("checked ")),
        "{run:?}"
    );
    assert_eq!(
        lines.get(1).copied(),
        Some(format!("removed {}", deleted.counts()).as_str()),
        "{run:?}"
    );
    assert_eq!(project.entries()?, before.outside(&one));
    assert_eq!(project.files()?, files);
    assert!(!project.cache.path().join(LOCK_FILE_NAME).exists());

    // A second clean finds nothing more to remove.
    let again = project.run("", &["cache", "clean"])?;
    assert_eq!(again.status, Some(0), "{again:?}");
    assert!(
        again
            .stdout
            .ends_with("nothing to remove: no cache entry is about a file that no longer exists\n"),
        "{again:?}"
    );
    assert_eq!(project.entries()?, before.outside(&one));
    Ok(())
}

/// `kizz cache clean` removes a step that depends on a file that has since been
/// deleted, as a whole record, and keeps a step that depends on a file that never
/// existed, with the task records and positions of their existing `kizz.md`.
#[test]
fn clean_removes_steps_on_deleted_files_and_keeps_steps_on_files_that_never_existed() -> TestResult
{
    let project = Project::new()?;
    project.write("input.txt", "input\n")?;
    project.kizz_md("", &[
        ("Read", "run(\"cat\", \"input.txt\")"),
        ("Probe", "run(\"test\", \"-e\", \"never.txt\")"),
    ])?;
    project.build("")?;
    let before = project.entries()?;
    let step = |argv: &[&str]| {
        before
            .steps
            .iter()
            .find(|(_, record)| record.spec.argv == argv)
            .cloned()
            .ok_or_else(|| format!("no step {argv:?}: {before:#?}"))
    };
    let read = step(&["cat", "input.txt"])?;
    let probe = step(&["test", "-e", "never.txt"])?;
    assert!(
        probe.1.observations.iter().any(|observation| {
            observation.path == project.path("never.txt")
                && observation.state == kizz_store::PathState::Absent
        }),
        "{probe:#?}"
    );

    fs::remove_file(project.path("input.txt"))?;
    let files = project.files()?;
    let run = project.run("", &["cache", "clean"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert!(
        run.stdout.ends_with(
            "removed 1 step record, 0 step positions, 0 task records, 0 learned-order entries, \
             0 last-writer entries\n"
        ),
        "{run:?}"
    );
    let after = project.entries()?;
    let expected = Entries {
        steps: before
            .steps
            .iter()
            .filter(|(key, _)| *key != read.0)
            .cloned()
            .collect(),
        ..before.clone()
    };
    assert_eq!(after, expected);
    assert!(after.steps.contains(&probe));
    assert_eq!(project.files()?, files);

    // The step on the deleted file runs again, and the other one stays cached.
    let rebuild = project.run("", &["--verbose", "build", "ci"])?;
    assert_eq!(rebuild.status, Some(0), "{rebuild:?}");
    assert!(
        rebuild
            .stdout
            .starts_with("ran `cat input.txt` in kizz.md:3 (Read) step 1: never run"),
        "{rebuild:?}"
    );
    assert!(
        rebuild
            .stdout
            .contains("cached `test -e never.txt` in kizz.md:10 (Probe) step 1\n"),
        "{rebuild:?}"
    );
    Ok(())
}

/// `kizz cache reset` in a directory shows what it will remove, asks, and only
/// after confirmation removes the entries for the tasks under it, and nothing else.
#[test]
fn reset_in_a_directory_removes_only_its_tasks_entries_after_confirmation() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[("Top", "run(\"cat\", \"sub/sub.txt\")")])?;
    project.kizz_md("sub", &[(
        "Sub",
        "run(\"sh\", \"-c\", \"echo sub > sub.txt\")",
    )])?;
    project.kizz_md("other", &[(
        "Other",
        "run(\"sh\", \"-c\", \"echo other > other.txt\")",
    )])?;
    project.build("")?;
    let sub = project.path("sub");
    let before = project.entries()?;
    let removed = before.under(&sub);
    assert_eq!(removed.tasks.len(), 1, "{before:#?}");
    assert!(!removed.steps.is_empty() && !removed.writers.is_empty());
    assert_eq!(before.under(&project.path("other")).tasks.len(), 1);
    let files = project.files()?;
    let (description, counts) = before.reset_counts(&sub);

    let mut session = Session::start(project.command("sub", &["cache", "reset"]))?;
    session.wait_for(QUESTION)?;
    session.answer("y")?;
    let (status, output) = session.finish()?;
    assert_eq!(status, Some(0), "{output}");
    assert_eq!(
        output,
        format!(
            "kizz cache reset wipes every entry for the tasks under `{}`: {}\n{QUESTION}y\n\
             removed {}\n",
            sub.display(),
            description,
            counts
        )
    );
    assert_eq!(project.entries()?, before.after_reset(&sub));
    assert_eq!(project.files()?, files);
    assert!(!project.cache.path().join(LOCK_FILE_NAME).exists());
    Ok(())
}

/// Declining the confirmation changes nothing, and `kizz cache reset --all` empties
/// the cache after confirmation.
#[test]
fn declining_changes_nothing_and_reset_all_empties_the_cache_after_confirmation() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[
        ("Use", "run(\"cat\", \"out.txt\")"),
        ("Make", "run(\"sh\", \"-c\", \"echo made > out.txt\")"),
    ])?;
    project.build("")?;
    let before = project.entries()?;
    assert!(!before.is_empty());
    let files = project.files()?;

    for (args, answer) in [
        (&["cache", "reset"][..], "n"),
        (&["cache", "reset"][..], ""),
        (&["cache", "reset", "--all"][..], "no"),
    ] {
        let mut session = Session::start(project.command("", args))?;
        session.wait_for(QUESTION)?;
        session.answer(answer)?;
        let (status, output) = session.finish()?;
        assert_eq!(status, Some(0), "{args:?}: {output}");
        assert!(
            output.ends_with(&format!(
                "{QUESTION}{answer}\nreset declined; nothing was removed\n"
            )),
            "{args:?}: {output}"
        );
        assert_eq!(project.entries()?, before, "{args:?}");
    }

    let mut session = Session::start(project.command("", &["cache", "reset", "--all"]))?;
    session.wait_for(QUESTION)?;
    session.answer("yes")?;
    let (status, output) = session.finish()?;
    assert_eq!(status, Some(0), "{output}");
    assert_eq!(
        output,
        format!(
            "kizz cache reset --all wipes the whole cache at `{}`: {}\n{QUESTION}yes\nwiped the \
             whole cache: removed {}\n",
            project.cache.path().display(),
            before.description(),
            before.counts()
        )
    );
    assert!(project.entries()?.is_empty());
    assert_eq!(project.files()?, files);

    // An empty cache has nothing to remove, so nothing is asked.
    let empty = Session::start(project.command("", &["cache", "reset", "--all"]))?;
    let (status, output) = empty.finish()?;
    assert_eq!(status, Some(0), "{output}");
    assert!(
        output.ends_with("is empty; nothing to remove\n"),
        "{output}"
    );
    Ok(())
}

/// `kizz cache clean` never asks for confirmation, even on a terminal, and
/// `--yes` skips a reset's question there.
#[test]
fn clean_never_asks_and_yes_skips_the_question() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("a", &[("A", "run(\"sh\", \"-c\", \"echo a > a.txt\")")])?;
    project.kizz_md("b", &[("B", "run(\"sh\", \"-c\", \"echo b > b.txt\")")])?;
    project.build("")?;
    fs::remove_file(project.path("a/a.txt"))?;
    let before = project.entries()?;
    let files = project.files()?;

    let clean = Session::start(project.command("", &["cache", "clean"]))?;
    let (status, output) = clean.finish()?;
    assert_eq!(status, Some(0), "{output}");
    assert!(!output.contains("[y/N]"), "{output}");
    assert!(output.contains("\nremoved 1 step record, "), "{output}");
    let cleaned = project.entries()?;
    assert_eq!(cleaned.steps.len(), before.steps.len().saturating_sub(1));
    assert_eq!(
        cleaned.outside(&project.path("a")),
        before.outside(&project.path("a"))
    );

    let reset = Session::start(project.command("b", &["cache", "reset", "--yes"]))?;
    let (status, output) = reset.finish()?;
    assert_eq!(status, Some(0), "{output}");
    assert!(!output.contains("[y/N]"), "{output}");
    assert!(output.contains("\nremoved 1 step record, "), "{output}");
    assert_eq!(project.entries()?, cleaned.after_reset(&project.path("b")));

    let all = Session::start(project.command("", &["cache", "reset", "--all", "--yes"]))?;
    let (status, output) = all.finish()?;
    assert_eq!(status, Some(0), "{output}");
    assert!(!output.contains("[y/N]"), "{output}");
    assert!(project.entries()?.is_empty());
    assert_eq!(project.files()?, files);
    Ok(())
}

/// Without a terminal, a reset refuses to run unless `--yes` is given, and `--yes`
/// skips the confirmation.
#[test]
fn without_a_terminal_a_reset_refuses_unless_yes_is_given() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Top",
        "run(\"sh\", \"-c\", \"echo top > top.txt\")",
    )])?;
    project.kizz_md("sub", &[(
        "Sub",
        "run(\"sh\", \"-c\", \"echo sub > sub.txt\")",
    )])?;
    project.build("")?;
    let sub = project.path("sub");
    let before = project.entries()?;
    let files = project.files()?;

    for args in [&["cache", "reset"][..], &["cache", "reset", "--all"][..]] {
        let run = project.run("sub", args)?;
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert_eq!(run.stderr, format!("{NO_TERMINAL}\n"), "{args:?}: {run:?}");
        assert!(!run.stdout.contains("removed"), "{args:?}: {run:?}");
        assert_eq!(project.entries()?, before, "{args:?}");
    }

    let reset = project.run("sub", &["cache", "reset", "--yes"])?;
    assert_eq!(reset.status, Some(0), "{reset:?}");
    assert_eq!(reset.stderr, "", "{reset:?}");
    assert!(!reset.stdout.contains("[y/N]"), "{reset:?}");
    assert_eq!(project.entries()?, before.after_reset(&sub));

    let all = project.run("sub", &["cache", "reset", "--all", "--yes"])?;
    assert_eq!(all.status, Some(0), "{all:?}");
    assert_eq!(all.stderr, "", "{all:?}");
    assert!(project.entries()?.is_empty());
    assert_eq!(project.files()?, files);
    Ok(())
}

/// `kizz cache reset --all` also empties a cache whose schema version differs,
/// which the other cache commands refuse to open.
#[test]
fn reset_all_empties_a_cache_with_another_schema_version() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Make",
        "run(\"sh\", \"-c\", \"echo made > out.txt\")",
    )])?;
    project.build("")?;
    let before = project.entries()?;
    let other = SCHEMA_VERSION
        .checked_add(1)
        .ok_or("no next schema version")?;
    {
        let store = Store::open(project.cache.path())?;
        let mut txn = store.write()?;
        txn.put(&SchemaKey, &SchemaVersion { version: other })?;
        txn.commit()?;
        store.close()?;
    }
    assert!(matches!(
        Store::open(project.cache.path()),
        Err(StoreError::SchemaMismatch { found, .. }) if found == other
    ));

    for args in [&["cache", "clean"][..], &["cache", "reset", "--yes"][..]] {
        let run = project.run("", args)?;
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert!(
            run.stderr.contains(&format!(
                "has schema version {other}, but this kizz uses schema version {SCHEMA_VERSION}; \
                 run `kizz cache reset --all` to wipe the cache"
            )),
            "{args:?}: {run:?}"
        );
    }

    let run = project.run("", &["cache", "reset", "--all", "--yes"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        format!(
            "kizz cache reset --all wipes the whole cache at `{}`: {}\nthe cache has schema \
             version {other}, and this kizz uses schema version {SCHEMA_VERSION}\nwiped the whole \
             cache: removed {}\n",
            project.cache.path().display(),
            before.description(),
            before.counts()
        )
    );
    assert!(project.entries()?.is_empty());
    Ok(())
}

/// A cache stamped with another identity is refused by `kizz build`, `kizz why`,
/// `kizz cache clean` and `kizz cache reset PATH`, which exit with status 2, print
/// the error, and change nothing; `kizz cache reset --all` then names what differs
/// and wipes the cache, and the next build succeeds and stamps the current identity.
#[test]
fn a_cache_of_another_identity_is_refused_until_reset_all() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("sub", &[(
        "Make",
        "run(\"sh\", \"-c\", \"echo made > out.txt\")",
    )])?;
    project.build("sub")?;
    let before = project.entries()?;
    let current = CacheIdentity::current();
    let mut other = current.clone();
    other.host.push_str(".elsewhere");
    other.machine = String::from("other-architecture");
    {
        let store = Store::open(project.cache.path())?;
        let mut txn = store.write()?;
        txn.put(&IdentityKey, &other)?;
        txn.commit()?;
        store.close()?;
    }
    let differences = other.differences(&current);
    assert_eq!(
        differences
            .iter()
            .map(|difference| difference.part)
            .collect::<Vec<_>>(),
        vec![IdentityPart::Host, IdentityPart::Machine]
    );
    let error = format!(
        "error: the cache database at `{}` belongs to another machine or user, so kizz \
         refuses to use it: {}; the cache is not portable between machines and users; run \
         `kizz cache reset --all` to wipe the cache, or set `KIZZ_CACHE_DIR` to use another \
         cache\n",
        project.cache.path().join(DATABASE_DIR).display(),
        identity_differences_text(&differences)
    );
    let inventory = Store::inventory(project.cache.path())?;
    assert_eq!(inventory.identity.as_ref(), Some(&other));
    let files = project.files()?;

    for (dir, args) in [
        ("sub", &["build", "ci"][..]),
        ("sub", &["why"][..]),
        ("", &["cache", "clean"][..]),
        ("", &["cache", "reset", "sub", "--yes"][..]),
    ] {
        let run = project.run(dir, args)?;
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert!(run.stderr.contains(&error), "{args:?}: {run:?}");
        assert!(
            run.stderr
                .contains("; machine architecture: \"other-architecture\" in the cache, \""),
            "{args:?}: {run:?}"
        );
        assert_eq!(
            Store::inventory(project.cache.path())?,
            inventory,
            "{args:?}"
        );
        assert_eq!(project.files()?, files, "{args:?}");
    }

    let run = project.run("", &["cache", "reset", "--all", "--yes"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        format!(
            "kizz cache reset --all wipes the whole cache at `{}`: {}\nthe cache belongs to \
             another machine or user: {}\nwiped the whole cache: removed {}\n",
            project.cache.path().display(),
            before.description(),
            identity_differences_text(&differences),
            before.counts()
        )
    );
    assert!(
        run.stdout
            .contains("; machine architecture: \"other-architecture\" in the cache, \""),
        "{run:?}"
    );
    assert_eq!(
        Store::inventory(project.cache.path())?,
        kizz_store::Inventory::default()
    );

    project.build("sub")?;
    assert_eq!(
        Store::inventory(project.cache.path())?.identity,
        Some(current)
    );
    assert!(!project.entries()?.is_empty());
    Ok(())
}

/// A cache of this schema version whose identity was removed is refused like one of
/// another identity, not adopted: `kizz build`, `kizz why`, `kizz cache clean` and
/// `kizz cache reset PATH` exit with status 2, print the missing-identity error, and
/// change nothing; `kizz cache reset --all` then says that the identity is missing
/// and wipes the cache, and the next build succeeds and stamps the current identity.
#[test]
fn a_cache_without_an_identity_is_refused_until_reset_all() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("sub", &[(
        "Make",
        "run(\"sh\", \"-c\", \"echo made > out.txt\")",
    )])?;
    project.build("sub")?;
    let before = project.entries()?;
    assert!(!before.is_empty());
    {
        let store = Store::open(project.cache.path())?;
        let mut txn = store.write()?;
        txn.delete::<CacheIdentity>(&IdentityKey)?;
        txn.commit()?;
        store.close()?;
    }
    let error = format!(
        "error: the cache database at `{}` has no cache identity, so kizz cannot tell which \
         machine and user it belongs to and refuses to use it; the cache is not portable \
         between machines and users; run `kizz cache reset --all` to wipe the cache, or set \
         `KIZZ_CACHE_DIR` to use another cache\n",
        project.cache.path().join(DATABASE_DIR).display()
    );
    let inventory = Store::inventory(project.cache.path())?;
    assert_eq!(inventory.schema_version, Some(SCHEMA_VERSION));
    assert_eq!(inventory.identity, None);
    let files = project.files()?;

    for (dir, args) in [
        ("sub", &["build", "ci"][..]),
        ("sub", &["why"][..]),
        ("", &["cache", "clean"][..]),
        ("", &["cache", "reset", "sub", "--yes"][..]),
    ] {
        let run = project.run(dir, args)?;
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert!(run.stderr.contains(&error), "{args:?}: {run:?}");
        assert_eq!(
            Store::inventory(project.cache.path())?,
            inventory,
            "{args:?}"
        );
        assert_eq!(project.files()?, files, "{args:?}");
    }

    let run = project.run("", &["cache", "reset", "--all", "--yes"])?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        format!(
            "kizz cache reset --all wipes the whole cache at `{}`: {}\nthe cache has no cache \
             identity, so kizz cannot tell which machine and user it belongs to\nwiped the \
             whole cache: removed {}\n",
            project.cache.path().display(),
            before.description(),
            before.counts()
        )
    );
    assert_eq!(
        Store::inventory(project.cache.path())?,
        kizz_store::Inventory::default()
    );

    project.build("sub")?;
    assert_eq!(
        Store::inventory(project.cache.path())?.identity,
        Some(CacheIdentity::current())
    );
    assert!(!project.entries()?.is_empty());
    Ok(())
}

/// A `kizz` started with another umask, which the test sets in the child before it
/// runs, is refused a cache that a `kizz` with the default umask created, and the
/// error names the umask, stored and current.
#[test]
fn a_kizz_with_another_umask_is_refused_the_cache() -> TestResult {
    let project = Project::new()?;
    project.kizz_md("", &[(
        "Make",
        "run(\"sh\", \"-c\", \"echo made > out.txt\")",
    )])?;
    project.build("")?;
    let stored = Store::inventory(project.cache.path())?
        .identity
        .ok_or("the build stamped no identity")?;
    assert_eq!(stored, CacheIdentity::current());
    let umask: libc::mode_t = if stored.umask == Some(0o027) {
        0o077
    } else {
        0o027
    };

    let mut command = project.command("", &["build", "ci"]);
    // SAFETY: the closure runs in the child between `fork` and `exec`, and calls only
    // `umask`, which is async-signal-safe and cannot fail.
    unsafe {
        command.pre_exec(move || {
            libc::umask(umask);
            Ok(())
        });
    }
    let output = command.output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "belongs to another machine or user, so kizz refuses to use it: umask: {} in \
             the cache, {umask:04o} now; the cache is not portable between machines and \
             users",
            IdentityPart::Umask.value(&stored)
        )),
        "{stderr}"
    );
    assert_eq!(
        Store::inventory(project.cache.path())?.identity,
        Some(stored)
    );
    Ok(())
}

/// A cache command started during a build waits for the cache lock, says at once
/// what it is waiting for, and runs once the build ends.
#[test]
fn a_cache_command_started_during_a_build_waits_for_the_lock() -> TestResult {
    for args in [
        &["cache", "clean"][..],
        &["cache", "reset", "--yes"][..],
        &["cache", "reset", "--all", "--yes"][..],
    ] {
        let project = Project::new()?;
        project.kizz_md("", &[("Hold", HOLD)])?;
        let (build, build_lines) = project.start_build("")?;

        let mut waiting = project
            .command("", args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = waiting.stderr.take().ok_or("no standard error")?;
        let mut lines = BufReader::new(stderr).lines();
        let first = lines
            .next()
            .ok_or("the cache command ended without waiting")??;
        assert!(
            first.starts_with(&format!(
                "waiting for the kizz cache, which is in use by process {} ",
                build.id()
            )),
            "{args:?}: {first}"
        );
        assert!(
            first.ends_with(" --color never --no-banner build ci"),
            "{args:?}: {first}"
        );

        project.write("go", "")?;
        let build_rest: Vec<String> = build_lines.collect::<Result<_, _>>()?;
        let build = build.wait_with_output()?;
        assert_eq!(build.status.code(), Some(0), "{build:?} {build_rest:?}");
        let output = waiting.wait_with_output()?;
        let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {output:?} {rest:?}"
        );
        assert_eq!(rest, Vec::<String>::new(), "{args:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("removed ") || stdout.contains("nothing to remove"),
            "{args:?}: {output:?}"
        );
        assert!(!project.cache.path().join(LOCK_FILE_NAME).exists());
    }
    Ok(())
}

/// The step record of `entries` that ran in `cwd` with `last` as its last argument.
fn record_of(
    entries: &Entries,
    cwd: &Path,
    last: &str,
) -> TestResult<(StepKey, StepRecord)> {
    entries
        .steps
        .iter()
        .find(|(_, record)| {
            record.spec.cwd == cwd && record.spec.argv.last().map(String::as_str) == Some(last)
        })
        .cloned()
        .ok_or_else(|| format!("no step record of {last:?} in {}", cwd.display()).into())
}

/// The owners of `record`, in order.
fn owners(record: &(StepKey, StepRecord)) -> Vec<TaskId> {
    record.1.owners.iter().cloned().collect()
}

/// The last argument of the first configuration of the steps in [`Owned`].
const FIRST: &str = "cat in.txt && echo v1";

/// The last argument of the second configuration of the steps in [`Owned`].
const SECOND: &str = "cat in.txt && echo v2";

/// The line of a verbose build that says the first configuration of `Mine` was
/// cached.
const MINE_CACHED: &str = "cached `sh -c cat in.txt && echo v1` in inside/kizz.md:3 (Mine) step 1";

/// The projects of the end-to-end reset regressions: `inside/kizz.md` has the task
/// `Mine`, whose step runs with an absolute `cwd` outside `inside`, and
/// `outside/kizz.md` has the task `Theirs`, whose step runs with an absolute `cwd`
/// in `inside/work`; each file also has the task `Shared`, with the same block, whose
/// step runs in `common`.
struct Owned<'p> {
    /// The projects and the cache.
    project: &'p Project,
    /// The directory that the test resets.
    inside: PathBuf,
    /// Where the step of `Mine` runs.
    elsewhere: PathBuf,
    /// Where the step of `Theirs` runs.
    work: PathBuf,
    /// Where the shared step runs.
    common: PathBuf,
    /// The task `Mine`.
    mine: TaskId,
    /// The task `Shared` in `inside`.
    inside_shared: TaskId,
    /// The task `Theirs`.
    theirs: TaskId,
    /// The task `Shared` outside `inside`.
    outside_shared: TaskId,
}

impl<'p> Owned<'p> {
    /// The projects, with the files that the steps read.
    fn new(project: &'p Project) -> TestResult<Self> {
        project.write("elsewhere/in.txt", "mine\n")?;
        project.write("inside/work/in.txt", "theirs\n")?;
        project.write("common/shared.txt", "shared\n")?;
        let task = |dir: &str, index: u32| {
            TaskId {
                file: project.path(dir).join("kizz.md"),
                index,
            }
        };
        Ok(Self {
            project,
            inside: project.path("inside"),
            elsewhere: project.path("elsewhere"),
            work: project.path("inside/work"),
            common: project.path("common"),
            mine: task("inside", 0),
            inside_shared: task("inside", 1),
            theirs: task("outside", 0),
            outside_shared: task("outside", 1),
        })
    }

    /// Write both `kizz.md` files, with the configurations `mine` and `theirs`.
    fn configure(
        &self,
        mine: &str,
        theirs: &str,
    ) -> TestResult {
        let shared = format!(
            "run(\"cat\", \"shared.txt\", cwd = \"{}\")",
            self.common.display()
        );
        let block = |cwd: &Path, version: &str| {
            format!(
                "run(\"sh\", \"-c\", \"cat in.txt && echo {version}\", cwd = \"{}\")",
                cwd.display()
            )
        };
        self.project.kizz_md("inside", &[
            ("Mine", &block(&self.elsewhere, mine)),
            ("Shared", &shared),
        ])?;
        self.project.kizz_md("outside", &[
            ("Theirs", &block(&self.work, theirs)),
            ("Shared", &shared),
        ])
    }

    /// Run `kizz --verbose build ci` in the projects' directory, which must succeed,
    /// and return its standard output.
    fn build(&self) -> TestResult<String> {
        let run = self.project.run("", &["--verbose", "build", "ci"])?;
        assert_eq!(run.status, Some(0), "{run:?}");
        Ok(run.stdout)
    }

    /// Build both configurations, the first configuration of `Mine` again, which is
    /// cached on its historical record, and the second again, so the first
    /// configuration of each task is historical.
    fn history(&self) -> TestResult {
        self.configure("v1", "v1")?;
        self.build()?;
        self.configure("v2", "v2")?;
        self.build()?;
        self.configure("v1", "v2")?;
        let restored = self.build()?;
        assert!(restored.contains(MINE_CACHED), "{restored}");
        self.configure("v2", "v2")?;
        self.build()?;
        Ok(())
    }

    /// Check the owners of every record in `before`, the cache after the history,
    /// and return the keys of the records of `Mine`.
    fn check_history(
        &self,
        before: &Entries,
    ) -> TestResult<Vec<StepKey>> {
        let mut mine = Vec::new();
        for last in [FIRST, SECOND] {
            let record = record_of(before, &self.elsewhere, last)?;
            assert_eq!(owners(&record), slice::from_ref(&self.mine));
            mine.push(record.0);
            let record = record_of(before, &self.work, last)?;
            assert_eq!(owners(&record), slice::from_ref(&self.theirs));
        }
        let shared = record_of(before, &self.common, "shared.txt")?;
        assert_eq!(owners(&shared), [
            self.inside_shared.clone(),
            self.outside_shared.clone()
        ]);
        // Every task that stores a step position owns the record its key names.
        for (key, position) in &before.positions {
            let owned = before
                .steps
                .iter()
                .any(|(step, record)| *step == position.step && record.owners.contains(&key.task));
            assert!(owned, "{key:?}: {before:#?}");
        }
        let mut removed: Vec<StepKey> = before
            .under(&self.inside)
            .steps
            .iter()
            .map(|(key, _)| *key)
            .collect();
        removed.sort();
        mine.sort();
        assert_eq!(removed, mine);
        assert_eq!(before.shared(&self.inside), [shared]);
        Ok(mine)
    }

    /// Check `after`, the cache after the reset of `inside`, against `before`.
    fn check_reset(
        &self,
        before: &Entries,
        after: &Entries,
    ) -> TestResult {
        assert_eq!(after, &before.after_reset(&self.inside));
        for last in [FIRST, SECOND] {
            assert!(
                record_of(after, &self.elsewhere, last).is_err(),
                "{after:#?}"
            );
            let theirs = record_of(before, &self.work, last)?;
            assert!(after.steps.contains(&theirs), "{after:#?}");
        }
        let shared = record_of(after, &self.common, "shared.txt")?;
        assert_eq!(owners(&shared), slice::from_ref(&self.outside_shared));
        assert!(after.tasks.iter().any(|(id, _)| *id == self.theirs));
        assert!(
            after
                .positions
                .iter()
                .any(|(key, _)| key.task == self.theirs)
        );
        let tasks = after.tasks.iter().map(|(id, _)| id);
        let orders = after.orders.iter().map(|(id, _)| id);
        let positioned = after.positions.iter().map(|(key, _)| &key.task);
        assert!(
            tasks
                .chain(orders)
                .chain(positioned)
                .all(|id| !id.file.starts_with(&self.inside)),
            "{after:#?}"
        );
        Ok(())
    }
}

/// A path reset follows the owners of step records, never their working
/// directories, through `kizz build` and `kizz cache reset`:
///
/// * a task under the path whose step runs with an absolute `cwd` outside it has a
///   historical record that is cached when its first configuration is restored before the
///   reset; the reset removes its historical and current records, so restoring the first
///   configuration afterwards runs the command again;
/// * a task outside the path whose step runs with an absolute `cwd` inside it keeps its
///   historical and current records, its step positions, and its task record, so
///   restoring its first configuration afterwards is cached;
/// * a record shared by a task under the path and a task outside it remains, owned by the
///   outside task alone, and the reset's description counts it as shared, not as removed;
///   the task under the path owns it again when it reaches it later.
#[test]
fn a_reset_follows_the_owners_of_step_records_not_their_working_directories() -> TestResult {
    let project = Project::new()?;
    let owned = Owned::new(&project)?;
    owned.history()?;
    let before = project.entries()?;
    let mine_keys = owned.check_history(&before)?;
    let mine_first = record_of(&before, &owned.elsewhere, FIRST)?;
    let theirs_first = record_of(&before, &owned.work, FIRST)?;

    let files = project.files()?;
    let (description, counts) = before.reset_counts(&owned.inside);
    assert!(
        description.starts_with("2 tasks; 2 step records, 2 step positions, 2 task records, "),
        "{description}"
    );
    assert!(
        counts.ends_with(
            "; 1 step record shared with tasks outside it, kept without the tasks under it"
        ),
        "{counts}"
    );
    let reset = project.run("inside", &["cache", "reset", "--yes"])?;
    assert_eq!(reset.status, Some(0), "{reset:?}");
    assert_eq!(reset.stderr, "", "{reset:?}");
    assert_eq!(
        reset.stdout,
        format!(
            "kizz cache reset wipes every entry for the tasks under `{}`: {description}\nremoved \
             {counts}\n",
            owned.inside.display()
        )
    );
    let after = project.entries()?;
    owned.check_reset(&before, &after)?;
    assert!(after.steps.iter().all(|(key, _)| !mine_keys.contains(key)));
    assert_eq!(project.files()?, files);

    // Restoring both first configurations runs the command of the task under the
    // path again, while the task outside it is cached, and the task under the path
    // owns the shared record again.
    owned.configure("v1", "v1")?;
    let rebuilt = owned.build()?;
    for line in [
        "ran `sh -c cat in.txt && echo v1` in inside/kizz.md:3 (Mine) step 1",
        "cached `sh -c cat in.txt && echo v1` in outside/kizz.md:3 (Theirs) step 1",
        "cached `cat shared.txt` in inside/kizz.md:10 (Shared) step 1",
    ] {
        assert!(rebuilt.contains(line), "{line}: {rebuilt}");
    }
    let again = project.entries()?;
    let mine_again = record_of(&again, &owned.elsewhere, FIRST)?;
    assert_eq!(mine_again.0, mine_first.0);
    assert!(mine_again.1.ran_at_nanos > mine_first.1.ran_at_nanos);
    assert_eq!(owners(&mine_again), slice::from_ref(&owned.mine));
    let theirs_again = record_of(&again, &owned.work, FIRST)?;
    assert_eq!(theirs_again.0, theirs_first.0);
    assert_eq!(theirs_again.1.ran_at_nanos, theirs_first.1.ran_at_nanos);
    assert_eq!(owners(&theirs_again), slice::from_ref(&owned.theirs));
    assert_eq!(owners(&record_of(&again, &owned.common, "shared.txt")?), [
        owned.inside_shared.clone(),
        owned.outside_shared.clone()
    ]);
    Ok(())
}
