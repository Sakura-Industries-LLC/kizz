// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Observation reports ("Observation reports"): what the commands of
//! `kizz execute` or `kizz run` touched, organized by process, for people and for
//! programs.
//!
//! A [`Report`] is built once from [`CommandInput`]s, in the order the commands
//! ran, and an optional [`TaskProgramInput`]. Each command that ran brings the
//! [`ProcessTree`] of the tracer, the [`CommandOutcome`] and the observations of
//! its program lookup; a command that came from the cache brings only its outcome.
//! The model then:
//!
//! * numbers the processes of every command from 1, across the commands, so a number is
//!   unique in the report, and maps each parent to its new number;
//! * puts the lookup's observations into the first program of the command's first
//!   process, or, when kizz started no process, into a placeholder process without a
//!   process ID or status, whose program is the name that kizz looked up;
//! * lists every reason that a command was traced incompletely, with its process: each
//!   reason of a program, each unknown system call, and a tracer failure;
//! * combines every observation and connection of the report, the task program's
//!   included, and counts the [`Totals`].
//!
//! Processes keep their start order, sorted by the tracer's numbers, programs
//! their order, reasons are sorted, and observations and connections are sets, so
//! the renderings do not depend on the order in which the model receives them.
//!
//! [`Report::render`] writes the report as text or JSON ([`ReportFormat`]), each
//! hierarchical or compact ([`ReportMode`]), from the same model. [`JSON_FIELDS`]
//! lists every field of the JSON format by its path, with the modes that hold it.
//! Paths and arguments that are not valid UTF-8 appear with each invalid sequence
//! replaced by U+FFFD, in both formats. Text then escapes each backslash and
//! control character of every string that it shows ([`escape_text`]), so no string
//! breaks its line and two different strings never read alike; JSON keeps its
//! standard escaping.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use kizz_trace::{
    AbsolutePath, AccessKind, Connection, IncompleteReason, Observations, Outcome, ProcessTree,
    Program, Termination, TraceError,
};

/// The name of the JSON format, its `format` field.
pub const FORMAT: &str = "kizz-execute";

/// The version of the JSON format, its `version` field.
///
/// A new field may appear without a new version; a field that changes its meaning
/// or disappears raises it.
pub const VERSION: u32 = 1;

/// Whether a report is written for people or for programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportFormat {
    /// Readable text, the default.
    Text,
    /// JSON, indented by two spaces and ending with a newline (`--json`).
    Json,
}

/// Whether a report keeps the process hierarchy or combines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportMode {
    /// Each command's processes, each with its programs and their observations, the
    /// default.
    Hierarchical,
    /// Every observation and connection of the report, combined into one set
    /// (`--compact`).
    Compact,
}

impl ReportMode {
    /// The mode as the report names it: `hierarchical` or `compact`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hierarchical => "hierarchical",
            Self::Compact => "compact",
        }
    }

    /// The paths of the JSON fields of a report in this mode, in the order of
    /// [`JSON_FIELDS`].
    pub fn json_fields(self) -> impl Iterator<Item = &'static str> {
        JSON_FIELDS
            .iter()
            .filter(move |field| field.is_in(self))
            .map(|field| field.path)
    }
}

/// One field of the JSON format, by its path, with the modes that hold it.
///
/// A path names the members from the top-level object, separated by `.`, with
/// `[]` after the name of an array for its elements, such as
/// `commands[].outcome.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JsonField {
    /// The path of the field.
    pub path: &'static str,
    /// Whether a hierarchical report holds the field.
    pub hierarchical: bool,
    /// Whether a compact report holds the field.
    pub compact: bool,
}

impl JsonField {
    /// A field of both modes.
    const fn both(path: &'static str) -> Self {
        Self {
            path,
            hierarchical: true,
            compact: true,
        }
    }

    /// A field of hierarchical reports only.
    const fn hierarchical(path: &'static str) -> Self {
        Self {
            path,
            hierarchical: true,
            compact: false,
        }
    }

    /// A field of compact reports only.
    const fn compact(path: &'static str) -> Self {
        Self {
            path,
            hierarchical: false,
            compact: true,
        }
    }

    /// Whether a report in `mode` holds the field.
    #[must_use]
    pub const fn is_in(
        self,
        mode: ReportMode,
    ) -> bool {
        match mode {
            ReportMode::Hierarchical => self.hierarchical,
            ReportMode::Compact => self.compact,
        }
    }
}

