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
//! * [`warning`]: the warnings of discovery and parsing.

pub mod discovery;
pub mod project;
pub mod targets;
pub mod tasks;
pub mod warning;

pub use discovery::{Discovery, KIZZ_FILE_NAME, KizzFile, default_order, discover};
pub use project::{ConfigError, LoadError, Project, load};
pub use targets::{Selected, TARGET_NAME_RULE, TargetExpr, TargetExprError, is_valid_target};
pub use tasks::{ParsedFile, TASK_LANGUAGE, Task, parse_tasks};
pub use warning::Warning;
