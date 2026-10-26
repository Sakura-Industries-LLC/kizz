// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Cleaning up the cache: the entries `kizz cache clean` removes because the files
//! they are about are gone, and the entries `kizz cache reset` wipes for the tasks
//! under a path.
//!
//! Both change cache entries only, never a project file, and both run under the
//! cache lock, which the caller holds, so no build changes the cache meanwhile.
//!
//! Every step record carries its owners, the tasks that produced or used it (see
//! [`StepRecord::owners`]): the engine adds a task when the task runs the step or
//! finds it cached. A task is under a path when its `kizz.md` is in the path or under
//! it, compared by path components.
//!
//! * [`clean`] checks every recorded path concurrently, on the command's worker pool, the
//!   `kizz.md` of every owner of a step record included. A step record that depends on a
//!   path that existed when kizz recorded it and no longer exists is removed as a whole;
//!   the step's working directory counts as such a dependency. From each step record it
//!   keeps, it takes out the owners whose `kizz.md` no longer exists, and it removes no
//!   record for that alone, so a record may be left with no owner. Task records, step
//!   positions, and learned order for `kizz.md` files that no longer exist are removed,
//!   as are last-writer entries for paths that no longer exist. A dependency on a path
//!   that did not exist when it was recorded is still valid and is kept, and so are
//!   untracked `/proc` and `/sys` accesses, which are never dependencies.
//! * [`plan_reset`] selects every entry for the tasks under a directory: their task
//!   records, step positions, and learned order, the last-writer entries for paths under
//!   it, and the step records they own. A step record is removed when it has an owner and
//!   every owner is a task under the directory; a step record that also has owners
//!   outside it is kept, and the reset only takes the tasks under the directory out of
//!   its owners; and a step record with no owner is no task's entry, so no path reset
//!   removes or changes it. Neither a step position nor a record's working directory
//!   decides which step records a reset removes. [`ResetPlan::apply`] then removes the
//!   entries and changes the owners in one transaction.

use std::{
    collections::{BTreeSet, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use kizz_store::{
    EntryCounts, LastWriter, LearnedOrder, PathKey, PathState, PositionKey, StepKey, StepPosition,
    StepRecord, Store, StoreError, TaskId, TaskRecord, Txn, path_prefix,
};
use rayon::iter::{IntoParallelIterator as _, ParallelIterator as _};

use crate::pool::WorkerPool;

/// What [`clean`] found and removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleaned {
    /// How many distinct recorded paths it checked.
    pub checked: usize,
    /// How many of them no longer exist.
    pub gone: usize,
    /// The entries it removed, and the step records it kept that lost owners.
    pub removed: EntryCounts,
}

/// Removes the cache entries about files and dependencies that existed when kizz
/// recorded them but no longer exist, and takes the tasks whose `kizz.md` no longer
/// exists out of the owners of the step records it keeps, after checking every
/// recorded path concurrently on `pool`, in one transaction.
///
/// # Errors
///
/// A [`StoreError`] when the cache cannot be read or the removal cannot be
/// committed; then nothing is removed.
pub fn clean(
    store: &Store,
    pool: &WorkerPool,
) -> Result<Cleaned, StoreError> {
    let mut txn = store.write()?;
    let steps = txn.scan::<StepRecord>(&[])?;
    let positions: Vec<PositionKey> = keys(txn.scan::<StepPosition>(&[])?);
    let tasks: Vec<TaskId> = keys(txn.scan::<TaskRecord>(&[])?);
    let orders: Vec<TaskId> = keys(txn.scan::<LearnedOrder>(&[])?);
    let writers: Vec<PathKey> = keys(txn.scan::<LastWriter>(&[])?);

    let mut paths: BTreeSet<&Path> = BTreeSet::new();
    for (_, record) in &steps {
        paths.extend(dependencies(record));
        paths.extend(record.owners.iter().map(|owner| owner.file.as_path()));
    }
    paths.extend(positions.iter().map(|key| key.task.file.as_path()));
    paths.extend(tasks.iter().chain(&orders).map(|id| id.file.as_path()));
    paths.extend(writers.iter().map(|key| key.0.as_path()));
    let checked = paths.len();
    let gone: HashSet<PathBuf> = pool.install(|| {
        paths
            .into_par_iter()
            .filter(|path| is_gone(path))
            .map(Path::to_path_buf)
            .collect()
    });

    let mut removed = Removal::default();
    for (key, record) in &steps {
        if dependencies(record).any(|path| gone.contains(path)) {
            removed.steps.push(*key);
            continue;
        }
        let vanished: BTreeSet<TaskId> = record
            .owners
            .iter()
            .filter(|owner| gone.contains(owner.file.as_path()))
            .cloned()
            .collect();
        if !vanished.is_empty() {
            removed.disowned.push((*key, vanished));
        }
    }
    removed.positions = positions
        .into_iter()
        .filter(|key| gone.contains(key.task.file.as_path()))
        .collect();
    removed.tasks = tasks
        .into_iter()
        .filter(|id| gone.contains(id.file.as_path()))
        .collect();
    removed.orders = orders
        .into_iter()
        .filter(|id| gone.contains(id.file.as_path()))
        .collect();
    removed.writers = writers
        .into_iter()
        .filter(|key| gone.contains(key.0.as_path()))
        .collect();
    let gone = gone.len();
    removed.delete(&mut txn)?;
    txn.commit()?;
    Ok(Cleaned {
        checked,
        gone,
        removed: removed.counts(),
    })
}

/// Every entry for the tasks under one directory, which [`ResetPlan::apply`]
/// removes, and the shared step records it takes those tasks out of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetPlan {
    /// The directory.
    dir: PathBuf,
    /// The entries.
    removal: Removal,
}