/// Every field of the JSON format, by its path, in the order the report writes
/// the members of each object ("Observation report fields").
///
/// An object holds only some of its fields when its kind says so: an outcome has
/// `code`, `signal` or `error`, a process's `exit` has `code` or `signal`, and a
/// connection has `address`, `path` or `name`. Every other field is always
/// present, `null` when it has no value.
pub const JSON_FIELDS: &[JsonField] = &[
    JsonField::both("format"),
    JsonField::both("version"),
    JsonField::both("mode"),
    JsonField::both("complete"),
    JsonField::both("totals"),
    JsonField::both("totals.commands"),
    JsonField::both("totals.cached"),
    JsonField::both("totals.processes"),
    JsonField::both("totals.programs"),
    JsonField::both("totals.paths"),
    JsonField::both("totals.connections"),
    JsonField::both("totals.kinds"),
    JsonField::both("totals.kinds.read"),
    JsonField::both("totals.kinds.probe"),
    JsonField::both("totals.kinds.list"),
    JsonField::both("totals.kinds.write"),
    JsonField::both("totals.kinds.exec"),
    JsonField::both("totals.kinds.xattr"),
    JsonField::both("commands"),
    JsonField::both("commands[].argv"),
    JsonField::both("commands[].program"),
    JsonField::both("commands[].cwd"),
    JsonField::both("commands[].cached"),
    JsonField::both("commands[].outcome"),
    JsonField::both("commands[].outcome.kind"),
    JsonField::both("commands[].outcome.code"),
    JsonField::both("commands[].outcome.signal"),
    JsonField::both("commands[].outcome.error"),
    JsonField::both("commands[].complete"),
    JsonField::both("commands[].incomplete"),
    JsonField::both("commands[].incomplete[].kind"),
    JsonField::both("commands[].incomplete[].detail"),
    JsonField::both("commands[].incomplete[].number"),
    JsonField::hierarchical("commands[].incomplete[].process"),
    JsonField::hierarchical("commands[].processes"),
    JsonField::hierarchical("commands[].processes[].id"),
    JsonField::hierarchical("commands[].processes[].pid"),
    JsonField::hierarchical("commands[].processes[].parent"),
    JsonField::hierarchical("commands[].processes[].exit"),
    JsonField::hierarchical("commands[].processes[].exit.kind"),
    JsonField::hierarchical("commands[].processes[].exit.code"),
    JsonField::hierarchical("commands[].processes[].exit.signal"),
    JsonField::hierarchical("commands[].processes[].programs"),
    JsonField::hierarchical("commands[].processes[].programs[].path"),
    JsonField::hierarchical("commands[].processes[].programs[].args"),
    JsonField::hierarchical("commands[].processes[].programs[].inherited"),
    JsonField::hierarchical("commands[].processes[].programs[].observations"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].path"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].access"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].access.read"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].access.probe"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].access.list"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].access.write"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].access.exec"),
    JsonField::hierarchical("commands[].processes[].programs[].observations[].access.xattr"),
    JsonField::hierarchical("commands[].processes[].programs[].connections"),
    JsonField::hierarchical("commands[].processes[].programs[].connections[].kind"),
    JsonField::hierarchical("commands[].processes[].programs[].connections[].address"),
    JsonField::hierarchical("commands[].processes[].programs[].connections[].path"),
    JsonField::hierarchical("commands[].processes[].programs[].connections[].name"),
    JsonField::both("task_program"),
    JsonField::both("task_program.task"),
    JsonField::hierarchical("task_program.observations"),
    JsonField::hierarchical("task_program.observations[].path"),
    JsonField::hierarchical("task_program.observations[].access"),
    JsonField::hierarchical("task_program.observations[].access.read"),
    JsonField::hierarchical("task_program.observations[].access.probe"),
    JsonField::hierarchical("task_program.observations[].access.list"),
    JsonField::hierarchical("task_program.observations[].access.write"),
    JsonField::hierarchical("task_program.observations[].access.exec"),
    JsonField::hierarchical("task_program.observations[].access.xattr"),
    JsonField::compact("observations"),
    JsonField::compact("observations[].path"),
    JsonField::compact("observations[].access"),
    JsonField::compact("observations[].access.read"),
    JsonField::compact("observations[].access.probe"),
    JsonField::compact("observations[].access.list"),
    JsonField::compact("observations[].access.write"),
    JsonField::compact("observations[].access.exec"),
    JsonField::compact("observations[].access.xattr"),
    JsonField::compact("connections"),
    JsonField::compact("connections[].kind"),
    JsonField::compact("connections[].address"),
    JsonField::compact("connections[].path"),
    JsonField::compact("connections[].name"),
];

/// How a command ended, or why it did not run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CommandOutcome {
    /// The command exited with this exit code: `exited`, with `code`.
    Exited(i32),
    /// A signal with this number killed the command: `signaled`, with `signal`.
    Signaled(i32),
    /// The program was not found: `not_found`, with `error`.
    NotFound(String),
    /// The program was found but cannot be run: `not_executable`, with `error`.
    NotExecutable(String),
    /// The program could not be started for another reason: `start_failed`, with
    /// `error`.
    StartFailed(String),
    /// Kizz or its tracer failed: `failed`, with `error`. The command's trace is
    /// then incomplete, with a `tracer_failed` reason.
    Failed(String),
}

impl CommandOutcome {
    /// The outcome of a command that ended as `termination` says.
    #[must_use]
    pub const fn from_termination(termination: Termination) -> Self {
        match termination {
            Termination::Exited(code) => Self::Exited(code),
            Termination::Signaled(signal) => Self::Signaled(signal),
        }
    }

    /// The outcome of a request that failed with `error`, whose message it keeps.
    #[must_use]
    pub fn from_error(error: &TraceError) -> Self {
        let message = error.to_string();
        match error {
            TraceError::ProgramNotFound { .. } => Self::NotFound(message),
            TraceError::ProgramNotExecutable { .. } => Self::NotExecutable(message),
            TraceError::StartFailed { .. } => Self::StartFailed(message),
            TraceError::Tracer { .. } => Self::Failed(message),
        }
    }

    /// The outcome's `kind`, as JSON names it, such as `not_found`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Exited(_) => "exited",
            Self::Signaled(_) => "signaled",
            Self::NotFound(_) => "not_found",
            Self::NotExecutable(_) => "not_executable",
            Self::StartFailed(_) => "start_failed",
            Self::Failed(_) => "failed",
        }
    }

    /// The outcome as text shows it, such as `exited with 0` or
    /// `not found: MESSAGE`, with the message escaped as [`escape_text`] escapes
    /// it.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Exited(code) => format!("exited with {code}"),
            Self::Signaled(signal) => format!("signaled with {signal}"),
            Self::NotFound(error) => format!("not found: {}", escape_text(error)),
            Self::NotExecutable(error) => format!("not executable: {}", escape_text(error)),
            Self::StartFailed(error) => format!("start failed: {}", escape_text(error)),
            Self::Failed(error) => format!("failed: {}", escape_text(error)),
        }
    }
}

/// What happened to one command of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Execution {
    /// The result came from the cache, so nothing was traced.
    Cached {
        /// How the cached run ended.
        outcome: CommandOutcome,
    },
    /// The command ran, or kizz tried to run it.
    Ran {
        /// How it ended, or why it did not run.
        outcome: CommandOutcome,
        /// The tracer's process tree, or `None` when kizz started no process, as
        /// when the lookup found no program or the tracer failed before the start.
        tree: Option<ProcessTree>,
        /// Every location that the program lookup checked, with its outcome.
        lookup: Observations,
    },
}

/// One command, as the report receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInput {
    /// The arguments, starting with the program as given.
    pub argv: Vec<OsString>,
    /// The absolute path of the program, or `None` when the lookup found none.
    pub program: Option<AbsolutePath>,
    /// The working directory.
    pub cwd: AbsolutePath,
    /// What happened to the command.
    pub execution: Execution,
}

/// The task program's own accesses, as the report of `kizz run` receives them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskProgramInput {
    /// The task, as output names it.
    pub task: String,
    /// The paths that the task program touched, with their kinds and outcomes.
    pub observations: Observations,
}

