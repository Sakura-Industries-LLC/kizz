// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz build engine.
//!
//! This crate holds discovery, Markdown parsing, the Starlark runtime, step keys,
//! cache checks, explanations, and ordering. It contains no Linux-specific code: it
//! runs commands only through the tracer interface of `kizz_trace` and keeps its
//! records in `kizz_store`, so its tests use a fake tracer.
//!
//! * [`discovery`]: every `kizz.md` under the current directory, in the default order.
//! * [`tasks`]: the task blocks of a `kizz.md`.
//! * [`targets`]: target names, target expressions joined with `+`, and task selection.
//! * [`project`]: discovery and parsing together, with their configuration errors.
//! * [`runtime`]: the Starlark runtime of task programs, with module loading, the program
//!   hash, and the `env` and `fs` functions.
//! * [`dotenv`]: the dotenv files that `env.load_file` and `env.read` read.
//! * [`files`]: the file operations behind the `fs` functions, which record every access.
//! * [`step`]: the step specification that `run()` builds, and the step executor the
//!   runtime hands it to.
//! * [`warning`]: the warnings of discovery and parsing.
//! * [`key`]: step keys, derived with BLAKE3 from a deterministic CBOR encoding.
//! * [`lookup`]: program lookup through the task environment's `PATH`.
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
//! * [`build`](mod@build): building the selected tasks in passes, in the default or
//!   learned order, until a pass runs nothing, with failure handling and the pass limit.

pub mod build;
pub mod check;
pub mod discovery;
pub mod dotenv;
pub mod engine;
pub mod explain;
pub mod files;
pub mod key;
pub mod lookup;
pub mod observe;
pub mod patterns;
pub mod project;
pub mod render;
pub mod runtime;
pub mod states;
pub mod step;
pub mod targets;
pub mod tasks;
pub mod warning;
pub mod why;

pub use build::{
    BuildEvents, BuildOutcome, BuildReport, ChangingInput, FailedTask, INTERRUPTED, NoEvents, Pass,
    TaskResult, build, learned_order, pass_limit, task_id, task_label,
};
pub use check::{Checked, RACY_WINDOW_NANOS, check};
pub use discovery::{Discovery, KIZZ_FILE_NAME, KizzFile, default_order, discover};
pub use dotenv::DotenvError;
pub use engine::{Engine, TaskSteps};
pub use explain::{
    Baseline, InputContext, RepeatedWarning, StepDecision, TaskIdentity, carried_from,
    changed_inputs, spec_reasons,
};
pub use files::{FileError, Files, stable_json};
pub use key::{KeyError, Occurrences, STEP_KEY_CONTEXT, step_key};
pub use lookup::{NoPathError, PATH_VARIABLE, ProgramLookup, find_program};
pub use observe::{HashCounter, Observed, access_bits, is_kernel_path, observe, path_state};
pub use patterns::{PathPatterns, PatternError, REGEX_PREFIX};
pub use project::{ConfigError, LoadError, Project, load};
pub use render::{CACHED, Render, access_kinds};
pub use runtime::{
    PrintSink, ProgramHash, ProgramLoad, ProgramParts, TaskFailure, TaskInfo, TaskProgram, TaskRun,
    UnexpectedExit,
};
pub use states::FileStates;
pub use step::{StepError, StepExecutor, StepOutcome, StepSpec};
pub use targets::{Selected, TARGET_NAME_RULE, TargetExpr, TargetExprError, is_valid_target};
pub use tasks::{ParsedFile, TASK_LANGUAGE, Task, parse_tasks};
pub use warning::Warning;
pub use why::{WhyReport, WhyStep, WhyTask, why};
