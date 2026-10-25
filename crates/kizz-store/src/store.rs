// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache database: `SurrealKV`, embedded, in the `db` directory of the cache.
//!
//! A [`Store`] is shared by the threads of one process. Every read and write goes
//! through a [`Txn`], which sees a consistent snapshot; a write transaction changes
//! nothing until [`Txn::commit`], and then changes every record it wrote or none.
//!
//! `SurrealKV` commits and closes asynchronously on a Tokio runtime, so the store owns a
//! small runtime and waits for each commit and for the close. After each commit the
//! store hands the write-ahead log to the operating system, so a committed record
//! survives a crash of the process.
//!
//! When the store creates the database it writes [`SCHEMA_VERSION`] into it, and it
//! refuses to open a database with another version. It never wipes a cache by
//! itself: `kizz cache reset --all` does, after asking, through [`Store::inventory`],
//! which counts the entries of a cache of any schema version, and [`Store::wipe`].

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use surrealkv::{Durability, LSMIterator as _, Mode, ReadOptions, Transaction, Tree, TreeBuilder};

use crate::records::{
    Family, PositionKey, Record, RecordKey, SchemaKey, SchemaVersion, TaskId, database_key,
};

/// The name of the database directory in the cache directory.
pub const DATABASE_DIR: &str = "db";

/// The name a wiped database directory has while [`Store::wipe`] removes it.
const WIPED_DIR: &str = "db.wiped";

/// The version of the record encoding that this build of kizz reads and writes.
///
/// Version 2 replaced the first step record with the full one: the step
/// specification, the observations with their path states, and why the step ran.
/// Version 3 added the cache check's reasons, with the differences it found, when
/// each record's observations were written, and the failure message of a missing
/// or non-executable program. Version 4 added the reason a run's result was not
/// cached, which marks a record that is never a cache hit, and the exit of a
/// command that could not start. Version 5 added explanations: the verdict of the
/// most recent build in step positions, the parts of the task program and how
/// output names the task in task records, and the position, build, and time of
/// each last writer. Version 6 added the reason for the result of a command that
/// exited with a code of its own after the build was interrupted, which, like a
/// command that a signal stopped then, is never a cache hit. Version 7 added the
/// owners of step records: the tasks that produced or used each record, which path
/// resets and `kizz cache clean` use. Version 8 added task programs as last
/// writers: each last writer names its task and either the step that wrote the path,
/// with its position, or the task program, and explanations name a task program
/// that last wrote a changed input. Version 9 added the owner, as user and group
/// numbers, to every regular-file state, the mode and owner to every directory
/// state, and the permission and owner changes that a cache check finds.
pub const SCHEMA_VERSION: u32 = 9;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database directory could not be created.
    #[error("cannot create the cache database directory `{}`", path.display())]
    CreateDir {
        /// The database directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// The runtime that drives the database could not start.
    #[error("cannot start the cache database runtime")]
    Runtime(#[source] io::Error),
    /// The database reported an error.
    #[error("the cache database failed")]
    Database(#[from] surrealkv::Error),
    /// Another transaction committed a change to a record this one wrote, so this
    /// one changed nothing.
    #[error("a concurrent transaction changed the same cache records; nothing was written")]
    Conflict,
    /// A record could not be encoded.
    #[error("cannot encode a {family}: {message}")]
    Encode {
        /// The family of the record.
        family: Family,
        /// What went wrong.
        message: String,
    },
    /// A stored record could not be decoded.
    #[error("cannot decode a stored {family}")]
    Decode {
        /// The family of the record.
        family: Family,
        /// The cause.
        #[source]
        source: minicbor::decode::Error,
    },
    /// A stored key is not a key of its family.
    #[error("the cache database holds a malformed {family} key")]
    MalformedKey {
        /// The family of the key.
        family: Family,
    },
    /// The database was written with another schema version.
    #[error(
        "the cache database at `{}` has schema version {found}, but this kizz uses schema \
         version {expected}; run `kizz cache reset --all` to wipe the cache",
        path.display()
    )]
    SchemaMismatch {
        /// The database directory.
        path: PathBuf,
        /// The version stored in the database.
        found: u32,
        /// The version this build uses, [`SCHEMA_VERSION`].
        expected: u32,
    },
    /// The database directory could not be removed.
    #[error("cannot remove the cache database `{}`", path.display())]
    Wipe {
        /// The directory that could not be removed.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
}

/// How many entries of each family a set of cache entries holds, and how many tasks
/// they belong to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntryCounts {
    /// The distinct tasks of the task records, step positions, and learned order,
    /// and the tasks taken out of the owners of step records.
    pub tasks: usize,
    /// Step records.
    pub steps: usize,
    /// Step records that are kept but lose owners, apart from the step records
    /// counted in [`EntryCounts::steps`].
    pub shared_steps: usize,
    /// Step positions.
    pub positions: usize,
    /// Task records.
    pub task_records: usize,
    /// Learned-order entries.
    pub orders: usize,
    /// Last-writer entries.
    pub writers: usize,
}