/// The kind of a reason that a command was traced incompletely, in the order
/// the report sorts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasonKind {
    /// A system call through a non-native interface.
    NonNativeSystemCall,
    /// A system call that kizz cannot trace.
    UntraceableSystemCall,
    /// A system call that the tracer does not know.
    UnknownSystemCall,
    /// The tracer itself failed.
    TracerFailed,
    /// Any other reason that the tracer reported.
    Other,
}

impl ReasonKind {
    /// The kind as JSON names it, such as `unknown_system_call`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NonNativeSystemCall => "non_native_system_call",
            Self::UntraceableSystemCall => "untraceable_system_call",
            Self::UnknownSystemCall => "unknown_system_call",
            Self::TracerFailed => "tracer_failed",
            Self::Other => "other",
        }
    }

    /// The kind as text shows it, such as `unknown system call`.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::NonNativeSystemCall => "non-native system call",
            Self::UntraceableSystemCall => "untraceable system call",
            Self::UnknownSystemCall => "unknown system call",
            Self::TracerFailed => "tracer failed",
            Self::Other => "other",
        }
    }
}

/// One reason that a command was traced incompletely.
///
/// Reasons sort by their process, then kind, number and detail.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reason {
    /// The process that caused it, by its number in the report.
    pub process: Option<u32>,
    /// What kind of reason it is.
    pub kind: ReasonKind,
    /// The number of an unknown system call, and `None` for other kinds.
    pub number: Option<u32>,
    /// What the reason is about.
    pub detail: String,
}

impl Reason {
    /// The reason of `reason`, which the tracer reported for `process`.
    fn of_tracer(
        reason: IncompleteReason,
        process: u32,
    ) -> Self {
        let (kind, detail) = match reason {
            IncompleteReason::NonNativeSystemCall { detail } => {
                (ReasonKind::NonNativeSystemCall, detail)
            },
            IncompleteReason::UntraceableSystemCall { detail } => {
                (ReasonKind::UntraceableSystemCall, detail)
            },
            IncompleteReason::Other { detail } => (ReasonKind::Other, detail),
        };
        Self {
            process: Some(process),
            kind,
            number: None,
            detail,
        }
    }

    /// The reason for the unknown system call `number` of `process`.
    fn unknown(
        number: u32,
        process: u32,
    ) -> Self {
        Self {
            process: Some(process),
            kind: ReasonKind::UnknownSystemCall,
            number: Some(number),
            detail: format!("the tracer does not know system call {number}"),
        }
    }
}

/// One program of a process in the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportProgram {
    /// The path of the program, or for a placeholder process the name that kizz
    /// looked up.
    pub path: PathBuf,
    /// The arguments that the program received, starting with its `argv[0]`.
    pub args: Vec<OsString>,
    /// Whether the process runs the program because its creator ran it.
    pub inherited: bool,
    /// The paths that the program touched, with their kinds and outcomes.
    pub observations: Observations,
    /// The connections that the program made.
    pub connections: BTreeSet<Connection>,
}

impl ReportProgram {
    /// The program named `path` with `args`, to which nothing is attributed.
    fn named(
        path: PathBuf,
        args: Vec<OsString>,
    ) -> Self {
        Self {
            path,
            args,
            inherited: false,
            observations: Observations::new(),
            connections: BTreeSet::new(),
        }
    }

    /// The placeholder program of a command that started no process: the name
    /// that kizz looked up, `argv[0]`, with the arguments.
    fn looked_up(argv: &[OsString]) -> Self {
        let name = argv.first().map(PathBuf::from).unwrap_or_default();
        Self::named(name, argv.to_vec())
    }
}

/// One process of a command in the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportProcess {
    /// Its number, unique in the report, counting from 1 across every command.
    pub id: u32,
    /// Its process ID, or `None` when kizz started no process or none is known.
    pub pid: Option<u32>,
    /// The number of the process that created it, or `None`.
    pub parent: Option<u32>,
    /// How it ended, or `None` when the tracer did not see it end.
    pub exit: Option<Termination>,
    /// Its programs, in order.
    pub programs: Vec<ReportProgram>,
}

/// One command of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportCommand {
    /// The arguments, starting with the program as given.
    pub argv: Vec<OsString>,
    /// The absolute path of the program, or `None` when the lookup found none.
    pub program: Option<AbsolutePath>,
    /// The working directory.
    pub cwd: AbsolutePath,
    /// Whether the result came from the cache.
    pub cached: bool,
    /// How it ended, or why it did not run.
    pub outcome: CommandOutcome,
    /// Every reason that it was traced incompletely, sorted, each once.
    pub incomplete: Vec<Reason>,
    /// Its processes, in the order they started; none for a cached command.
    pub processes: Vec<ReportProcess>,
}

impl ReportCommand {
    /// Whether the command was traced completely, or `None` when it came from the
    /// cache.
    #[must_use]
    pub const fn complete(&self) -> Option<bool> {
        if self.cached {
            None
        } else {
            Some(self.incomplete.is_empty())
        }
    }

    /// The command of `input`, whose processes are numbered from `next_id`, which
    /// moves past them.
    fn build(
        input: CommandInput,
        next_id: &mut u32,
    ) -> Self {
        let CommandInput {
            argv,
            program,
            cwd,
            execution,
        } = input;
        let (outcome, tree, lookup) = match execution {
            Execution::Cached { outcome } => {
                return Self {
                    argv,
                    program,
                    cwd,
                    cached: true,
                    outcome,
                    incomplete: Vec::new(),
                    processes: Vec::new(),
                };
            },
            Execution::Ran {
                outcome,
                tree,
                lookup,
            } => (outcome, tree, lookup),
        };
        let (mut processes, mut incomplete) = renumber(tree.unwrap_or_default(), next_id);
        if processes.is_empty() {
            processes.push(ReportProcess {
                id: take_id(next_id),
                pid: None,
                parent: None,
                exit: None,
                programs: Vec::new(),
            });
        }
        let first_id = processes.first().map(|process| process.id);
        if let Some(first) = processes.first_mut() {
            if first.programs.is_empty() {
                first.programs.push(ReportProgram::looked_up(&argv));
            }
            if let Some(program) = first.programs.first_mut() {
                program.observations.merge(&lookup);
            }
        }
        if let CommandOutcome::Failed(message) = &outcome {
            incomplete.push(Reason {
                process: first_id,
                kind: ReasonKind::TracerFailed,
                number: None,
                detail: message.clone(),
            });
        }
        incomplete.sort();
        incomplete.dedup();
        Self {
            argv,
            program,
            cwd,
            cached: false,
            outcome,
            incomplete,
            processes,
        }
    }
}

