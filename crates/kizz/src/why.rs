// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz why` command: the reasons stored by the most recent build for the tasks
//! under a path, the current directory by default, with each step's untracked
//! `/proc` and `/sys` accesses and how many accesses it ignored.
//!
//! It reads the cache under the cache lock, as the cache commands do, so it waits
//! for a running build and says what it is waiting for. Environment values are
//! shown only with `--verbose`, because they can be secrets.
//!
//! The report and its warnings go through the `ui` helpers, which drop a write
//! that fails, such as one to a closed pipe, where the `print!` macros would panic
//! ("Kizz never panics"); the command still exits with the status of its work.

use std::{
    env,
    path::{Path, PathBuf},
    process::ExitCode,
};

use kizz_core::{Render, why};
use kizz_store::{CacheLock, LockDetails, Store, Verdict, cache_dir_from_env};

use crate::{cli::USAGE_ERROR, ui};

/// Print the reasons stored by the most recent build for the tasks under `path`,
/// relative to the current directory, or under the current directory without it,
/// filtering by task cache status when `cached` is set and showing environment
/// values when `verbose` is set. Return the exit status.
///
/// A cache that cannot be located, locked, opened, or read exits with
/// [`USAGE_ERROR`].
pub(crate) fn run(
    path: Option<&Path>,
    cached: Option<bool>,
    verbose: bool,
) -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return failure(&format!("cannot read the current directory: {error}"));
        },
    };
    let target = path.map_or_else(|| cwd.clone(), |path| cwd.join(path));
    // A path that no longer exists may still have records, under its lexical form.
    let target = target.canonicalize().unwrap_or(target);
    let cache = match cache_dir_from_env() {
        Ok(cache) => cache,
        Err(error) => return failure(&error.to_string()),
    };
    let lock = match CacheLock::acquire(&cache, &LockDetails::current(), |holder| {
        ui::print_waiting(&holder.to_string());
    }) {
        Ok(lock) => lock,
        Err(error) => return failure(&error.to_string()),
    };
    let store = match Store::open(&cache) {
        Ok(store) => store,
        Err(error) => return failure(&error.to_string()),
    };
    let mut report = match why(&store, &target) {
        Ok(report) => report,
        Err(error) => return failure(&format!("cannot read the cache: {error}")),
    };
    if let Some(cached) = cached {
        report.tasks.retain(|task| {
            let fully_cached = task
                .steps
                .iter()
                .all(|step| matches!(step.verdict, Verdict::Cached));
            !task.steps.is_empty() && fully_cached == cached
        });
    }
    let render = Render {
        base: cwd,
        home: env::var_os(kizz_store::location::HOME)
            .map(PathBuf::from)
            .filter(|home| home.is_absolute()),
        verbose,
    };
    ui::print_text(&render.why(&report));
    let warnings: Vec<String> = report
        .warnings()
        .iter()
        .map(|warning| render.repeated(warning))
        .collect();
    for warning in &warnings {
        ui::print_warning(warning);
    }
    ui::print_warning_summary(&warnings);
    if let Err(error) = store.close() {
        return failure(&error.to_string());
    }
    if let Err(error) = lock.release() {
        return failure(&error.to_string());
    }
    ExitCode::SUCCESS
}

/// Print `message` as an error and return the usage-error status.
fn failure(message: &str) -> ExitCode {
    ui::print_error(message);
    ExitCode::from(USAGE_ERROR)
}
