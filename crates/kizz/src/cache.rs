// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz cache` commands: `kizz cache clean`, which removes the entries about
//! files that no longer exist without asking, and `kizz cache reset`, which wipes the
//! entries for the tasks under a path, or the whole cache with `--all`, after showing
//! what it will remove and asking for confirmation.
//!
//! Every cache command takes the cache lock first, so none runs during a build, and
//! a waiting command says what it is waiting for, as a waiting build does. The
//! commands change cache entries only, never project files.
//!
//! A reset asks on standard error and reads the answer from standard input, so it
//! needs both to be a terminal; without one it refuses to run unless `--yes` is
//! given, which also skips the question.

use std::{
    env,
    io::{self, BufRead as _, IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

use kizz_core::{WorkerPool, clean as clean_cache, plan_reset};
use kizz_store::{
    CacheIdentity, CacheLock, EntryCounts, LockDetails, SCHEMA_VERSION, Store, StoreError,
    cache_dir_from_env, identity_differences_text,
};

use crate::{cli::USAGE_ERROR, ui};

/// The error of a reset that has no terminal to ask on and no `--yes`.
const NO_TERMINAL: &str = "refusing to reset the cache without a terminal to ask for \
                           confirmation; pass --yes to reset without asking";

/// The error of `kizz cache reset --all` with a path.
const ALL_WITH_PATH: &str = "`kizz cache reset --all` wipes the whole cache and takes no path";

/// What a path reset does to the step records that tasks outside the path also own.
const RESET_KEEPS: &str = "shared with tasks outside it, kept without the tasks under it";

/// What `kizz cache clean` does to the step records it keeps that lose owners.
const CLEAN_KEEPS: &str = "kept without the tasks whose kizz.md no longer exists";

/// Remove the cache entries about files and dependencies that no longer exist,
/// and take the tasks whose `kizz.md` no longer exists out of the owners of the
/// step records it keeps, without asking; return the exit status.
///
/// A cache that cannot be located, locked, opened, or cleaned exits with
/// [`USAGE_ERROR`], and so does a clean whose worker pool, which checks the recorded
/// paths, cannot start.
pub(crate) fn clean() -> ExitCode {
    finish(locked(|cache| {
        let pool = WorkerPool::new().map_err(|error| error.to_string())?;
        let store = Store::open(cache).map_err(|error| error.to_string())?;
        let cleaned = clean_cache(&store, &pool).map_err(|error| cannot("clean", &error))?;
        store.close().map_err(|error| error.to_string())?;
        println!(
            "checked {}; {} no longer {}",
            ui::count(cleaned.checked, "recorded path"),
            cleaned.gone,
            if cleaned.gone == 1 { "exists" } else { "exist" }
        );
        if cleaned.removed.is_empty() {
            println!("nothing to remove: no cache entry is about a file that no longer exists");
        } else {
            println!(
                "removed {}{}",
                entries(&cleaned.removed),
                kept(&cleaned.removed, CLEAN_KEEPS)
            );
        }
        Ok(())
    }))
}

/// Wipe the cache entries for the tasks under `path`, relative to the current
/// directory, or under the current directory without it, or the whole cache when
/// `all` is set, after showing what will be removed and asking, unless `yes` is set;
/// return the exit status.
///
/// A path given with `all`, a missing terminal without `yes`, and a cache that
/// cannot be located, locked, opened, or changed exit with [`USAGE_ERROR`]. A
/// declined confirmation changes nothing and exits with status 0.
pub(crate) fn reset(
    path: Option<&Path>,
    all: bool,
    yes: bool,
) -> ExitCode {
    if all && path.is_some() {
        return failure(ALL_WITH_PATH);
    }
    let target = match env::current_dir() {
        Ok(cwd) => path.map_or_else(|| cwd.clone(), |path| cwd.join(path)),
        Err(error) => return failure(&format!("cannot read the current directory: {error}")),
    };
    // A path that no longer exists may still have entries, under its lexical form.
    let target = target.canonicalize().unwrap_or(target);
    finish(locked(|cache| {
        if all {
            reset_all(cache, yes)
        } else {
            reset_under(cache, &target, yes)
        }
    }))
}

/// Wipe the whole cache in `cache`, whatever its schema version and identity, after
/// asking unless `yes` is set; a cache of another version or identity, or of this
/// version without an identity, says so before the question.
fn reset_all(
    cache: &Path,
    yes: bool,
) -> Result<(), String> {
    let inventory = Store::inventory(cache).map_err(|error| cannot("read", &error))?;
    let other_version = inventory
        .schema_version
        .filter(|version| *version != SCHEMA_VERSION);
    let differences = inventory
        .identity
        .as_ref()
        .map(|stored| stored.differences(&CacheIdentity::current()))
        .unwrap_or_default();
    let missing_identity =
        inventory.schema_version == Some(SCHEMA_VERSION) && inventory.identity.is_none();
    if inventory.entries.is_empty()
        && other_version.is_none()
        && differences.is_empty()
        && !missing_identity
    {
        println!(
            "the cache at `{}` is empty; nothing to remove",
            cache.display()
        );
        return Ok(());
    }
    println!(
        "kizz cache reset --all wipes the whole cache at `{}`: {}",
        cache.display(),
        tasks_and_entries(&inventory.entries)
    );
    if let Some(version) = other_version {
        println!(
            "the cache has schema version {version}, and this kizz uses schema version \
             {SCHEMA_VERSION}"
        );
    }
    if !differences.is_empty() {
        println!(
            "the cache belongs to another machine or user: {}",
            identity_differences_text(&differences)
        );
    }
    if missing_identity {
        println!(
            "the cache has no cache identity, so kizz cannot tell which machine and user it \
             belongs to"
        );
    }
    if !confirm(yes)? {
        println!("reset declined; nothing was removed");
        return Ok(());
    }
    Store::wipe(cache).map_err(|error| cannot("wipe", &error))?;
    println!(
        "wiped the whole cache: removed {}",
        entries(&inventory.entries)
    );
    Ok(())
}

/// Wipe the entries in `cache` for the tasks under `dir`, and take those tasks out
/// of the owners of the step records that tasks outside it also own, after asking
/// unless `yes` is set. A reset whose only change is to owners describes it and
/// asks like any other.
fn reset_under(
    cache: &Path,
    dir: &Path,
    yes: bool,
) -> Result<(), String> {
    let store = Store::open(cache).map_err(|error| error.to_string())?;
    let plan = plan_reset(&store, dir).map_err(|error| cannot("read", &error))?;
    let counts = plan.counts();
    if counts.is_empty() {
        println!(
            "the cache has no entries for tasks under `{}`; nothing to remove",
            plan.dir().display()
        );
        return store.close().map_err(|error| error.to_string());
    }
    println!(
        "kizz cache reset wipes every entry for the tasks under `{}`: {}{}",
        plan.dir().display(),
        tasks_and_entries(&counts),
        kept(&counts, RESET_KEEPS)
    );
    if !confirm(yes)? {
        println!("reset declined; nothing was removed");
        return store.close().map_err(|error| error.to_string());
    }
    plan.apply(&store)
        .map_err(|error| cannot("change", &error))?;
    store.close().map_err(|error| error.to_string())?;
    println!("removed {}{}", entries(&counts), kept(&counts, RESET_KEEPS));
    Ok(())
}

/// Whether the reset may go ahead: at once with `yes`; otherwise only when standard
/// input and standard error are a terminal and the answer to the question is yes.
///
/// # Errors
///
/// [`NO_TERMINAL`] without a terminal, and a message when the answer cannot be
/// read.
fn confirm(yes: bool) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(NO_TERMINAL.to_owned());
    }
    ui::print_question("Remove these entries? [y/N]");
    io::stderr()
        .flush()
        .map_err(|error| format!("cannot ask for confirmation: {error}"))?;
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| format!("cannot read the confirmation: {error}"))?;
    Ok(is_yes(&answer))
}

