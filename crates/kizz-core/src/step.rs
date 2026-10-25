// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The boundary between task programs and the engine.
//!
//! Each `run()` call in a task program builds a [`StepSpec`], the step
//! specification, and hands it to the [`StepExecutor`] the task runs with. The
//! executor decides how the step is run, traced, and cached, and returns a
//! [`StepOutcome`]: how the command ended, its raw captured output, and whether the
//! outcome came from the cache. Tests implement [`StepExecutor`] with a fake.

use std::{collections::BTreeMap, fmt};

use kizz_trace::{AbsolutePath, CaptureMode, CapturedOutput, Termination};

/// One step, as a `run()` call in a task program specifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSpec {
    /// The argument vector exactly as written: the program, then its arguments.
    pub argv: Vec<String>,
    /// The text for standard input, or `None` to connect `/dev/null`.
    pub stdin: Option<String>,
    /// The exact environment of the command: the task environment with the
    /// per-run `env` applied.
    pub env: BTreeMap<String, String>,
    /// The absolute working directory: the task directory, or `cwd` resolved
    /// against it.
    pub cwd: AbsolutePath,
    /// How standard output and standard error are captured.
    pub capture: CaptureMode,
    /// Whether output is also echoed live to the terminal.
    pub stream: bool,
    /// Whether the command runs on every build, even when cached.
    pub always: bool,
    /// The user exceptions: patterns of paths not recorded as dependencies.
    pub ignore: Vec<String>,
    /// The `/proc` and `/sys` files whose content counts as an input.
    pub track: Vec<String>,
    /// Whether a different modification or change time of a file or directory that
    /// the step read, ran, listed, or checked is a change, even when the content is
    /// the same.
    pub timestamps: bool,
    /// The acceptable exit codes, or `None` when the exit status never fails the
    /// task.
    pub expect: Option<Vec<i32>>,
}

impl StepSpec {
    /// The command as messages show it: the argument vector joined with spaces.
    #[must_use]
    pub fn command(&self) -> String {
        self.argv.join(" ")
    }
}

/// The outcome of one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    /// How the command ended.
    pub termination: Termination,
    /// The raw captured output, exactly as the command wrote it or as the cache
    /// recorded it.
    pub output: CapturedOutput,
    /// Whether the outcome came from the cache.
    pub cached: bool,
}

/// A step the executor could not run, such as a program that does not exist.
///
/// It fails the task with its message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct StepError {
    /// What went wrong.
    pub message: String,
}

impl StepError {
    /// An error with `message`.
    #[must_use]
    pub fn new(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

/// Runs the steps of a task program.
///
/// The runtime calls [`StepExecutor::execute`] once for each `run()` call, in
/// program order, and returns the outcome to the program.
pub trait StepExecutor {
    /// Run one step and return its outcome.
    ///
    /// # Errors
    ///
    /// Returns [`StepError`] when the step cannot be run; the task then fails.
    fn execute(
        &self,
        step: &StepSpec,
    ) -> Result<StepOutcome, StepError>;
}
