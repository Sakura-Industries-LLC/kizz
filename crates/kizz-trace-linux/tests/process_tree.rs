// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Linux tracer reports the process tree of the real processes it starts when
//! the request asks for it: which process and program made each observation, with
//! its outcome, the arguments of each program, and how each process ended.
//!
//! Each test works in its own temporary directory, by its canonical path, because
//! the kernel reports working directories by their real paths, and gives every
//! child an explicit environment. Programs that the shell cannot express run this
//! test executable again in a helper mode: [`helper_process`] does nothing in a
//! normal test run, and does the work of its mode when the tracer starts the
//! executable with the helper variable set in the request's explicit environment.
//!
//! Every tree must combine to exactly the run's accesses, connections and unknown
//! system calls, number its processes from 1, and give each process but the first a
//! parent that comes before it, as [`tree_of`] checks.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: a helper process starts a plain std thread, and the tests assert, \
              lock and print"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::{CString, OsString},
    fs, io,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    ptr, thread,
};

use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CaptureMode, Outcome, Process, ProcessTree,
    Program, RunRequest, RunResult, StdioEcho, Termination, TraceError, Tracer,
};
use kizz_trace_linux::LinuxTracer;

/// The variable that selects the helper mode of this test executable.
const HELPER_VARIABLE: &str = "KIZZ_TRACE_TREE_HELPER";
/// The search path of the shell commands in these tests.
const SEARCH_PATH: &str = "/usr/bin:/bin";
/// The program that the lookup helper runs through `PATH`, which no directory
/// holds.
const MISSING_PROGRAM: &str = "kizz-missing-program";

/// The result of a test or a test helper.
type TestResult<T> = Result<T, Box<dyn Error>>;

/// A temporary directory, known by its canonical path.
struct Workspace {
    /// The directory, removed when the workspace is dropped.
    _dir: tempfile::TempDir,
    /// The canonical path of the directory.
    root: PathBuf,
}

impl Workspace {
    /// A new workspace with the files `files`, each holding its own name.
    fn with_files(files: &[&str]) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        for file in files {
            fs::write(root.join(file), file.as_bytes())?;
        }
        Ok(Self { _dir: dir, root })
    }

    /// The absolute path of `relative` in the workspace.
    fn path(
        &self,
        relative: &str,
    ) -> TestResult<AbsolutePath> {
        Ok(AbsolutePath::new(self.root.join(relative))?)
    }

    /// The accesses of `accesses` under the workspace.
    fn under(
        &self,
        accesses: &Accesses,
    ) -> Accesses {
        accesses
            .iter()
            .filter(|(path, _)| path.as_path().starts_with(&self.root))
            .flat_map(|(path, kinds)| kinds.iter().map(move |kind| (path.clone(), kind)))
            .collect()
    }
}

/// A request that asks for the tree, to run `program` with `args` in `cwd`, with
/// only `PATH` set.
fn request(
    program: &Path,
    args: &[OsString],
    cwd: &Path,
) -> TestResult<RunRequest> {
    Ok(RunRequest {
        program: AbsolutePath::new(program)?,
        args: args.to_vec(),
        env: BTreeMap::from([(OsString::from("PATH"), OsString::from(SEARCH_PATH))]),
        cwd: AbsolutePath::new(cwd)?,
        stdin: None,
        capture: CaptureMode::Split,
        echo: false,
        tree: true,
    })
}