/// The next process number of the report, moving `next_id` past it.
fn take_id(next_id: &mut u32) -> u32 {
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    id
}

/// The processes of `tree` in their start order, the order of the tracer's
/// numbers, numbered from `next_id`, with their parents renumbered, and every
/// reason that their programs made the trace incomplete.
fn renumber(
    tree: ProcessTree,
    next_id: &mut u32,
) -> (Vec<ReportProcess>, Vec<Reason>) {
    let mut processes = tree.processes;
    processes.sort_by_key(|process| process.number);
    let mut ids = BTreeMap::new();
    let numbered: Vec<_> = processes
        .into_iter()
        .map(|process| {
            let id = take_id(next_id);
            ids.entry(process.number).or_insert(id);
            (id, process)
        })
        .collect();
    let mut reasons = Vec::new();
    let renumbered = numbered
        .into_iter()
        .map(|(id, process)| {
            let programs = process
                .programs
                .into_iter()
                .map(|program| report_program(program, id, &mut reasons))
                .collect();
            ReportProcess {
                id,
                pid: process.pid,
                parent: process.parent.and_then(|parent| ids.get(&parent).copied()),
                exit: process.status,
                programs,
            }
        })
        .collect();
    (renumbered, reasons)
}

/// The report's program of `program`, which the process numbered `process` ran,
/// adding each reason that it made the trace incomplete to `reasons`.
fn report_program(
    program: Program,
    process: u32,
    reasons: &mut Vec<Reason>,
) -> ReportProgram {
    let Program {
        path,
        args,
        inherited,
        observations,
        connections,
        unknown_system_calls,
        incomplete,
    } = program;
    reasons.extend(
        incomplete
            .into_iter()
            .map(|reason| Reason::of_tracer(reason, process)),
    );
    reasons.extend(
        unknown_system_calls
            .into_iter()
            .map(|number| Reason::unknown(number, process)),
    );
    ReportProgram {
        path: path.into_path_buf(),
        args,
        inherited,
        observations,
        connections,
    }
}

/// The task program of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportTaskProgram {
    /// The task, as output names it.
    pub task: String,
    /// The paths that the task program touched, with their kinds and outcomes.
    pub observations: Observations,
}

/// The totals of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Totals {
    /// The commands.
    pub commands: usize,
    /// The commands that came from the cache.
    pub cached: usize,
    /// The processes of every command, placeholder processes included.
    pub processes: usize,
    /// The programs of every process that are not inherited.
    pub programs: usize,
    /// The paths of the report, each once.
    pub paths: usize,
    /// The connections of the report, each once.
    pub connections: usize,
    /// The paths of each access kind, every kind listed.
    pub kinds: BTreeMap<AccessKind, usize>,
}

impl Totals {
    /// The totals of `commands`, whose combined observations and connections,
    /// with the task program's, are `observations` and `connections`.
    fn count(
        commands: &[ReportCommand],
        observations: &Observations,
        connections: &BTreeSet<Connection>,
    ) -> Self {
        let mut totals = Self {
            commands: commands.len(),
            cached: 0,
            processes: 0,
            programs: 0,
            paths: observations.len(),
            connections: connections.len(),
            kinds: AccessKind::ALL.iter().map(|kind| (*kind, 0)).collect(),
        };
        for command in commands {
            if command.cached {
                totals.cached = totals.cached.saturating_add(1);
            }
            for process in &command.processes {
                totals.processes = totals.processes.saturating_add(1);
                for program in &process.programs {
                    if !program.inherited {
                        totals.programs = totals.programs.saturating_add(1);
                    }
                }
            }
        }
        for (_, kinds) in observations {
            for kind in kinds.keys() {
                let count = totals.kinds.entry(*kind).or_default();
                *count = count.saturating_add(1);
            }
        }
        totals
    }

    /// The number of paths of `kind`.
    #[must_use]
    pub fn kind(
        &self,
        kind: AccessKind,
    ) -> usize {
        self.kinds.get(&kind).copied().unwrap_or_default()
    }
}

/// An observation report: its commands in the order they ran, the task program,
/// every observation and connection combined, and the totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The commands, in the order they ran.
    pub commands: Vec<ReportCommand>,
    /// The task program's own accesses, for `kizz run`.
    pub task_program: Option<ReportTaskProgram>,
    /// Every observation of every program of every command and of the task
    /// program, combined.
    pub observations: Observations,
    /// Every connection of every program, combined.
    pub connections: BTreeSet<Connection>,
    /// The totals.
    pub totals: Totals,
}

impl Report {
    /// The report of `commands`, in the order they ran, and of the task program's
    /// own accesses, if any.
    #[must_use]
    pub fn new(
        commands: Vec<CommandInput>,
        task_program: Option<TaskProgramInput>,
    ) -> Self {
        let mut next_id = 1;
        let commands: Vec<ReportCommand> = commands
            .into_iter()
            .map(|command| ReportCommand::build(command, &mut next_id))
            .collect();
        let task_program = task_program.map(|input| {
            ReportTaskProgram {
                task: input.task,
                observations: input.observations,
            }
        });
        let mut observations = Observations::new();
        let mut connections = BTreeSet::new();
        for command in &commands {
            for process in &command.processes {
                for program in &process.programs {
                    observations.merge(&program.observations);
                    connections.extend(program.connections.iter().cloned());
                }
            }
        }
        if let Some(task_program) = &task_program {
            observations.merge(&task_program.observations);
        }
        let totals = Totals::count(&commands, &observations, &connections);
        Self {
            commands,
            task_program,
            observations,
            connections,
            totals,
        }
    }

