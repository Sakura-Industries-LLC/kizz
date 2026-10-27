// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Running one command for `kizz execute` ("Observing any command with
//! `kizz execute`"), through the tracer interface and without the cache.
//!
//! [`execute`] takes the command's arguments, the caller's whole environment and
//! working directory ([`ExecuteRequest`]), and the handle through which the caller
//! forwards signals. It looks the program up with the lookup of `run()`
//! ([`search_program`]), through the caller's `PATH`, and names the outcome of each
//! checked location through the tracer's error-name query. When it finds a program,
//! it runs it through the tracer with the caller's environment and working
//! directory unchanged, keeping the terminal and asking for the process tree. It
//! returns how the command ended ([`CommandOutcome`]), the exit status of
//! `kizz execute` ([`exit_status`]), and the observation [`Report`] of the one
//! command, in which the lookup's probes are observations of the command's first
//! program. It never opens the store.
//!
//! The outcomes and their statuses:
//!
//! * the program exited with a code: `exited`, with that code;
//! * a signal killed it: `signaled`, with 128 plus the signal's number;
//! * the lookup found no location, or a name without `/` met an environment without
//!   `PATH`, or the tracer reported the program missing: `not_found`, with
//!   [`STATUS_NOT_FOUND`];
//! * the lookup found only a location that cannot be run, or the tracer reported the
//!   program not executable: `not_executable`, with [`STATUS_NOT_EXECUTABLE`];
//! * the tracer could not start the program for another reason: `start_failed`, with
//!   [`STATUS_NOT_EXECUTABLE`] too, since the program was found but could not be invoked;
//! * the tracer failed: `failed`, with [`STATUS_FAILED`], and the report says that the
//!   trace is incomplete and names the failure.
//!
//! When the lookup fails, no process starts, and the report's command holds one
//! placeholder process whose program is the name that kizz looked up, with every
//! location that the lookup checked.
//!
//! The report never claims a complete trace that the tracer reported otherwise: a
//! reason that the trace is incomplete, an unknown system call or a connection that
//! the run reports but that no program of its tree lists is put on the first
//! program of the command's first process, and a tracer that gives no tree gets the
//! tree of one process with everything the run reports.
//!
//! [`not_executed`] gives the outcome, status and report of a command that kizz
//! could not run because it failed itself before the start, such as when it could
//! not set up the handling of signals.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
};

use kizz_trace::{
    AbsolutePath, Cancellation, CaptureMode, Connection, Observations, Process, ProcessTree,
    Program, RunRequest, RunResult, StdioEcho, Termination, Tracer,
};

use crate::{
    lookup::{PATH_VARIABLE, ProgramSearch, search_program},
    report::{CommandInput, CommandOutcome, Execution, Report},
};

/// The exit status when kizz itself fails after it accepted the command line: the
/// tracer failed, or the report could not be written.
pub const STATUS_FAILED: u8 = 125;

/// The exit status when the program is found but cannot be run or started.
pub const STATUS_NOT_EXECUTABLE: u8 = 126;

/// The exit status when the program is not found.
pub const STATUS_NOT_FOUND: u8 = 127;

/// The base of the exit status of a program that a signal killed: 128 plus the
/// signal's number.
const SIGNALED_BASE: u8 = 128;

/// The command that `kizz execute` runs, as its caller gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteRequest {
    /// The program, as given, and its arguments: the argument vector that the
    /// program receives.
    pub args: Vec<OsString>,
    /// The caller's whole environment, which the program gets unchanged.
    pub env: BTreeMap<OsString, OsString>,
    /// The caller's working directory, which is the program's.
    pub cwd: AbsolutePath,
}

/// What a run of `kizz execute` gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    /// How the command ended, or why it did not run.
    pub outcome: CommandOutcome,
    /// The exit status of `kizz execute`, as [`exit_status`] gives it.
    pub status: u8,
    /// The observation report of the one command.
    pub report: Report,
}

/// The exit status of `kizz execute` for a command whose outcome is `outcome`: the
/// program's exit code, its low eight bits as the system reports them; 128 plus
/// the number of the signal that killed it; [`STATUS_NOT_FOUND`] when the program
/// is not found; [`STATUS_NOT_EXECUTABLE`] when it is found but cannot be run or
/// started; and [`STATUS_FAILED`] when the tracer failed. A signal number that
/// does not fit gives 255.
#[must_use]
pub fn exit_status(outcome: &CommandOutcome) -> u8 {
    match outcome {
        CommandOutcome::Exited(code) => u8::try_from(code & 0xFF).unwrap_or(u8::MAX),
        CommandOutcome::Signaled(signal) => {
            u8::try_from(*signal)
                .ok()
                .and_then(|signal| SIGNALED_BASE.checked_add(signal))
                .unwrap_or(u8::MAX)
        },
        CommandOutcome::NotFound(_) => STATUS_NOT_FOUND,
        CommandOutcome::NotExecutable(_) | CommandOutcome::StartFailed(_) => STATUS_NOT_EXECUTABLE,
        CommandOutcome::Failed(_) => STATUS_FAILED,
    }
}