/// The words `words` as arguments.
fn words(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// A request that asks for the tree, to run the shell script `script` in `cwd`.
fn shell(
    script: &str,
    cwd: &Path,
) -> TestResult<RunRequest> {
    request(Path::new("/bin/sh"), &words(&["sh", "-c", script]), cwd)
}

/// A request that asks for the tree, to run this test executable in a helper mode
/// in `cwd`, with `PATH` set to `path`.
fn helper(
    mode: &str,
    cwd: &Path,
    path: &str,
) -> TestResult<RunRequest> {
    let executable = std::env::current_exe()?;
    let mut request = request(
        &executable,
        &words(&[
            "process_tree",
            "--exact",
            "helper_process",
            "--nocapture",
            "--test-threads=1",
            "--quiet",
        ]),
        cwd,
    )?;
    request.env = BTreeMap::from([
        (OsString::from(HELPER_VARIABLE), OsString::from(mode)),
        (OsString::from("PATH"), OsString::from(path)),
    ]);
    Ok(request)
}

/// Run a request without echo or cancellation.
fn run(request: &RunRequest) -> Result<RunResult, TraceError> {
    LinuxTracer::new().run(request, &StdioEcho, &Cancellation::new())
}

/// The tree of `result`, checked: it combines to exactly the run's accesses,
/// connections and unknown system calls, its processes are numbered from 1 in
/// order, and each but the first has a parent that comes before it.
fn tree_of(result: &RunResult) -> TestResult<&ProcessTree> {
    let tree = result.tree.as_ref().ok_or("the result holds no tree")?;
    check_tree(tree, &result.accesses)?;
    if tree.connections() != result.connections {
        return Err("the tree's connections differ from the run's".into());
    }
    if tree.unknown_system_calls() != result.unknown_system_calls {
        return Err("the tree's unknown system calls differ from the run's".into());
    }
    Ok(tree)
}

/// Check that `tree` combines to exactly `accesses`, numbers its processes from 1
/// in order, and gives each process but the first a parent that comes before it.
fn check_tree(
    tree: &ProcessTree,
    accesses: &Accesses,
) -> TestResult<()> {
    if tree.accesses() != *accesses {
        return Err(format!(
            "the tree combines to {:?}, not to the run's {accesses:?}",
            tree.accesses()
        )
        .into());
    }
    for (number, process) in (1_u32..).zip(&tree.processes) {
        if process.number != number {
            return Err(format!("process {} is listed as {number}", process.number).into());
        }
        let parented = process.parent.is_some_and(|parent| parent < number);
        if number > 1 && !parented {
            return Err(format!("process {number} has the parent {:?}", process.parent).into());
        }
    }
    Ok(())
}

/// The program of `tree`, with its process, whose observations hold `path`.
fn observer<'t>(
    tree: &'t ProcessTree,
    path: &AbsolutePath,
) -> TestResult<(&'t Process, &'t Program)> {
    let mut found = tree
        .programs()
        .filter(|(_, program)| program.observations.get(path).is_some());
    let first = found
        .next()
        .ok_or_else(|| format!("no program observed {path}"))?;
    if found.next().is_some() {
        return Err(format!("more than one program observed {path}").into());
    }
    Ok(first)
}

/// The outcomes of `kind` of `path` in the observations of `program`.
fn outcomes(
    program: &Program,
    path: &AbsolutePath,
    kind: AccessKind,
) -> BTreeSet<Outcome> {
    program
        .observations
        .get(path)
        .and_then(|kinds| kinds.get(&kind))
        .cloned()
        .unwrap_or_default()
}

/// The work of a helper mode, run by this test executable when the tracer starts it
/// with [`HELPER_VARIABLE`] set. In a normal test run it does nothing.
#[test]
fn helper_process() {
    let Some(mode) = std::env::var_os(HELPER_VARIABLE) else {
        return;
    };
    match mode.to_str() {
        Some("thread") => helper_thread().unwrap(),
        Some("lookup") => helper_lookup().unwrap(),
        other => panic!("unknown helper mode {other:?}"),
    }
}

/// Read `thread.txt` from a thread of its own.
fn helper_thread() -> TestResult<()> {
    let reader = thread::spawn(|| fs::read("thread.txt"));
    reader.join().map_err(|_| "the thread panicked")??;
    Ok(())
}

/// Fork a child that runs [`MISSING_PROGRAM`] through `PATH` with `execvp`, which
/// fails in every directory, and exits with 0; wait for it.
fn helper_lookup() -> TestResult<()> {
    let name = CString::new(MISSING_PROGRAM)?;
    let argv = [name.as_ptr(), ptr::null()];
    // SAFETY: the child only calls `execvp` with a NUL-terminated name and a
    // null-terminated argument vector, prepared before the fork, and `_exit`.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: as above.
        unsafe {
            libc::execvp(name.as_ptr(), argv.as_ptr());
            libc::_exit(0);
        }
    }
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut status = 0;
    // SAFETY: `status` is writable, and `pid` is this process's child.
    let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    if waited != pid {
        return Err(io::Error::last_os_error().into());
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(format!("the child ended with the status {status}").into());
    }
    Ok(())
}