impl EntryCounts {
    /// Whether there is no entry at all and no step record loses an owner.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.steps == 0
            && self.shared_steps == 0
            && self.positions == 0
            && self.task_records == 0
            && self.orders == 0
            && self.writers == 0
    }
}

/// What a whole cache holds, as [`Store::inventory`] counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Inventory {
    /// The entries of every family but the schema version.
    pub entries: EntryCounts,
    /// The schema version stored in the database, or `None` when there is no
    /// database or it holds no readable version.
    pub schema_version: Option<u32>,
}

/// The cache database, shared by the threads of one process.
pub struct Store {
    /// The `SurrealKV` tree. Declared before the runtime so that it is dropped first.
    tree: Tree,
    /// The runtime that drives the tree's commits and background work.
    runtime: tokio::runtime::Runtime,
    /// The database directory.
    path: PathBuf,
    /// Whether the tree has been closed.
    closed: bool,
}

impl std::fmt::Debug for Store {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens the database in the `db` directory of `cache_dir`, creating it, with the
    /// current schema version, if it does not exist.
    ///
    /// # Errors
    ///
    /// [`StoreError::SchemaMismatch`] when the database has another schema version,
    /// and another [`StoreError`] when the database cannot be created or opened.
    pub fn open(cache_dir: &Path) -> Result<Self, StoreError> {
        let store = Self::open_tree(cache_dir)?;
        store.check_schema()?;
        Ok(store)
    }

    /// Counts the entries of the database in the `db` directory of `cache_dir`,
    /// whatever its schema version, by their keys alone, without decoding a record
    /// or changing anything. A cache without a database has no entries.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] when the database cannot be opened or read.
    pub fn inventory(cache_dir: &Path) -> Result<Inventory, StoreError> {
        if !cache_dir.join(DATABASE_DIR).is_dir() {
            return Ok(Inventory::default());
        }
        let store = Self::open_tree(cache_dir)?;
        let inventory = {
            let txn = store.read()?;
            let schema_version = txn
                .inner
                .get(database_key::<SchemaVersion>(&SchemaKey))?
                .and_then(|bytes| decode::<SchemaVersion>(&bytes).ok())
                .map(|schema| schema.version);
            let task_keys = txn.raw_keys(Family::Task)?;
            let position_keys = txn.raw_keys(Family::Position)?;
            let order_keys = txn.raw_keys(Family::Order)?;
            let tasks: BTreeSet<TaskId> = task_keys
                .iter()
                .chain(&order_keys)
                .filter_map(|key| TaskId::decode_key(key))
                .chain(
                    position_keys
                        .iter()
                        .filter_map(|key| PositionKey::decode_key(key))
                        .map(|key| key.task),
                )
                .collect();
            Inventory {
                entries: EntryCounts {
                    tasks: tasks.len(),
                    steps: txn.raw_keys(Family::Step)?.len(),
                    shared_steps: 0,
                    positions: position_keys.len(),
                    task_records: task_keys.len(),
                    orders: order_keys.len(),
                    writers: txn.raw_keys(Family::Writer)?.len(),
                },
                schema_version,
            }
        };
        store.close()?;
        Ok(inventory)
    }