/// Whether `answer` is a yes: `y` or `yes` in any case, around white space.
fn is_yes(answer: &str) -> bool {
    let answer = answer.trim();
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

/// The number of tasks and then the entries of `counts`.
fn tasks_and_entries(counts: &EntryCounts) -> String {
    format!("{}; {}", ui::count(counts.tasks, "task"), entries(counts))
}

/// The entries of `counts`, family by family.
fn entries(counts: &EntryCounts) -> String {
    [
        ui::count(counts.steps, "step record"),
        ui::count(counts.positions, "step position"),
        ui::count(counts.task_records, "task record"),
        ui::count_as(
            counts.orders,
            "learned-order entry",
            "learned-order entries",
        ),
        ui::count_as(counts.writers, "last-writer entry", "last-writer entries"),
    ]
    .join(", ")
}

/// The step records of `counts` that are kept but lose owners, as `what` says what
/// happens to them, after a semicolon; nothing when there are none.
fn kept(
    counts: &EntryCounts,
    what: &str,
) -> String {
    if counts.shared_steps == 0 {
        String::new()
    } else {
        format!("; {} {what}", ui::count(counts.shared_steps, "step record"))
    }
}

/// Run `work` on the cache directory under the cache lock, saying what it waits for
/// while another process holds the lock, and release the lock afterwards.
fn locked(work: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    let cache: PathBuf = cache_dir_from_env().map_err(|error| error.to_string())?;
    let lock = CacheLock::acquire(&cache, &LockDetails::current(), |holder| {
        ui::print_waiting(&holder.to_string());
    })
    .map_err(|error| error.to_string())?;
    let result = work(&cache);
    let released = lock.release().map_err(|error| error.to_string());
    result.and(released)
}

/// The message for a cache that cannot be changed or read as `action` says.
fn cannot(
    action: &str,
    error: &StoreError,
) -> String {
    format!("cannot {action} the cache: {error}")
}

/// The exit status of `result`, printing its error.
fn finish(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => failure(&message),
    }
}