/// A shell script that reads a file, runs a subshell that reads another, and runs
/// `cat` on a third gives a tree in which each read belongs to the program that
/// made it: the shell, the subshell's inherited shell, and `cat`, a program of its
/// own with its arguments.
#[test]
fn each_read_belongs_to_the_program_that_made_it() -> TestResult<()> {
    let workspace = Workspace::with_files(&["first.txt", "second.txt", "third.txt"])?;
    let script = "read line < first.txt; (read line < second.txt); /bin/cat third.txt; true";
    let result = run(&shell(script, &workspace.root)?)?;
    assert_eq!(result.termination, Termination::Exited(0));
    let tree = tree_of(&result)?;
    let ok = BTreeSet::from([Outcome::Ok]);

    let first = workspace.path("first.txt")?;
    let (process, program) = observer(tree, &first)?;
    assert_eq!(process.number, 1);
    assert_eq!(program.path.as_path(), Path::new("/bin/sh"));
    assert_eq!(program.args, words(&["sh", "-c", script]));
    assert!(!program.inherited);
    assert_eq!(outcomes(program, &first, AccessKind::Read), ok);

    let second = workspace.path("second.txt")?;
    let (process, program) = observer(tree, &second)?;
    assert_eq!(process.parent, Some(1));
    assert_eq!(program.path.as_path(), Path::new("/bin/sh"));
    assert_eq!(program.args, words(&["sh", "-c", script]));
    assert!(program.inherited, "the subshell's shell is not inherited");
    assert_eq!(outcomes(program, &second, AccessKind::Read), ok);

    let third = workspace.path("third.txt")?;
    let (process, program) = observer(tree, &third)?;
    assert_eq!(process.parent, Some(1));
    assert_eq!(program.path.as_path(), Path::new("/bin/cat"));
    assert_eq!(program.args, words(&["/bin/cat", "third.txt"]));
    assert!(!program.inherited);
    assert_eq!(outcomes(program, &third, AccessKind::Read), ok);
    assert_eq!(
        outcomes(program, &AbsolutePath::new("/bin/cat")?, AccessKind::Exec),
        ok
    );
    Ok(())
}

/// A child that runs a missing program through `PATH` records the failed lookups,
/// each a probe with `ENOENT`, under the program it inherited.
#[test]
fn failed_lookups_belong_to_the_inherited_program() -> TestResult<()> {
    let workspace = Workspace::with_files(&[])?;
    let directories = ["one", "two"];
    for directory in directories {
        fs::create_dir(workspace.root.join(directory))?;
    }
    let path = directories
        .map(|directory| {
            workspace
                .root
                .join(directory)
                .to_string_lossy()
                .into_owned()
        })
        .join(":");
    let request = helper("lookup", &workspace.root, &path)?;
    let result = run(&request)?;
    assert_eq!(result.termination, Termination::Exited(0));
    let tree = tree_of(&result)?;
    let enoent = BTreeSet::from([Outcome::Error("ENOENT".to_owned())]);
    for directory in directories {
        let missing = workspace.path(&format!("{directory}/{MISSING_PROGRAM}"))?;
        let (process, program) = observer(tree, &missing)?;
        assert_eq!(process.parent, Some(1), "{missing}");
        assert!(program.inherited, "{missing}");
        assert_eq!(program.path, request.program);
        assert_eq!(program.args, request.args);
        assert_eq!(outcomes(program, &missing, AccessKind::Probe), enoent);
        assert_eq!(
            program
                .observations
                .kinds(&missing)
                .map(|kinds| kinds.iter().collect::<Vec<_>>()),
            Some(vec![AccessKind::Probe])
        );
        assert_eq!(process.status, Some(Termination::Exited(0)));
    }
    Ok(())
}

/// A thread's accesses fold into its process, so a helper that reads a file from a
/// thread of its own has one process, whose program observed the file.
#[test]
fn a_thread_folds_into_its_process() -> TestResult<()> {
    let workspace = Workspace::with_files(&["thread.txt"])?;
    let result = run(&helper("thread", &workspace.root, SEARCH_PATH)?)?;
    assert_eq!(result.termination, Termination::Exited(0));
    let tree = tree_of(&result)?;
    assert_eq!(tree.processes.len(), 1, "{tree:?}");
    let file = workspace.path("thread.txt")?;
    let (process, program) = observer(tree, &file)?;
    assert_eq!(process.number, 1);
    assert!(!program.inherited);
    assert_eq!(
        outcomes(program, &file, AccessKind::Read),
        BTreeSet::from([Outcome::Ok])
    );
    Ok(())
}