    /// Wipes the whole cache database in the `db` directory of `cache_dir`, whatever
    /// its schema version; the next [`Store::open`] creates an empty one. The caller
    /// holds the cache lock, so no process has the database open.
    ///
    /// The directory is first renamed, in one step, so a crash during the removal
    /// leaves no partial database behind, only a directory that the next wipe
    /// removes.
    ///
    /// # Errors
    ///
    /// [`StoreError::Wipe`] when the directory cannot be renamed or removed.
    pub fn wipe(cache_dir: &Path) -> Result<(), StoreError> {
        let path = cache_dir.join(DATABASE_DIR);
        let wiped = cache_dir.join(WIPED_DIR);
        let remove = |dir: &Path| {
            match fs::remove_dir_all(dir) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    Err(StoreError::Wipe {
                        path: dir.to_path_buf(),
                        source: error,
                    })
                },
                _ => Ok(()),
            }
        };
        remove(&wiped)?;
        match fs::rename(&path, &wiped) {
            Ok(()) => remove(&wiped),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(StoreError::Wipe {
                    path,
                    source: error,
                })
            },
        }
    }

    /// Opens the database in the `db` directory of `cache_dir`, creating it if it does
    /// not exist, without checking or writing the schema version.
    fn open_tree(cache_dir: &Path) -> Result<Self, StoreError> {
        let path = cache_dir.join(DATABASE_DIR);
        fs::create_dir_all(&path).map_err(|source| {
            StoreError::CreateDir {
                path: path.clone(),
                source,
            }
        })?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kizz-store")
            .enable_all()
            .build()
            .map_err(StoreError::Runtime)?;
        let tree = {
            // The tree starts its background tasks on the runtime it is opened in.
            let _entered = runtime.enter();
            TreeBuilder::new().with_path(path.clone()).build()?
        };
        Ok(Self {
            tree,
            runtime,
            path,
            closed: false,
        })
    }

    /// The database directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Begins a read-only transaction.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] when the database cannot begin one.
    pub fn read(&self) -> Result<Txn<'_>, StoreError> {
        Ok(Txn {
            store: self,
            inner: self.tree.begin_with_mode(Mode::ReadOnly)?,
        })
    }

    /// Begins a read-write transaction, which changes nothing until it is committed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] when the database cannot begin one.
    pub fn write(&self) -> Result<Txn<'_>, StoreError> {
        Ok(Txn {
            store: self,
            inner: self.tree.begin_with_mode(Mode::ReadWrite)?,
        })
    }

    /// Closes the database, after every commit that has started.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] when closing fails.
    pub fn close(mut self) -> Result<(), StoreError> {
        self.close_inner()
    }

    /// Closes the tree once.
    fn close_inner(&mut self) -> Result<(), StoreError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.runtime.block_on(self.tree.close())?;
        Ok(())
    }

    /// Writes the schema version into a new database, or checks the version of an
    /// existing one.
    fn check_schema(&self) -> Result<(), StoreError> {
        let mut txn = self.write()?;
        match txn.get::<SchemaVersion>(&SchemaKey)? {
            Some(SchemaVersion {
                version: SCHEMA_VERSION,
            }) => Ok(()),
            Some(SchemaVersion { version }) => {
                Err(StoreError::SchemaMismatch {
                    path: self.path.clone(),
                    found: version,
                    expected: SCHEMA_VERSION,
                })
            },
            None => {
                txn.put(&SchemaKey, &SchemaVersion {
                    version: SCHEMA_VERSION,
                })?;
                txn.commit()
            },
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // Nothing can report a failure here; the next open recovers from the log.
        let _ignored: Result<(), StoreError> = self.close_inner();
    }
}

/// A transaction on the [`Store`]: a consistent snapshot of the records, and for a
/// write transaction, the changes to commit. Dropping it without committing discards
/// the changes.
pub struct Txn<'s> {
    /// The store.
    store: &'s Store,
    /// The `SurrealKV` transaction.
    inner: Transaction,
}

impl std::fmt::Debug for Txn<'_> {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Txn")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl Txn<'_> {
    /// The record of type `R` at `key`, if there is one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] when the read fails, and [`StoreError::Decode`] when
    /// the stored record cannot be decoded.
    pub fn get<R: Record>(
        &self,
        key: &R::Key,
    ) -> Result<Option<R>, StoreError> {
        self.inner
            .get(database_key::<R>(key))?
            .map(|bytes| decode::<R>(&bytes))
            .transpose()
    }

    /// Writes `record` at `key`, replacing any record there, when the transaction
    /// commits.
    ///
    /// # Errors
    ///
    /// [`StoreError::Encode`] when the record cannot be encoded, and
    /// [`StoreError::Database`] when the transaction is read-only.
    pub fn put<R: Record>(
        &mut self,
        key: &R::Key,
        record: &R,
    ) -> Result<(), StoreError> {
        let bytes = minicbor::to_vec(record).map_err(|error| {
            StoreError::Encode {
                family: R::FAMILY,
                message: error.to_string(),
            }
        })?;
        self.inner.set(database_key::<R>(key), bytes)?;
        Ok(())
    }

