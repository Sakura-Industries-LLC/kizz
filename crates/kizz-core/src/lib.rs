// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz build engine.
//!
//! This crate holds discovery, Markdown parsing, the Starlark runtime, step keys,
//! cache checks, explanations, ordering, observation reports, and running
//! `kizz execute`. It contains no Linux-specific code: it runs commands only through
//! the tracer interface of `kizz_trace`, reads extended attributes only through that
//! interface's attribute query, and keeps its records in `kizz_store`, so its tests
//! use a fake tracer.
//!
//! * [`discovery`]: every `kizz.md` under the current directory, in the default order.
//! * [`tasks`]: the task blocks of a `kizz.md`.
//! * [`targets`]: target names, target expressions joined with `+`, and task selection.
//! * [`project`]: discovery and parsing together, with their configuration errors.
//! * [`runtime`]: the Starlark runtime of task programs, with module loading, the program
//!   hash, and the `env` and `fs` functions.
//! * [`dotenv`]: the dotenv files that `env.load_file` and `env.read` read.
//! * [`files`]: the file operations behind the `fs` functions, which record every access
//!   and every file a write changed.
//! * [`step`]: the step specification that `run()` builds, and the step executor the
//!   runtime hands it to.
//! * [`warning`]: the warnings of discovery and parsing.
//! * [`key`]: step keys, derived with BLAKE3 from a deterministic CBOR encoding.
//! * [`lookup`]: program lookup through the `PATH` of a task environment or of the caller
//!   of `kizz execute`.
//! * [`patterns`]: the path patterns of `ignore` and `track`.
//! * [`observe`](mod@observe): the recorded state of every path a step touched.
//! * [`states`]: the in-memory concurrent cache of file states and hashes of one build,
//!   so a file is hashed once per build.
//! * [`check`](mod@check): the cache check, which compares a step's recorded observations
//!   with the file system.
//! * [`engine`]: the engine side of the step executor, which checks the cache for each
//!   step, replays a hit, and otherwise runs the step through the tracer and stores its
//!   record, unless the result is not cached.
//! * [`explain`](mod@explain): why each step ran, from the cache check, the step-position
//!   records, and the last-writer records, with the warning for steps that re-run for a
//!   self-inflicted reason.
//! * [`render`]: the one-line and full reasons as output shows them.
//! * [`why`](mod@why): the report of `kizz why`, read from the cache.
//! * [`report`]: observation reports, built from the tracer's process trees and the
//!   commands' outcomes, and written as text or JSON, hierarchical or compact.
//! * [`execute`](mod@execute): running one command for `kizz execute`, with the caller's
//!   environment and working directory, the kept terminal and the process tree, and its
//!   exit status and observation report, without the cache.
//! * [`build`](mod@build): building the selected tasks in passes, in the default or
//!   learned order, until a pass does no work, with failure handling and the pass limit.
//! * [`cleanup`]: the entries `kizz cache clean` removes because their files are gone,
//!   and the entries `kizz cache reset` wipes for the tasks under a path.
//! * [`pool`]: the worker pool that runs a command's parallel work, whose threads start
//!   through `kizz_threads`, so `rayon`'s global pool never starts.
//!
//! State that one thread changes through shared references lives in a `Cell`, which
//! the private `cells` module takes out, changes and puts back, where a `RefCell`
//! borrow would panic while the cell is borrowed; state that threads share is locked
//! with `kizz_threads::lock`, where `Mutex::lock` may panic ("Kizz never panics").

pub mod build;
mod cells;
pub mod check;
pub mod cleanup;
pub mod discovery;
pub mod dotenv;
pub mod engine;
pub mod execute;
pub mod explain;
pub mod files;
pub mod key;
pub mod lookup;
pub mod observe;
pub mod patterns;
pub mod pool;
pub mod project;
pub mod render;
pub mod report;
pub mod runtime;
pub mod states;
pub mod step;
pub mod targets;
pub mod tasks;
pub mod warning;
pub mod why;

pub use build::{
    BuildEvents, BuildOutcome, BuildReport, ChangingInput, FailedTask, INTERRUPTED, NoEvents, Pass,
    ProgramWrite, TaskResult, UncheckedStep, build, learned_order, pass_limit, task_id, task_label,
};
pub use check::{Checked, RACY_WINDOW_NANOS, check};
pub use cleanup::{Cleaned, ResetPlan, clean, plan_reset};
pub use discovery::{Discovery, KIZZ_FILE_NAME, KizzFile, default_order, discover};
pub use dotenv::DotenvError;
pub use engine::{Engine, StepConnections, TaskSteps, UnknownCalls};
pub use execute::{
    ExecuteRequest, Executed, STATUS_FAILED, STATUS_NOT_EXECUTABLE, STATUS_NOT_FOUND, execute,
    exit_status, not_executed,
};
pub use explain::{
    Baseline, InputContext, RepeatedWarning, StepDecision, TaskIdentity, carried_from,
    changed_inputs, spec_reasons,
};
pub use files::{ChangedFile, FileChanges, FileError, Files, stable_json};
pub use key::{KeyError, Occurrences, STEP_KEY_CONTEXT, step_key};
pub use lookup::{
    NoPathError, PATH_VARIABLE, ProgramLookup, ProgramSearch, find_program, search_program,
};
pub use observe::{
    HashCounter, Observed, access_bits, changed_after_read, is_kernel_path, kernel_links, observe,
    path_state, reaches_kernel, record_attributes,
};
pub use patterns::{PathPatterns, PatternError, REGEX_PREFIX};
pub use pool::{MAX_WORKERS, PoolError, WorkerPool};
pub use project::{ConfigError, LoadError, Project, load};
pub use render::{CACHED, Render, access_kinds};
pub use report::{
    CommandInput, CommandOutcome, Execution, JSON_FIELDS, JsonField, Report, ReportFormat,
    ReportMode, TaskProgramInput,
};
pub use runtime::{
    PrintSink, ProgramHash, ProgramLoad, ProgramParts, TaskFailure, TaskInfo, TaskProgram, TaskRun,
    UncheckedExit, UnexpectedExit,
};
pub use states::FileStates;
pub use step::{StepError, StepExecutor, StepOutcome, StepSpec};
pub use targets::{Selected, TARGET_NAME_RULE, TargetExpr, TargetExprError, is_valid_target};
pub use tasks::{
    OptionProblem, ParsedFile, TASK_LANGUAGE, TASK_NAME_RULE, TASK_OPTION_KEYS, Task, TaskOptions,
    parse_tasks,
};
pub use warning::Warning;
pub use why::{WhyReport, WhyStep, WhyTask, why};