/// Arguments that hold an empty string, spaces and bytes that are not UTF-8 are
/// recorded exactly.
#[test]
fn arguments_are_recorded_exactly() -> TestResult<()> {
    let workspace = Workspace::with_files(&[])?;
    let args = vec![
        OsString::from("true"),
        OsString::new(),
        OsString::from("two  words "),
        OsString::from_vec(b"\xFF\xFE".to_vec()),
    ];
    let result = run(&request(Path::new("/bin/true"), &args, &workspace.root)?)?;
    assert_eq!(result.termination, Termination::Exited(0));
    let tree = tree_of(&result)?;
    let command = tree.process(1).ok_or("no process 1")?;
    let programs: Vec<(&Path, &[OsString], bool)> = command
        .programs
        .iter()
        .map(|program| {
            (
                program.path.as_path(),
                program.args.as_slice(),
                program.inherited,
            )
        })
        .collect();
    assert_eq!(programs, [(Path::new("/bin/true"), args.as_slice(), false)]);
    Ok(())
}

/// A child killed by a signal has that status, and the status of process 1, which
/// ran the command, is the result's termination.
#[test]
fn statuses_are_those_the_processes_ended_with() -> TestResult<()> {
    let workspace = Workspace::with_files(&[])?;
    let script = "sh -c 'kill -9 $$'; exit 3";
    let result = run(&shell(script, &workspace.root)?)?;
    assert_eq!(result.termination, Termination::Exited(3));
    let tree = tree_of(&result)?;
    let command = tree.process(1).ok_or("no process 1")?;
    assert_eq!(command.status, Some(result.termination));
    let killed: Vec<&Process> = tree
        .processes
        .iter()
        .filter(|process| {
            process
                .programs
                .iter()
                .any(|program| program.args == words(&["sh", "-c", "kill -9 $$"]))
        })
        .collect();
    let [killed] = killed.as_slice() else {
        return Err(format!("expected one inner shell: {tree:?}").into());
    };
    assert_eq!(killed.parent, Some(1));
    assert_eq!(killed.status, Some(Termination::Signaled(libc::SIGKILL)));
    Ok(())
}

/// A missing program gives an error whose tree holds the command's one process,
/// without a status, with the request's program and the failed attempt's outcome.
#[test]
fn a_missing_program_gives_a_tree_of_the_attempt() -> TestResult<()> {
    let workspace = Workspace::with_files(&[])?;
    let missing = workspace.root.join("missing");
    let request = request(&missing, &words(&["missing", "-v"]), &workspace.root)?;
    let error = run(&request).err().ok_or("the missing program ran")?;
    let TraceError::ProgramNotFound { accesses, tree, .. } = &error else {
        return Err(format!("expected a missing program, got {error:?}").into());
    };
    let tree = tree.as_ref().ok_or("the error holds no tree")?;
    check_tree(tree, accesses)?;
    let [command] = tree.processes.as_slice() else {
        return Err(format!("expected one process: {tree:?}").into());
    };
    assert!(command.pid.is_some());
    assert_eq!(command.status, None);
    let [program] = command.programs.as_slice() else {
        return Err(format!("expected one program: {command:?}").into());
    };
    assert_eq!(program.path, request.program);
    assert_eq!(program.args, request.args);
    assert!(!program.inherited);
    assert_eq!(
        outcomes(program, &request.program, AccessKind::Probe),
        BTreeSet::from([Outcome::Error("ENOENT".to_owned())])
    );
    Ok(())
}

/// The same command with and without the request for the tree gives the same
/// combined accesses under the test's temporary directory, and no tree without
/// it; a missing program gives no tree without it either.
#[test]
fn the_tree_changes_nothing_else() -> TestResult<()> {
    let workspace = Workspace::with_files(&["first.txt", "second.txt", "third.txt"])?;
    fs::create_dir(workspace.root.join("out"))?;
    let script = "read line < first.txt; (read line < second.txt); /bin/cat third.txt; \
                  echo x > out/written.txt; test -e out/missing; true";
    let mut request = shell(script, &workspace.root)?;
    let with = run(&request)?;
    request.tree = false;
    let without = run(&request)?;
    assert_eq!(without.tree, None);
    tree_of(&with)?;
    assert_eq!(
        workspace.under(&with.accesses),
        workspace.under(&without.accesses)
    );
    assert!(!workspace.under(&with.accesses).is_empty());
    assert_eq!(with.termination, without.termination);

    let mut missing = shell("true", &workspace.root)?;
    missing.program = workspace.path("missing")?;
    missing.tree = false;
    let error = run(&missing).err().ok_or("the missing program ran")?;
    assert!(error.tree().is_none(), "{error:?}");
    Ok(())
}
