// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz list` command: the discovered tasks, or those a target expression
//! selects, with every discovery warning. It does not use the cache.

use std::{env, process::ExitCode};

use kizz_core::{LoadError, TargetExpr, Warning, WorkerPool, load};

use crate::{cli::USAGE_ERROR, ui};

/// List the tasks under the current directory that `targets` selects, or every task
/// without it, and return the exit status.
///
/// A malformed target expression, an unreadable current directory, configuration
/// errors in a `kizz.md`, and a worker pool for discovery that cannot start exit with
/// [`USAGE_ERROR`]. Configuration errors still show every warning found while
/// loading, and repeat them in the final summary.
pub(crate) fn run(targets: Option<&str>) -> ExitCode {
    let expression = match targets.map(str::parse::<TargetExpr>).transpose() {
        Ok(expression) => expression,
        Err(error) => return usage_error(&error.to_string()),
    };
    let root = match env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            return usage_error(&format!("cannot read the current directory: {error}"));
        },
    };
    let pool = match WorkerPool::new() {
        Ok(pool) => pool,
        Err(error) => return usage_error(&error.to_string()),
    };
    let project = match load(&root, &pool) {
        Ok(project) => project,
        Err(LoadError::Config { errors, warnings }) => {
            let warnings = print_warnings(&warnings);
            for error in &errors {
                ui::print_error(&error.to_string());
            }
            ui::print_warning_summary(&warnings);
            return ExitCode::from(USAGE_ERROR);
        },
        Err(error) => return usage_error(&error.to_string()),
    };

    let warnings = print_warnings(&project.warnings);
    let all = ui::count(project.tasks.len(), "task");
    if let Some(expression) = expression {
        let selected = expression.select(&project.tasks);
        for chosen in &selected {
            ui::print_task(&chosen.task.location(), &chosen.task.targets);
        }
        ui::print_summary(&format!(
            "{} of {all} selected by `{expression}`.",
            selected.len()
        ));
    } else {
        for task in &project.tasks {
            ui::print_task(&task.location(), &task.targets);
        }
        ui::print_summary(&format!(
            "{all} in {}.",
            ui::count(project.files.len(), "kizz.md file")
        ));
    }
    ui::print_warning_summary(&warnings);
    ExitCode::SUCCESS
}

/// Print each warning loudly and return their messages for the final summary.
fn print_warnings(warnings: &[Warning]) -> Vec<String> {
    let messages: Vec<String> = warnings.iter().map(ToString::to_string).collect();
    for message in &messages {
        ui::print_warning(message);
    }
    messages
}

/// Print `message` as an error and return the usage-error status.
fn usage_error(message: &str) -> ExitCode {
    ui::print_error(message);
    ExitCode::from(USAGE_ERROR)
}