impl ResetPlan {
    /// The directory whose tasks the plan wipes.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many entries the plan removes, and for how many tasks: the step records
    /// it removes, and apart from them the shared step records that only lose
    /// owners; the tasks count every task under the directory with a task record, a
    /// step position, learned order, or a step record it owns. A plan whose only
    /// change is to owner sets is not empty.
    #[must_use]
    pub fn counts(&self) -> EntryCounts {
        self.removal.counts()
    }

    /// Removes the entries and takes the tasks under the directory out of the
    /// owners of the shared step records, all in one transaction.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] when the removal cannot be committed; then nothing is
    /// removed or changed.
    pub fn apply(
        &self,
        store: &Store,
    ) -> Result<(), StoreError> {
        let mut txn = store.write()?;
        self.removal.delete(&mut txn)?;
        txn.commit()
    }
}

/// Selects every entry for the tasks under `dir`, an absolute path, without
/// changing anything.
///
/// A task is under `dir` when its `kizz.md` is in `dir` or under it, compared by
/// path components. The step records of those tasks are the step records they own:
/// a record whose owners are all under `dir` is removed, a record that also has
/// owners outside `dir` only loses the owners under it, and a record with no owner
/// is left alone. Neither a step position nor a record's working directory decides
/// which step records are removed.
///
/// # Errors
///
/// A [`StoreError`] when the cache cannot be read.
pub fn plan_reset(
    store: &Store,
    dir: &Path,
) -> Result<ResetPlan, StoreError> {
    let under = |id: &TaskId| id.file.starts_with(dir);
    let txn = store.read()?;
    let mut removal = Removal {
        positions: keys(txn.scan::<StepPosition>(&[])?)
            .into_iter()
            .filter(|key| under(&key.task))
            .collect(),
        tasks: keys(txn.scan::<TaskRecord>(&[])?)
            .into_iter()
            .filter(under)
            .collect(),
        orders: keys(txn.scan::<LearnedOrder>(&[])?)
            .into_iter()
            .filter(under)
            .collect(),
        writers: keys(txn.scan::<LastWriter>(&path_prefix(dir))?),
        ..Removal::default()
    };
    for (key, record) in txn.scan::<StepRecord>(&[])? {
        let (inside, outside): (BTreeSet<TaskId>, BTreeSet<TaskId>) =
            record.owners.into_iter().partition(under);
        if inside.is_empty() {
            continue;
        }
        if outside.is_empty() {
            removal.steps.push(key);
            removal.owners.extend(inside);
        } else {
            removal.disowned.push((key, inside));
        }
    }
    Ok(ResetPlan {
        dir: dir.to_path_buf(),
        removal,
    })
}

