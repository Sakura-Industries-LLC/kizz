// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The parallel work of a command never uses `rayon`'s global pool, which panics
//! when it cannot start a thread ("Kizz never panics"): discovery, the cache check,
//! the recording of observations, BLAKE3's parallel hashing of a large file, and
//! cache cleanup all run on the command's [`WorkerPool`].
//!
//! This is a test executable of its own, with one test, so nothing else in the
//! process can start the global pool; building it afterwards succeeds only when it
//! never started.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: building the global pool proves that it never started, and the \
              tests assert, lock and print"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

use kizz_core::{
    BuildOutcome, Engine, NoEvents, PrintSink, TargetExpr, TaskInfo, WorkerPool, build, clean, load,
};
use kizz_store::Store;
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Cancellation, CapturedOutput, EchoSink, ExtendedAttributes,
    FirstReads, MachineState, OutputStream, RunRequest, RunResult, Termination, TraceError, Tracer,
};

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The size of the large file that every task reads: large enough for BLAKE3 to
/// split its hashing with `rayon::join`.
const LARGE: usize = 4_194_304;

/// A tracer that runs nothing: each argument `read:PATH` of a command reads a file,
/// and `write:PATH` writes one, relative to the command's working directory, and it
/// reports those accesses.
struct FakeTracer;

impl Tracer for FakeTracer {
    fn run(
        &self,
        request: &RunRequest,
        _echo: &dyn EchoSink,
        _cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        let failure = |error: &dyn Error| {
            TraceError::Tracer {
                message: error.to_string(),
                source: None,
            }
        };
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Exec);
        for arg in request.args.iter().skip(1) {
            let arg = arg.to_str().unwrap_or_default();
            let (kind, path) = match arg.split_once(':') {
                Some(("read", path)) => (AccessKind::Read, path),
                Some(("write", path)) => (AccessKind::Write, path),
                _ => continue,
            };
            let path = request.cwd.as_path().join(path);
            if kind == AccessKind::Write {
                fs::write(&path, "written").map_err(|error| failure(&error))?;
            } else {
                fs::read(&path).map_err(|error| failure(&error))?;
            }
            accesses.record(
                AbsolutePath::new(path).map_err(|error| failure(&error))?,
                kind,
            );
        }
        Ok(RunResult {
            termination: Termination::Exited(0),
            output: CapturedOutput::empty(request.capture),
            tree: None,
            accesses,
            incomplete: None,
            first_reads: FirstReads::new(),
            unknown_system_calls: BTreeSet::new(),
            connections: BTreeSet::new(),
        })
    }

    fn extended_attributes(
        &self,
        _path: &Path,
        _follow: bool,
    ) -> io::Result<ExtendedAttributes> {
        Ok(ExtendedAttributes::new())
    }

    fn machine_state(
        &self,
        _cwd: &Path,
    ) -> MachineState {
        MachineState::default()
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

/// Prints nothing.
struct NoPrint;

impl PrintSink for NoPrint {
    fn print_line(
        &self,
        _line: &str,
    ) {
    }
}

/// Write `content` to `relative` under `root`, creating its directories.
fn write(
    root: &Path,
    relative: &str,
    content: &[u8],
) -> TestResult<PathBuf> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;
    Ok(path)
}

/// Build the tasks of target `ci` under `project` once, with the cache in `cache`,
/// on `pool`.
fn build_once(
    project: &Path,
    cache: &Path,
    pool: &WorkerPool,
) -> TestResult {
    let loaded = load(project, pool)?;
    assert_eq!(loaded.files.len(), 3);
    let target: TargetExpr = "ci".parse()?;
    let mut tasks = Vec::new();
    for selected in target.select(&loaded.tasks) {
        tasks.push(TaskInfo::new(selected.task, &selected.selected)?);
    }
    let caller = BTreeMap::from([(
        "PATH".to_owned(),
        project.join("bin").to_string_lossy().into_owned(),
    )]);
    let store = Store::open(cache)?;
    let engine = Engine::new(
        &FakeTracer,
        &store,
        &NoEcho,
        Cancellation::new(),
        None,
        pool,
    );
    let report = build(&engine, &tasks, &caller, &NoPrint, &NoEvents)?;
    assert_eq!(report.outcome, BuildOutcome::Succeeded);
    drop(engine);
    store.close()?;
    Ok(())
}

/// A build that discovers several `kizz.md` files, checks and records steps, and
/// hashes a file large enough for BLAKE3 to split its work, then a cache cleanup,
/// never start `rayon`'s global pool: building it afterwards succeeds.
#[test]
fn a_build_and_a_cleanup_never_start_the_global_pool() -> TestResult {
    let project = tempfile::tempdir()?;
    let cache = tempfile::tempdir()?;
    let root = project.path();
    let tool = write(root, "bin/tool", b"#!/bin/sh\n")?;
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755))?;
    let large: Vec<u8> = (0..LARGE)
        .map(|index| u8::try_from(index % 251).unwrap_or_default())
        .collect();
    write(root, "large.bin", &large)?;
    for dir in ["", "a/", ".hidden/"] {
        write(
            root,
            &format!("{dir}kizz.md"),
            [
                "# Tasks\n\n## Build\n\n```starlark ci",
                "env.inherit(\"PATH\")",
                format!(
                    "run(\"tool\", \"read:{}\", \"write:out.txt\")",
                    root.join("large.bin").display()
                )
                .as_str(),
                "```\n",
            ]
            .join("\n")
            .as_bytes(),
        )?;
    }

    let pool = WorkerPool::new()?;
    // The first build records the steps, hashing the large file; the second checks
    // them, hashing it again in a new build.
    build_once(root, cache.path(), &pool)?;
    build_once(root, cache.path(), &pool)?;
    let store = Store::open(cache.path())?;
    let cleaned = clean(&store, &pool)?;
    assert!(cleaned.checked > 0);
    store.close()?;

    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()?;
    Ok(())
}