    /// Whether every command that ran was traced completely; cached commands do
    /// not count.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.commands
            .iter()
            .all(|command| command.complete() != Some(false))
    }

    /// The report in `format` and `mode`.
    #[must_use]
    pub fn render(
        &self,
        format: ReportFormat,
        mode: ReportMode,
    ) -> String {
        match format {
            ReportFormat::Text => self.text(mode),
            ReportFormat::Json => self.json(mode),
        }
    }

    /// The report as text, in `mode`: a header with the completeness and the
    /// totals, each command with its processes in a hierarchical report, the task
    /// program, and in a compact report every observation and connection once.
    #[must_use]
    pub fn text(
        &self,
        mode: ReportMode,
    ) -> String {
        let mut blocks = vec![self.text_header(mode)];
        for (index, command) in self.commands.iter().enumerate() {
            blocks.push(text_command(index.saturating_add(1), command, mode));
        }
        if let Some(task_program) = &self.task_program {
            let mut block = format!("task program: {}\n", escape_text(&task_program.task));
            if mode == ReportMode::Hierarchical {
                text_groups(&mut block, &task_program.observations, None, 1);
            }
            blocks.push(block);
        }
        if mode == ReportMode::Compact {
            let mut block = String::new();
            text_groups(&mut block, &self.observations, Some(&self.connections), 0);
            if !block.is_empty() {
                blocks.push(block);
            }
        }
        blocks.join("\n")
    }

    /// The header of the text report.
    fn text_header(
        &self,
        mode: ReportMode,
    ) -> String {
        let totals = &self.totals;
        let kinds: Vec<String> = AccessKind::ALL
            .iter()
            .map(|kind| format!("{} {}", kind.name(), totals.kind(*kind)))
            .collect();
        format!(
            "kizz observation report, version {VERSION}, {}\ntrace: {}\ntotals: commands {}, cached \
             {}, processes {}, programs {}, paths {}, connections {}\npaths by access: {}\n",
            mode.name(),
            if self.complete() {
                "complete"
            } else {
                "incomplete"
            },
            totals.commands,
            totals.cached,
            totals.processes,
            totals.programs,
            totals.paths,
            totals.connections,
            kinds.join(", ")
        )
    }

    /// The report as JSON, in `mode`, indented by two spaces and ending with a
    /// newline.
    #[must_use]
    pub fn json(
        &self,
        mode: ReportMode,
    ) -> String {
        let totals = &self.totals;
        let kinds = AccessKind::ALL
            .iter()
            .map(|kind| (kind.name(), Json::count(totals.kind(*kind))))
            .collect();
        let mut members = vec![
            ("format", Json::text(FORMAT)),
            ("version", Json::Number(VERSION.to_string())),
            ("mode", Json::text(mode.name())),
            ("complete", Json::Bool(self.complete())),
            (
                "totals",
                Json::Object(vec![
                    ("commands", Json::count(totals.commands)),
                    ("cached", Json::count(totals.cached)),
                    ("processes", Json::count(totals.processes)),
                    ("programs", Json::count(totals.programs)),
                    ("paths", Json::count(totals.paths)),
                    ("connections", Json::count(totals.connections)),
                    ("kinds", Json::Object(kinds)),
                ]),
            ),
            (
                "commands",
                Json::Array(
                    self.commands
                        .iter()
                        .map(|command| json_command(command, mode))
                        .collect(),
                ),
            ),
            (
                "task_program",
                self.task_program
                    .as_ref()
                    .map_or(Json::Null, |task_program| {
                        let mut members = vec![("task", Json::text(&task_program.task))];
                        if mode == ReportMode::Hierarchical {
                            members.push((
                                "observations",
                                json_observations(&task_program.observations),
                            ));
                        }
                        Json::Object(members)
                    }),
            ),
        ];
        if mode == ReportMode::Compact {
            members.push(("observations", json_observations(&self.observations)));
            members.push(("connections", json_connections(&self.connections)));
        }
        let mut text = String::new();
        Json::Object(members).write(&mut text, 0);
        text.push('\n');
        text
    }
}

/// `text` as it appears in a report: each sequence that is not valid UTF-8
/// replaced by U+FFFD.
fn lossy(text: &OsStr) -> String {
    text.to_string_lossy().into_owned()
}

/// `path` as it appears in a report, as [`lossy`] makes it.
fn lossy_path(path: &Path) -> String {
    lossy(path.as_os_str())
}

/// `text` as a text report shows it: a backslash as `\\`, a newline as `\n`, a tab
/// as `\t`, a carriage return as `\r`, every other control character, U+0000
/// through U+001F and U+007F through U+009F, and the separators U+2028 and U+2029
/// as `\u{`, the code in lowercase hexadecimal without leading zeros, and `}`, as
/// [`char::escape_unicode`] writes it, and every other character as it is.
///
/// So no string breaks its line, and since the backslash is escaped too, each
/// escape reads one way and two different strings never give the same text.
#[must_use]
pub fn escape_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            other if other.is_control() || matches!(other, '\u{2028}' | '\u{2029}') => {
                escaped.extend(other.escape_unicode());
            },
            other => escaped.push(other),
        }
    }
    escaped
}

/// `path` as a text report shows it: as [`lossy`] makes it, then escaped as
/// [`escape_text`] escapes it.
fn text_path(path: &Path) -> String {
    escape_text(&lossy_path(path))
}

/// Whether `word` reads the same to a POSIX shell without quotes: it is not empty
/// and holds only letters, digits and `-_./=:,+@%^`.
fn is_plain_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./=:,+@%^".contains(character))
}