/// Run the command of `request` through `tracer`, keeping the terminal and with
/// the process tree, while `cancel` carries the signals that the caller forwards;
/// return its outcome, the exit status and the report, as the module's
/// documentation says.
pub fn execute(
    tracer: &dyn Tracer,
    request: ExecuteRequest,
    cancel: &Cancellation,
) -> Executed {
    let ExecuteRequest { args, env, cwd } = request;
    let Some(name) = args.first().cloned() else {
        return finish(
            args,
            None,
            cwd,
            CommandOutcome::NotFound("no program was given".to_owned()),
            None,
            Observations::new(),
        );
    };
    let shown = name.to_string_lossy().into_owned();
    let search = env.get(OsStr::new(PATH_VARIABLE)).map(OsString::as_os_str);
    let Some(found) = search_program(&name, search, &cwd, |errno| tracer.error_name(errno)) else {
        let message = format!(
            "program `{shown}` was not found: a program name without `/` is looked up in PATH, \
             and PATH is not set"
        );
        return finish(
            args,
            None,
            cwd,
            CommandOutcome::NotFound(message),
            None,
            Observations::new(),
        );
    };
    match found {
        ProgramSearch::NotFound { checked } => {
            let message = format!("program `{shown}` was not found in PATH");
            finish(
                args,
                None,
                cwd,
                CommandOutcome::NotFound(message),
                None,
                checked,
            )
        },
        ProgramSearch::NotExecutable { candidate, checked } => {
            let message = format!("program `{shown}` is not executable: {candidate}");
            finish(
                args,
                Some(candidate),
                cwd,
                CommandOutcome::NotExecutable(message),
                None,
                checked,
            )
        },
        ProgramSearch::Found { program, checked } => {
            let run = RunRequest {
                program: program.clone(),
                args: args.clone(),
                env,
                cwd: cwd.clone(),
                stdin: None,
                capture: CaptureMode::Split,
                echo: false,
                tree: true,
                terminal: true,
            };
            let (outcome, tree) = match tracer.run(&run, &StdioEcho, cancel) {
                Ok(result) => {
                    (
                        CommandOutcome::from_termination(result.termination),
                        Some(run_tree(&run, &result)),
                    )
                },
                Err(error) => (CommandOutcome::from_error(&error), error.tree().cloned()),
            };
            finish(args, Some(program), cwd, outcome, tree, checked)
        },
    }
}

/// The outcome, status and report of the command of `request`, which kizz did not
/// run because it failed itself before the start, as `message` says: `failed`,
/// with [`STATUS_FAILED`], and a report whose command holds the placeholder
/// process and says that the trace is incomplete.
#[must_use]
pub fn not_executed(
    request: ExecuteRequest,
    message: String,
) -> Executed {
    finish(
        request.args,
        None,
        request.cwd,
        CommandOutcome::Failed(message),
        None,
        Observations::new(),
    )
}

/// The outcome, status and report of the command `argv`, whose program is
/// `program`, run in `cwd`, which ended as `outcome` says, with the tracer's
/// `tree` and the lookup's observations `lookup`.
fn finish(
    argv: Vec<OsString>,
    program: Option<AbsolutePath>,
    cwd: AbsolutePath,
    outcome: CommandOutcome,
    tree: Option<ProcessTree>,
    lookup: Observations,
) -> Executed {
    let status = exit_status(&outcome);
    let report = Report::new(
        vec![CommandInput {
            argv,
            program,
            cwd,
            execution: Execution::Ran {
                outcome: outcome.clone(),
                tree,
                lookup,
            },
        }],
        None,
    );
    Executed {
        outcome,
        status,
        report,
    }
}

/// The process tree of `result`, the run of `request`, with everything that the run
/// reports: the tracer's tree, or the tree of one process when the tracer gave none,
/// and on the first program of its first process the run's reason that the trace is
/// incomplete, its unknown system calls and its connections that no program of the
/// tree lists.
fn run_tree(
    request: &RunRequest,
    result: &RunResult,
) -> ProcessTree {
    let mut tree = result.tree.clone().unwrap_or_else(|| {
        ProcessTree::of_request(request, &result.accesses, Some(result.termination))
    });
    let unknown: BTreeSet<u32> = result
        .unknown_system_calls
        .difference(&tree.unknown_system_calls())
        .copied()
        .collect();
    let connections: BTreeSet<Connection> = result
        .connections
        .difference(&tree.connections())
        .cloned()
        .collect();
    let reason = result
        .incomplete
        .as_ref()
        .filter(|reason| {
            !tree
                .programs()
                .any(|(_, program)| program.incomplete.contains(reason))
        })
        .cloned();
    if unknown.is_empty() && connections.is_empty() && reason.is_none() {
        return tree;
    }
    if let Some(first) = first_program(&mut tree, request, result.termination) {
        first.unknown_system_calls.extend(unknown);
        first.connections.extend(connections);
        first.incomplete.extend(reason);
    }
    tree
}

/// The first program of the first process of `tree`, the run of `request` that
/// ended as `termination` says, adding the process or the program first when the
/// tree has none.
fn first_program<'a>(
    tree: &'a mut ProcessTree,
    request: &RunRequest,
    termination: Termination,
) -> Option<&'a mut Program> {
    if tree.processes.is_empty() {
        tree.processes.push(Process {
            number: 1,
            pid: None,
            parent: None,
            programs: Vec::new(),
            status: Some(termination),
        });
    }
    let process = tree
        .processes
        .iter_mut()
        .min_by_key(|process| process.number)?;
    if process.programs.is_empty() {
        process.programs.push(Program::new(
            request.program.clone(),
            request.args.clone(),
            false,
        ));
    }
    process.programs.first_mut()
}