/// The keys of the entries of a scan.
fn keys<K, R>(entries: Vec<(K, R)>) -> Vec<K> {
    entries.into_iter().map(|(key, _)| key).collect()
}

/// The paths a step record depends on that existed when kizz recorded them: its
/// working directory and every observed path whose recorded state is not absent.
/// Untracked `/proc` and `/sys` accesses are not dependencies.
fn dependencies(record: &StepRecord) -> impl Iterator<Item = &Path> {
    std::iter::once(record.spec.cwd.as_path()).chain(
        record
            .observations
            .iter()
            .filter(|observation| {
                !matches!(observation.state, PathState::Absent | PathState::Untracked)
            })
            .map(|observation| observation.path.as_path()),
    )
}

/// Whether nothing exists at `path` any longer; a symbolic link counts as existing
/// even when its target is gone. A path that cannot be checked for another reason,
/// such as a missing permission, may still exist, so it is not gone.
fn is_gone(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) => {
            matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            )
        },
    }
}

/// The keys of the entries to remove, by family, and the owners to take out of the
/// step records that are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Removal {
    /// Step records.
    steps: Vec<StepKey>,
    /// The tasks that owned the removed step records and are taken out with them,
    /// for a path reset; [`clean`] removes step records for their dependencies, not
    /// for their owners, so it leaves this empty.
    owners: BTreeSet<TaskId>,
    /// Step records that are kept, each with the owners to take out of it.
    disowned: Vec<(StepKey, BTreeSet<TaskId>)>,
    /// Step positions.
    positions: Vec<PositionKey>,
    /// Task records.
    tasks: Vec<TaskId>,
    /// Learned-order entries.
    orders: Vec<TaskId>,
    /// Last-writer entries.
    writers: Vec<PathKey>,
}

impl Removal {
    /// How many entries there are of each family, how many kept step records lose
    /// owners, and the distinct tasks of the task records, step positions, learned
    /// order, and owners taken out.
    fn counts(&self) -> EntryCounts {
        let tasks: BTreeSet<&TaskId> = self
            .tasks
            .iter()
            .chain(&self.orders)
            .chain(self.positions.iter().map(|key| &key.task))
            .chain(&self.owners)
            .chain(self.disowned.iter().flat_map(|(_, owners)| owners))
            .collect();
        EntryCounts {
            tasks: tasks.len(),
            steps: self.steps.len(),
            shared_steps: self.disowned.len(),
            positions: self.positions.len(),
            task_records: self.tasks.len(),
            orders: self.orders.len(),
            writers: self.writers.len(),
        }
    }

    /// Deletes every entry and takes the owners out of the kept step records, in
    /// `txn`.
    fn delete(
        &self,
        txn: &mut Txn<'_>,
    ) -> Result<(), StoreError> {
        for key in &self.steps {
            txn.delete::<StepRecord>(key)?;
        }
        for (key, owners) in &self.disowned {
            if let Some(mut record) = txn.get::<StepRecord>(key)? {
                record.owners.retain(|owner| !owners.contains(owner));
                txn.put(key, &record)?;
            }
        }
        for key in &self.positions {
            txn.delete::<StepPosition>(key)?;
        }
        for id in &self.tasks {
            txn.delete::<TaskRecord>(id)?;
        }
        for id in &self.orders {
            txn.delete::<LearnedOrder>(id)?;
        }
        for key in &self.writers {
            txn.delete::<LastWriter>(key)?;
        }
        Ok(())
    }
}

/// Unit tests for the check for gone paths.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::path::Path;

    use super::is_gone;

    /// An existing path is not gone; a missing one, and one under a regular file,
    /// are.
    #[test]
    fn gone_paths_are_missing_or_under_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(!is_gone(dir.path()));
        assert!(!is_gone(&file));
        assert!(is_gone(&dir.path().join("missing")));
        assert!(is_gone(&file.join("below")));
        assert!(!is_gone(Path::new("/")));
    }
}
