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

use std::{
    env,
    path::{Path, PathBuf},
    process::ExitCode,
};

use kizz_core::{Render, why};
use kizz_store::{CacheLock, LockDetails, Store, cache_dir_from_env};

use crate::{cli::USAGE_ERROR, ui};

/// Print the reasons stored by the most recent build for the tasks under `path`,
/// relative to the current directory, or under the current directory without it,
/// showing environment values when `verbose` is set; return the exit status.
///
/// A cache that cannot be located, locked, opened, or read exits with
/// [`USAGE_ERROR`].
pub(crate) fn run(
    path: Option<&Path>,
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
    let report = match why(&store, &target) {
        Ok(report) => report,
        Err(error) => return failure(&format!("cannot read the cache: {error}")),
    };
    let render = Render {
        base: cwd,
        home: env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute()),
        verbose,
    };
    print!("{}", render.why(&report));
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