/// Print `message` as an error and return the usage-error status.
fn failure(message: &str) -> ExitCode {
    ui::print_error(message);
    ExitCode::from(USAGE_ERROR)
}

/// Unit tests for the answers and the descriptions of entries.
#[cfg(test)]
mod tests {
    use kizz_store::EntryCounts;

    use super::{CLEAN_KEEPS, RESET_KEEPS, entries, is_yes, kept, tasks_and_entries};

    /// `y` and `yes` in any case confirm; anything else, an empty answer included,
    /// declines.
    #[test]
    fn only_yes_confirms() {
        for answer in ["y\n", "Y", " yes \n", "YES", "Yes\r\n"] {
            assert!(is_yes(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n", "no", "yes please", "y y", "ok"] {
            assert!(!is_yes(answer), "{answer:?}");
        }
    }

    /// The description counts the tasks and every family, singular and plural, and
    /// the step records that are kept but lose owners only when there are any.
    #[test]
    fn descriptions_count_every_family() {
        let counts = EntryCounts {
            tasks: 2,
            steps: 3,
            shared_steps: 0,
            positions: 1,
            task_records: 2,
            orders: 1,
            writers: 0,
        };
        assert_eq!(
            tasks_and_entries(&counts),
            "2 tasks; 3 step records, 1 step position, 2 task records, 1 learned-order entry, \
             0 last-writer entries"
        );
        assert_eq!(kept(&counts, RESET_KEEPS), "");
        assert_eq!(
            entries(&EntryCounts::default()),
            "0 step records, 0 step positions, 0 task records, 0 learned-order entries, 0 \
             last-writer entries"
        );
        let shared = EntryCounts {
            tasks: 1,
            shared_steps: 1,
            ..EntryCounts::default()
        };
        assert!(!shared.is_empty());
        assert_eq!(
            format!(
                "{}{}",
                tasks_and_entries(&shared),
                kept(&shared, RESET_KEEPS)
            ),
            "1 task; 0 step records, 0 step positions, 0 task records, 0 learned-order \
             entries, 0 last-writer entries; 1 step record shared with tasks outside it, kept \
             without the tasks under it"
        );
        assert_eq!(
            kept(
                &EntryCounts {
                    shared_steps: 2,
                    ..EntryCounts::default()
                },
                CLEAN_KEEPS
            ),
            "; 2 step records kept without the tasks whose kizz.md no longer exists"
        );
    }
}