/// `args` as text shows them: separated by spaces, each escaped as
/// [`escape_text`] escapes it, then quoted for a POSIX shell when it is empty or
/// holds another character than [`is_plain_word`] allows, so an argument with an
/// escape is always quoted.
fn shell_words(args: &[OsString]) -> String {
    let words: Vec<String> = args
        .iter()
        .map(|arg| {
            let word = escape_text(&lossy(arg));
            if is_plain_word(&word) {
                word
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect();
    words.join(" ")
}

/// `head`, followed by `: ` and the arguments when there are any.
fn with_args(
    head: String,
    args: &[OsString],
) -> String {
    if args.is_empty() {
        head
    } else {
        format!("{head}: {}", shell_words(args))
    }
}

/// Append `depth` levels of two-space indentation to `text`.
fn indent(
    text: &mut String,
    depth: usize,
) {
    for _ in 0..depth {
        text.push_str("  ");
    }
}

/// Append `content`, indented `depth` levels, and a newline to `text`.
fn line(
    text: &mut String,
    depth: usize,
    content: &str,
) {
    indent(text, depth);
    text.push_str(content);
    text.push('\n');
}

/// `outcomes`, separated by `, `, in their order, each escaped as [`escape_text`]
/// escapes it.
fn outcome_list<'a>(outcomes: impl Iterator<Item = &'a Outcome>) -> String {
    let outcomes: Vec<String> = outcomes
        .map(|outcome| escape_text(&outcome.to_string()))
        .collect();
    outcomes.join(", ")
}

/// A connection as text shows it: `ip ADDRESS`, `unix PATH` or `abstract NAME`,
/// with the path and the name as [`lossy`] makes them, escaped as [`escape_text`]
/// escapes them.
fn connection_text(connection: &Connection) -> String {
    match connection {
        Connection::Ip(address) => format!("ip {address}"),
        Connection::UnixPath(path) => format!("unix {}", text_path(path.as_path())),
        Connection::UnixAbstract(name) => {
            format!("abstract {}", escape_text(&String::from_utf8_lossy(name)))
        },
    }
}

/// Append to `text`, at `depth`, each access kind of `observations` that has
/// paths, with its paths as `PATH: OUTCOMES`, then the `connections` group when
/// `connections` has any.
fn text_groups(
    text: &mut String,
    observations: &Observations,
    connections: Option<&BTreeSet<Connection>>,
    depth: usize,
) {
    let inner = depth.saturating_add(1);
    for kind in AccessKind::ALL {
        let paths: Vec<_> = observations
            .iter()
            .filter_map(|(path, kinds)| kinds.get(&kind).map(|outcomes| (path, outcomes)))
            .collect();
        if paths.is_empty() {
            continue;
        }
        line(text, depth, &format!("{}:", kind.name()));
        for (path, outcomes) in paths {
            line(
                text,
                inner,
                &format!(
                    "{}: {}",
                    text_path(path.as_path()),
                    outcome_list(outcomes.iter())
                ),
            );
        }
    }
    if let Some(connections) = connections.filter(|connections| !connections.is_empty()) {
        line(text, depth, "connections:");
        for connection in connections {
            line(text, inner, &connection_text(connection));
        }
    }
}

/// How a process ended, as text shows it: `exited with C` or `signaled with S`.
fn exit_text(exit: Termination) -> String {
    match exit {
        Termination::Exited(code) => format!("exited with {code}"),
        Termination::Signaled(signal) => format!("signaled with {signal}"),
    }
}

/// The line of `process`: `process N`, then `pid P` or `no pid`, `child of
/// process M` when it has a parent, and how it ended; a process without a process
/// ID or status is `not started`.
fn process_line(process: &ReportProcess) -> String {
    if process.pid.is_none() && process.exit.is_none() {
        return format!("process {}, not started", process.id);
    }
    let mut parts = vec![format!("process {}", process.id)];
    parts.push(
        process
            .pid
            .map_or_else(|| "no pid".to_owned(), |pid| format!("pid {pid}")),
    );
    if let Some(parent) = process.parent {
        parts.push(format!("child of process {parent}"));
    }
    parts.push(
        process
            .exit
            .map_or_else(|| "end not seen".to_owned(), exit_text),
    );
    parts.join(", ")
}

/// The line of `reason`, with its process in a hierarchical report.
fn reason_line(
    reason: &Reason,
    mode: ReportMode,
) -> String {
    match (mode, reason.process) {
        (ReportMode::Hierarchical, Some(process)) => {
            format!(
                "{}, process {process}: {}",
                reason.kind.text(),
                escape_text(&reason.detail)
            )
        },
        _ => format!("{}: {}", reason.kind.text(), escape_text(&reason.detail)),
    }
}

/// The text block of the command numbered `number`.
fn text_command(
    number: usize,
    command: &ReportCommand,
    mode: ReportMode,
) -> String {
    let mut text = String::new();
    line(
        &mut text,
        0,
        &with_args(format!("command {number}"), &command.argv),
    );
    let program = command.program.as_ref().map_or_else(
        || "not found".to_owned(),
        |program| text_path(program.as_path()),
    );
    line(&mut text, 1, &format!("program: {program}"));
    line(
        &mut text,
        1,
        &format!("directory: {}", text_path(command.cwd.as_path())),
    );
    line(
        &mut text,
        1,
        &format!("outcome: {}", command.outcome.text()),
    );
    let trace = match command.complete() {
        None => "cached, not traced",
        Some(true) => "complete",
        Some(false) => "incomplete",
    };
    line(&mut text, 1, &format!("trace: {trace}"));
    for reason in &command.incomplete {
        line(&mut text, 2, &reason_line(reason, mode));
    }
    if mode == ReportMode::Hierarchical {
        text_processes(&mut text, &command.processes);
    }
    text
}

/// Append the processes of one command to `text`, each nested under the process
/// that created it, in the order they started.
///
/// A process is nested under its parent only when the parent started before it,
/// so every process appears once, whatever the parents say.
fn text_processes(
    text: &mut String,
    processes: &[ReportProcess],
) {
    let positions: BTreeMap<u32, usize> = processes
        .iter()
        .enumerate()
        .rev()
        .map(|(position, process)| (process.id, position))
        .collect();
    let mut children: Vec<Vec<&ReportProcess>> = processes.iter().map(|_| Vec::new()).collect();
    let mut roots = Vec::new();
    for (position, process) in processes.iter().enumerate() {
        let parent = process
            .parent
            .and_then(|parent| positions.get(&parent).copied())
            .filter(|parent| *parent < position);
        match parent.and_then(|parent| children.get_mut(parent)) {
            Some(siblings) => siblings.push(process),
            None => roots.push(process),
        }
    }
    let mut stack: Vec<(&ReportProcess, usize)> = roots
        .into_iter()
        .rev()
        .map(|process| (process, 1))
        .collect();
    while let Some((process, depth)) = stack.pop() {
        line(text, depth, &process_line(process));
        let inner = depth.saturating_add(1);
        for program in &process.programs {
            let head = if program.inherited {
                format!("program {}, inherited", text_path(&program.path))
            } else {
                format!("program {}", text_path(&program.path))
            };
            line(text, inner, &with_args(head, &program.args));
            text_groups(
                text,
                &program.observations,
                Some(&program.connections),
                inner.saturating_add(1),
            );
        }
        let own = positions
            .get(&process.id)
            .and_then(|position| children.get(*position));
        for child in own.into_iter().flatten().rev() {
            stack.push((*child, inner));
        }
    }
}

/// A JSON value of the report, whose objects keep their members in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number, already written in decimal.
    Number(String),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Self>),
    /// An object, with its members in order.
    Object(Vec<(&'static str, Self)>),
}

impl Json {
    /// The string `text`.
    fn text(text: &str) -> Self {
        Self::String(text.to_owned())
    }

    /// The number `count`.
    fn count(count: usize) -> Self {
        Self::Number(count.to_string())
    }

    /// `value` as a number, or `null` when it is `None`.
    fn optional<T: ToString>(value: Option<T>) -> Self {
        value.map_or(Self::Null, |value| Self::Number(value.to_string()))
    }

    /// Append the value, nested `level` deep, to `text`: each element and member
    /// on a line of its own, indented by two spaces per level, with `": "` after
    /// each key, and empty arrays and objects as `[]` and `{}`.
    fn write(
        &self,
        text: &mut String,
        level: usize,
    ) {
        let inner = level.saturating_add(1);
        match self {
            Self::Null => text.push_str("null"),
            Self::Bool(value) => text.push_str(if *value { "true" } else { "false" }),
            Self::Number(number) => text.push_str(number),
            Self::String(value) => write_json_string(text, value),
            Self::Array(items) if items.is_empty() => text.push_str("[]"),
            Self::Object(members) if members.is_empty() => text.push_str("{}"),
            Self::Array(items) => {
                text.push('[');
                for (index, item) in items.iter().enumerate() {
                    text.push_str(if index == 0 { "\n" } else { ",\n" });
                    indent(text, inner);
                    item.write(text, inner);
                }
                text.push('\n');
                indent(text, level);
                text.push(']');
            },
            Self::Object(members) => {
                text.push('{');
                for (index, (key, member)) in members.iter().enumerate() {
                    text.push_str(if index == 0 { "\n" } else { ",\n" });
                    indent(text, inner);
                    write_json_string(text, key);
                    text.push_str(": ");
                    member.write(text, inner);
                }
                text.push('\n');
                indent(text, level);
                text.push('}');
            },
        }
    }
}

/// Append `value` to `text` as a JSON string, escaped as `serde_json` escapes it:
/// `"` and `\` with a backslash, the control characters with a short escape where
/// JSON has one and `\u00XX` otherwise, and everything else as it is.
fn write_json_string(
    text: &mut String,
    value: &str,
) {
    text.push('"');
    for character in value.chars() {
        match character {
            '"' => text.push_str("\\\""),
            '\\' => text.push_str("\\\\"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            '\u{08}' => text.push_str("\\b"),
            '\u{0c}' => text.push_str("\\f"),
            control if control < ' ' => {
                // Below U+0020, the code is `0x1N` or `0x0N`: its high digit is
                // the bit 0x10, and its low digit the bits 0x0F.
                let code = u32::from(control);
                text.push_str(if code & 0x10 == 0 { "\\u000" } else { "\\u001" });
                let low = usize::try_from(code & 0x0F).unwrap_or_default();
                text.push(char::from(
                    b"0123456789abcdef".get(low).copied().unwrap_or(b'0'),
                ));
            },
            other => text.push(other),
        }
    }
    text.push('"');
}

/// The strings of `args`, as [`lossy`] makes them.
fn json_args(args: &[OsString]) -> Json {
    Json::Array(args.iter().map(|arg| Json::String(lossy(arg))).collect())
}

/// The JSON of `outcome`: its `kind`, with `code`, `signal` or `error`.
fn json_outcome(outcome: &CommandOutcome) -> Json {
    let detail = match outcome {
        CommandOutcome::Exited(code) => ("code", Json::Number(code.to_string())),
        CommandOutcome::Signaled(signal) => ("signal", Json::Number(signal.to_string())),
        CommandOutcome::NotFound(error)
        | CommandOutcome::NotExecutable(error)
        | CommandOutcome::StartFailed(error)
        | CommandOutcome::Failed(error) => ("error", Json::text(error)),
    };
    Json::Object(vec![("kind", Json::text(outcome.kind())), detail])
}

/// The JSON of how a process ended: `exited`, with `code`, or `signaled`, with
/// `signal`.
fn json_exit(exit: Termination) -> Json {
    match exit {
        Termination::Exited(code) => {
            Json::Object(vec![
                ("kind", Json::text("exited")),
                ("code", Json::Number(code.to_string())),
            ])
        },
        Termination::Signaled(signal) => {
            Json::Object(vec![
                ("kind", Json::text("signaled")),
                ("signal", Json::Number(signal.to_string())),
            ])
        },
    }
}

/// The JSON of `reason`, with its process in a hierarchical report.
fn json_reason(
    reason: &Reason,
    mode: ReportMode,
) -> Json {
    let mut members = vec![
        ("kind", Json::text(reason.kind.name())),
        ("detail", Json::text(&reason.detail)),
        ("number", Json::optional(reason.number)),
    ];
    if mode == ReportMode::Hierarchical {
        members.push(("process", Json::optional(reason.process)));
    }
    Json::Object(members)
}

/// The JSON of `observations`: each path with its `access` object, which maps each
/// access kind of the path to its outcomes.
fn json_observations(observations: &Observations) -> Json {
    Json::Array(
        observations
            .iter()
            .map(|(path, kinds)| {
                let access = kinds
                    .iter()
                    .map(|(kind, outcomes)| {
                        (
                            kind.name(),
                            Json::Array(
                                outcomes
                                    .iter()
                                    .map(|outcome| Json::String(outcome.to_string()))
                                    .collect(),
                            ),
                        )
                    })
                    .collect();
                Json::Object(vec![
                    ("path", Json::String(lossy_path(path.as_path()))),
                    ("access", Json::Object(access)),
                ])
            })
            .collect(),
    )
}

/// The JSON of `connections`: each with its `kind`, `ip`, `unix` or `abstract`,
/// and its `address`, `path` or `name`.
fn json_connections(connections: &BTreeSet<Connection>) -> Json {
    Json::Array(
        connections
            .iter()
            .map(|connection| {
                let (kind, member) = match connection {
                    Connection::Ip(address) => {
                        ("ip", ("address", Json::String(address.to_string())))
                    },
                    Connection::UnixPath(path) => {
                        ("unix", ("path", Json::String(lossy_path(path.as_path()))))
                    },
                    Connection::UnixAbstract(name) => {
                        (
                            "abstract",
                            (
                                "name",
                                Json::String(String::from_utf8_lossy(name).into_owned()),
                            ),
                        )
                    },
                };
                Json::Object(vec![("kind", Json::text(kind)), member])
            })
            .collect(),
    )
}

/// The JSON of `program`.
fn json_program(program: &ReportProgram) -> Json {
    Json::Object(vec![
        ("path", Json::String(lossy_path(&program.path))),
        ("args", json_args(&program.args)),
        ("inherited", Json::Bool(program.inherited)),
        ("observations", json_observations(&program.observations)),
        ("connections", json_connections(&program.connections)),
    ])
}

/// The JSON of `process`.
fn json_process(process: &ReportProcess) -> Json {
    Json::Object(vec![
        ("id", Json::Number(process.id.to_string())),
        ("pid", Json::optional(process.pid)),
        ("parent", Json::optional(process.parent)),
        ("exit", process.exit.map_or(Json::Null, json_exit)),
        (
            "programs",
            Json::Array(process.programs.iter().map(json_program).collect()),
        ),
    ])
}

/// The JSON of `command`, with its processes in a hierarchical report.
fn json_command(
    command: &ReportCommand,
    mode: ReportMode,
) -> Json {
    let mut members = vec![
        ("argv", json_args(&command.argv)),
        (
            "program",
            command.program.as_ref().map_or(Json::Null, |program| {
                Json::String(lossy_path(program.as_path()))
            }),
        ),
        ("cwd", Json::String(lossy_path(command.cwd.as_path()))),
        ("cached", Json::Bool(command.cached)),
        ("outcome", json_outcome(&command.outcome)),
        (
            "complete",
            command.complete().map_or(Json::Null, Json::Bool),
        ),
        (
            "incomplete",
            Json::Array(
                command
                    .incomplete
                    .iter()
                    .map(|reason| json_reason(reason, mode))
                    .collect(),
            ),
        ),
    ];
    if mode == ReportMode::Hierarchical {
        members.push((
            "processes",
            Json::Array(command.processes.iter().map(json_process).collect()),
        ));
    }
    Json::Object(members)
}

/// Unit tests of the report's private helpers.
#[cfg(test)]
#[allow(clippy::disallowed_macros, reason = "test code: the tests assert")]
mod tests {
    use std::ffi::OsString;

    use super::{Json, escape_text, is_plain_word, shell_words, write_json_string};

    /// Strings are escaped as `serde_json` escapes them, so it reads them back.
    #[test]
    fn strings_escape_as_serde_json_does() -> Result<(), serde_json::Error> {
        let value = format!(
            "quote \" backslash \\ newline \n tab \t bell \u{07} back \u{08} feed \u{0c} return \
             \r unit \u{1f} delete \u{7f} replacement {}",
            char::REPLACEMENT_CHARACTER
        );
        let mut text = String::new();
        write_json_string(&mut text, &value);
        assert_eq!(text, serde_json::to_string(&value)?);
        assert_eq!(serde_json::from_str::<String>(&text)?, value);
        Ok(())
    }

    /// Empty arrays and objects are written as `[]` and `{}`, and members keep
    /// their order.
    #[test]
    fn values_write_in_order() {
        let mut text = String::new();
        Json::Object(vec![
            ("b", Json::Array(Vec::new())),
            ("a", Json::Object(Vec::new())),
            ("c", Json::Array(vec![Json::Null, Json::Bool(false)])),
        ])
        .write(&mut text, 0);
        assert_eq!(
            text,
            "{\n  \"b\": [],\n  \"a\": {},\n  \"c\": [\n    null,\n    false\n  ]\n}"
        );
    }

    /// A word is quoted when it is empty or holds a character outside the plain
    /// set, and a single quote in it is written as `'\''`.
    #[test]
    fn words_are_quoted_for_a_shell() {
        assert!(is_plain_word("a-Z_0./=:,+@%^"));
        assert!(!is_plain_word(""));
        assert!(!is_plain_word("a b"));
        assert!(!is_plain_word("caf\u{e9}"));
        let args: Vec<OsString> = ["cc", "", "it's", "$HOME", "a*b"]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert_eq!(shell_words(&args), "cc '' 'it'\\''s' '$HOME' 'a*b'");
    }

    /// Each escape of text: a backslash, a newline, a tab and a carriage return
    /// with their short escapes, every other control character and the two
    /// separators as `\u{...}` in lowercase hexadecimal without leading zeros.
    #[test]
    fn text_escapes_backslashes_and_control_characters() {
        let cases = [
            ("\\", "\\\\"),
            ("\n", "\\n"),
            ("\t", "\\t"),
            ("\r", "\\r"),
            ("\u{0}", "\\u{0}"),
            ("\u{1}", "\\u{1}"),
            ("\u{1b}", "\\u{1b}"),
            ("\u{1f}", "\\u{1f}"),
            ("\u{7f}", "\\u{7f}"),
            ("\u{85}", "\\u{85}"),
            ("\u{9f}", "\\u{9f}"),
            ("\u{2028}", "\\u{2028}"),
            ("\u{2029}", "\\u{2029}"),
            ("a\nb\\c", "a\\nb\\\\c"),
        ];
        for (text, escaped) in cases {
            assert_eq!(escape_text(text), escaped, "{text:?}");
        }
    }

    /// Every other character stays as it is, such as letters, quotes, `é`, U+FFFD,
    /// U+00A0 and U+200B, so a string without a backslash or a control character
    /// reads as before.
    #[test]
    fn text_keeps_every_other_character() {
        for text in [
            "plain /work/main.c",
            "it's \"quoted\" $HOME",
            "caf\u{e9}",
            "�",
            "\u{a0}",
            "\u{200b}",
            "\u{1f600}",
            "",
        ] {
            assert_eq!(escape_text(text), text, "{text:?}");
        }
    }

    /// Two strings that would read alike without the backslash's escape read
    /// differently, such as a newline and a backslash followed by `n`.
    #[test]
    fn escapes_keep_different_strings_apart() {
        let pairs = [
            ("\n", "\\n"),
            ("\t", "\\t"),
            ("\r", "\\r"),
            ("\u{1b}", "\\u{1b}"),
            ("\u{0}", "\\u{0}"),
            ("\u{2028}", "\\u{2028}"),
            ("a\\", "a\\\\"),
        ];
        for (first, second) in pairs {
            assert_ne!(first, second);
            assert_ne!(
                escape_text(first),
                escape_text(second),
                "{first:?} {second:?}"
            );
        }
    }

    /// An argument is escaped before it is quoted, so an argument with an escape is
    /// always quoted, and the escape of a single quote stays as it is.
    #[test]
    fn arguments_are_escaped_before_they_are_quoted() {
        let args: Vec<OsString> = ["a\nb", "back\\slash", "it's\t"]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert_eq!(shell_words(&args), "'a\\nb' 'back\\\\slash' 'it'\\''s\\t'");
    }
}