    /// Deletes the record of type `R` at `key`, if there is one, when the transaction
    /// commits.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] when the transaction is read-only.
    pub fn delete<R: Record>(
        &mut self,
        key: &R::Key,
    ) -> Result<(), StoreError> {
        self.inner.delete(database_key::<R>(key))?;
        Ok(())
    }

    /// Every record of type `R` whose key, after the family prefix, starts with
    /// `prefix`, in key order. An empty prefix selects the whole family.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] when the scan fails, [`StoreError::MalformedKey`] and
    /// [`StoreError::Decode`] when a stored key or record cannot be decoded.
    pub fn scan<R: Record>(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<(R::Key, R)>, StoreError> {
        let family = R::FAMILY.prefix();
        let mut start = family.to_vec();
        start.extend_from_slice(prefix);
        let mut options = ReadOptions::new();
        options.set_iterate_upper_bound(prefix_end(&start));
        options.set_iterate_lower_bound(Some(start));
        let mut iter = self.inner.range_with_options(&options)?;
        let mut records = Vec::new();
        let mut valid = iter.seek_first()?;
        while valid {
            let key = iter
                .key()
                .user_key()
                .strip_prefix(family)
                .and_then(R::Key::decode_key)
                .ok_or(StoreError::MalformedKey { family: R::FAMILY })?;
            let record = decode::<R>(&iter.value()?)?;
            records.push((key, record));
            valid = iter.next()?;
        }
        Ok(records)
    }

    /// The key of every entry of `family`, after the family prefix, in key order,
    /// without decoding a key or a record.
    fn raw_keys(
        &self,
        family: Family,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        let start = family.prefix().to_vec();
        let mut options = ReadOptions::new();
        options.set_iterate_upper_bound(prefix_end(&start));
        options.set_iterate_lower_bound(Some(start));
        let mut iter = self.inner.range_with_options(&options)?;
        let mut keys = Vec::new();
        let mut valid = iter.seek_first()?;
        while valid {
            if let Some(key) = iter.key().user_key().strip_prefix(family.prefix()) {
                keys.push(key.to_vec());
            }
            valid = iter.next()?;
        }
        Ok(keys)
    }

    /// Commits every change of the transaction, or none of them.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when a concurrent transaction committed a change to a
    /// record this one wrote, and [`StoreError::Database`] when the commit fails; in
    /// both cases nothing was written.
    pub fn commit(mut self) -> Result<(), StoreError> {
        self.inner.set_durability(Durability::Eventual);
        match self.store.runtime.block_on(self.inner.commit()) {
            Ok(()) => {},
            Err(surrealkv::Error::TransactionWriteConflict) => return Err(StoreError::Conflict),
            Err(error) => return Err(error.into()),
        }
        // Hand the log to the operating system, so the commit survives a crash of
        // this process.
        self.store.tree.flush_wal(false)?;
        Ok(())
    }
}

/// Decodes a stored record of type `R`.
fn decode<R: Record>(bytes: &[u8]) -> Result<R, StoreError> {
    minicbor::decode(bytes).map_err(|source| {
        StoreError::Decode {
            family: R::FAMILY,
            source,
        }
    })
}

/// The first key after every key that starts with `prefix`, or `None` when no such
/// key exists because the prefix is all `0xFF` bytes.
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if let Some(next) = last.checked_add(1) {
            end.push(next);
            return Some(end);
        }
    }
    None
}

/// Unit tests for the parts of the store that need no database.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_end_is_the_first_key_after_the_prefix() {
        assert_eq!(prefix_end(b"task/"), Some(b"task0".to_vec()));
        assert_eq!(prefix_end(b"a\xFF\xFF"), Some(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xFF\xFF"), None);
        assert_eq!(prefix_end(b""), None);
    }

    #[test]
    fn schema_mismatch_names_both_versions_and_the_reset_command() {
        let message = StoreError::SchemaMismatch {
            path: PathBuf::from("/cache/db"),
            found: 5,
            expected: SCHEMA_VERSION,
        }
        .to_string();
        assert!(message.contains("schema version 5"), "{message}");
        assert!(
            message.contains(&format!("schema version {SCHEMA_VERSION}")),
            "{message}"
        );
        assert!(message.contains("`kizz cache reset --all`"), "{message}");
        assert!(message.contains("/cache/db"), "{message}");
    }

    /// Version 9 stores owners and directory modes, one more than version 8, whose
    /// file states had no owner and whose directory states only a listing.
    #[test]
    fn schema_version_9_has_owners_and_directory_modes() {
        assert_eq!(SCHEMA_VERSION, 9);
    }
}
