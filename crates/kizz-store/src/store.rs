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
//! The store starts no thread of its own ("Kizz never panics"), so it cannot fail
//! when the system cannot start one. Its runtime is a current-thread runtime, which
//! runs `SurrealKV`'s tasks on the threads that call the store, while they wait for a
//! commit or the close. Kizz builds `SurrealKV` from a patched copy in
//! `vendor/surrealkv`, and the store opens it with the copy's direct clock,
//! [`ClockMode::Direct`], which reads the system clock on every reading instead of
//! keeping a thread that brings it up to date. Its stall thresholds, [`STALL_THRESHOLD`],
//! are so high that `SurrealKV` never paces or stalls a write, which a cache of small
//! records does not need. When it replays several log segments after a crash,
//! the copy starts a thread when it can and otherwise replays them on the thread
//! that opens the store.
//!
//! The store waits for `SurrealKV` with `Runtime::block_on` only after it checks,
//! with `Handle::try_current`, that the calling thread is in no runtime, because
//! `block_on` panics when it is called within an asynchronous execution context
//! ("Kizz never panics"); a call from inside a runtime is
//! [`StoreError::InsideRuntime`] instead. Its runtime never panics when it is
//! dropped either: inside an asynchronous context, where dropping a Tokio runtime
//! panics, it shuts down in the background instead.
//!
//! When the store creates the database it writes [`SCHEMA_VERSION`] and the current
//! [`CacheIdentity`] into it. It refuses to open a database with another version,
//! and then one whose identity differs from the current one or that has none,
//! because the cache is not portable between machines and users. It never wipes a
//! cache by itself:
//! `kizz cache reset --all` does, after asking, through [`Store::inventory`], which
//! counts the entries of a cache of any schema version and reads its identity, and
//! [`Store::wipe`].

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use surrealkv::{
    ClockMode, Durability, LSMIterator as _, Mode, Options, ReadOptions, Transaction, Tree,
    TreeBuilder,
};

use crate::{
    identity::{CacheIdentity, IdentityDifference, IdentityKey, identity_differences_text},
    records::{
        Family, PositionKey, Record, RecordKey, SchemaKey, SchemaVersion, TaskId, database_key,
    },
};

/// The name of the database directory in the cache directory.
pub const DATABASE_DIR: &str = "db";

/// The name a wiped database directory has while [`Store::wipe`] removes it.
const WIPED_DIR: &str = "db.wiped";

/// The number of immutable memory tables, and of level-0 files, at which `SurrealKV`
/// would stall writes, as the store sets it: the largest number there is.
///
/// `SurrealKV` paces writes once a count passes 2 below its memory-table threshold
/// or 4 below its level-0 threshold, and stalls them at the threshold, so with this
/// value it would take more than `usize::MAX - 4` memory tables or files in one
/// database, which no process can hold: write pacing, which waited on a thread in
/// published `SurrealKV`, never runs, and no write stalls.
pub const STALL_THRESHOLD: usize = usize::MAX;

/// The options the store opens the database in `path` with: the direct clock, which
/// starts no thread, and stall thresholds that keep write pacing from running.
fn tree_options(path: PathBuf) -> Options {
    Options::new_with_clock(ClockMode::Direct)
        .with_path(path)
        .with_memtable_stall_threshold(STALL_THRESHOLD)
        .with_l0_stall_threshold(STALL_THRESHOLD)
}

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
/// state, and the permission and owner changes that a cache check finds. Version 10
/// added the `timestamps` option to the step specification, the modification and
/// change times to every directory state, and the timestamp change that a cache
/// check finds. Version 11 keeps directory listings in the order the file system
/// returned their entries, with a hash of that order and a hash of the same entries
/// sorted, and adds the names of the entries that changed and the order change to
/// what a cache check finds. Version 12 adds the reason for a run in which files
/// changed after the command first read them or ran them, which lists each such
/// file and whether the command wrote it, and, like the other reasons a run's
/// result is not cached, marks a record that is never a cache hit. Version 13 adds
/// the xattr access bit, the extended attributes recorded with a path whose
/// attributes the command read or listed, each name with a hash of its value, for
/// the path and, for a symbolic link, for what it points to, and the change that
/// lists the attributes that appeared, disappeared, or changed. Version 14 adds the
/// numbers of the system calls that a command made and that the tracer does not
/// know to every step record, whether or not its result was cached, and the reason
/// for a run that made such calls, which, like the other reasons a run's result is
/// not cached, marks a record that is never a cache hit. Version 15 adds the
/// connections that a command made to IPv4 and IPv6 addresses and to Unix sockets,
/// by path or abstract name, to every step record, whether or not its result was
/// cached; they never keep a result out of the cache. Version 16 adds the cache
/// identity, in the `meta/` family: the host name, the effective user and group, the
/// supplementary groups, the umask, the machine architecture, and the CPU model of
/// the process that created the database, which every later open compares with the
/// current values. Version 17 adds the machine state that a command started with to
/// every step record, the `machine` names to the step specification, the machine
/// state values that a cache check finds changed, and the reasons that name a change
/// of the `machine` names or of the machine state. Version 18 adds the reason of a
/// command that runs because its task is marked `cache=never`, to step records and
/// to the explanations of step positions. Version 19 adds the reason of a command
/// that runs because `kizz run --no-cache` was given, to step records and to the
/// explanations of step positions. Version 20 records explicit untraced execution
/// and its explanation, with a distinct key and no reuse across builds.
pub const SCHEMA_VERSION: u32 = 20;

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
    /// The store was called from inside an asynchronous execution context, where
    /// waiting for the database would panic, so it did not wait.
    #[error("the cache database cannot be waited for from inside an asynchronous task")]
    InsideRuntime,
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
    /// The database was stamped with another cache identity: another machine or user
    /// created it.
    #[error(
        "the cache database at `{}` belongs to another machine or user, so kizz refuses to \
         use it: {}; the cache is not portable between machines and users; run `kizz cache \
         reset --all` to wipe the cache, or set `KIZZ_CACHE_DIR` to use another cache",
        path.display(),
        identity_differences_text(differences)
    )]
    IdentityMismatch {
        /// The database directory.
        path: PathBuf,
        /// Each value that differs, stored and current, in the order of the fields of
        /// [`CacheIdentity`].
        differences: Vec<IdentityDifference>,
    },
    /// The database has this schema version but no cache identity, so kizz cannot
    /// tell which machine and user it belongs to.
    #[error(
        "the cache database at `{}` has no cache identity, so kizz cannot tell which machine \
         and user it belongs to and refuses to use it; the cache is not portable between \
         machines and users; run `kizz cache reset --all` to wipe the cache, or set \
         `KIZZ_CACHE_DIR` to use another cache",
        path.display()
    )]
    IdentityMissing {
        /// The database directory.
        path: PathBuf,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// The entries of every family but the database facts: the schema version and the
    /// cache identity.
    pub entries: EntryCounts,
    /// The schema version stored in the database, or `None` when there is no
    /// database or it holds no readable version.
    pub schema_version: Option<u32>,
    /// The cache identity stored in the database, or `None` when there is no
    /// database or it holds no readable identity.
    pub identity: Option<CacheIdentity>,
}

/// The cache database, shared by the threads of one process.
pub struct Store {
    /// The `SurrealKV` tree. Declared before the runtime so that it is dropped first.
    tree: Tree,
    /// The runtime that drives the tree's commits and background work.
    runtime: StoreRuntime,
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
    /// current schema version and the current [`CacheIdentity`], if it does not exist.
    ///
    /// An existing database's schema version is checked first, then its identity.
    ///
    /// # Errors
    ///
    /// [`StoreError::SchemaMismatch`] when the database has another schema version,
    /// [`StoreError::IdentityMismatch`] when it has another identity,
    /// [`StoreError::IdentityMissing`] when it has this schema version but no
    /// identity, and another [`StoreError`] when the database cannot be created or
    /// opened.
    pub fn open(cache_dir: &Path) -> Result<Self, StoreError> {
        let store = Self::open_tree(cache_dir)?;
        store.check_stamp(&CacheIdentity::current())?;
        Ok(store)
    }

    /// Counts the entries of the database in the `db` directory of `cache_dir`,
    /// whatever its schema version, by their keys alone, and reads its schema version
    /// and its identity when it can, without decoding another record or changing
    /// anything. A cache without a database has no entries.
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
            let identity = txn
                .inner
                .get(database_key::<CacheIdentity>(&IdentityKey))?
                .and_then(|bytes| decode::<CacheIdentity>(&bytes).ok());
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
                identity,
            }
        };
        store.close()?;
        Ok(inventory)
    }

    /// Wipes the whole cache database in the `db` directory of `cache_dir`, whatever
    /// its schema version and identity; the next [`Store::open`] creates an empty one,
    /// stamped with the current identity. The caller holds the cache lock, so no
    /// process has the database open.
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
        // A current-thread runtime starts no thread: the tree's tasks run on the
        // threads that wait in `block_on`.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(StoreError::Runtime)?;
        let tree = {
            // The tree starts its background tasks on the runtime it is opened in.
            let _entered = runtime.enter();
            TreeBuilder::with_options(tree_options(path.clone())).build()?
        };
        Ok(Self {
            tree,
            runtime: StoreRuntime(Some(runtime)),
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
        wait_for(&self.runtime, self.tree.close())??;
        Ok(())
    }

    /// Writes the schema version and `current`, the identity of this process, into a
    /// new database, in one transaction, or checks the version of an existing one
    /// first and then its identity against `current`.
    ///
    /// A database of this schema version always gets its identity in the transaction
    /// that creates it, so one that has none is refused: kizz cannot tell whose
    /// cache it is, and only `kizz cache reset --all` makes it usable again.
    fn check_stamp(
        &self,
        current: &CacheIdentity,
    ) -> Result<(), StoreError> {
        let mut txn = self.write()?;
        match txn.get::<SchemaVersion>(&SchemaKey)? {
            Some(SchemaVersion {
                version: SCHEMA_VERSION,
            }) => {},
            Some(SchemaVersion { version }) => {
                return Err(StoreError::SchemaMismatch {
                    path: self.path.clone(),
                    found: version,
                    expected: SCHEMA_VERSION,
                });
            },
            None => {
                txn.put(&SchemaKey, &SchemaVersion {
                    version: SCHEMA_VERSION,
                })?;
                txn.put(&IdentityKey, current)?;
                return txn.commit();
            },
        }
        let Some(stored) = txn.get::<CacheIdentity>(&IdentityKey)? else {
            return Err(StoreError::IdentityMissing {
                path: self.path.clone(),
            });
        };
        let differences = stored.differences(current);
        if differences.is_empty() {
            Ok(())
        } else {
            Err(StoreError::IdentityMismatch {
                path: self.path.clone(),
                differences,
            })
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
        match wait_for(&self.store.runtime, self.inner.commit())? {
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

/// The store's runtime, which never panics when it is dropped ("Kizz never
/// panics").
///
/// Dropping a Tokio runtime waits for its blocking pool, which panics with "Cannot
/// drop a runtime in a context where blocking is not allowed" inside an
/// asynchronous context. Kizz drops no store there, but should one be dropped
/// there, its runtime shuts down in the background, with
/// `Runtime::shutdown_background`, which does not wait, instead; elsewhere it shuts
/// down as usual. The runtime is `None` only while it is dropped.
struct StoreRuntime(Option<tokio::runtime::Runtime>);

impl StoreRuntime {
    /// The runtime, until it is dropped.
    const fn get(&self) -> Option<&tokio::runtime::Runtime> {
        self.0.as_ref()
    }
}

impl Drop for StoreRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take()
            && tokio::runtime::Handle::try_current().is_ok()
        {
            runtime.shutdown_background();
        }
    }
}

/// Runs `future` to its end on `runtime` with `Runtime::block_on`, after checking
/// that the calling thread is in no runtime, where `block_on` would panic.
///
/// # Errors
///
/// [`StoreError::InsideRuntime`] when the calling thread is in a runtime; the
/// future is dropped without running. [`StoreError::Runtime`] for a runtime that
/// is already gone, which only a store being dropped has.
fn wait_for<F: Future>(
    runtime: &StoreRuntime,
    future: F,
) -> Result<F::Output, StoreError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(StoreError::InsideRuntime);
    }
    let Some(runtime) = runtime.get() else {
        return Err(StoreError::Runtime(io::Error::other(
            "the cache database runtime has shut down",
        )));
    };
    Ok(runtime.block_on(future))
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
#[allow(clippy::disallowed_macros, reason = "test code: the tests assert")]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// The store's runtime, dropped inside an asynchronous context, where dropping a
    /// Tokio runtime panics with "Cannot drop a runtime in a context where blocking
    /// is not allowed", shuts down in the background instead, and the context goes
    /// on; dropped outside one, it shuts down as usual.
    #[test]
    fn the_runtime_drops_without_a_panic_inside_an_asynchronous_context() {
        let outer = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let inside = StoreRuntime(Some(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
        ));
        assert!(inside.get().is_some());
        outer.block_on(async move { drop(inside) });
        let outside = StoreRuntime(Some(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
        ));
        drop(outside);
        let gone = StoreRuntime(None);
        assert!(matches!(
            wait_for(&gone, async { 5 }),
            Err(StoreError::Runtime(_))
        ));
    }

    /// Waiting for the database from inside a runtime is
    /// [`StoreError::InsideRuntime`], where `Runtime::block_on` would panic, and
    /// the future never runs; outside a runtime, it runs to its end.
    #[test]
    fn waiting_inside_a_runtime_is_an_error() {
        let outer = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let runtime = StoreRuntime(Some(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
        ));
        let ran = Cell::new(false);
        let entered = outer.enter();
        let inside = wait_for(&runtime, async { ran.set(true) });
        assert!(
            matches!(inside, Err(StoreError::InsideRuntime)),
            "{inside:?}"
        );
        assert!(!ran.get());
        assert_eq!(
            StoreError::InsideRuntime.to_string(),
            "the cache database cannot be waited for from inside an asynchronous task"
        );
        drop(entered);
        assert_eq!(wait_for(&runtime, async { 5 }).unwrap(), 5);
    }

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

    /// The identity mismatch names each difference, stored and current, says that the
    /// cache is not portable, and gives both ways on.
    #[test]
    fn identity_mismatch_names_each_difference_and_both_ways_on() {
        let message = StoreError::IdentityMismatch {
            path: PathBuf::from("/cache/db"),
            differences: vec![
                IdentityDifference {
                    part: crate::IdentityPart::Host,
                    stored: String::from(r#""old-host""#),
                    current: String::from(r#""new-host""#),
                },
                IdentityDifference {
                    part: crate::IdentityPart::Umask,
                    stored: String::from("0022"),
                    current: String::from("0077"),
                },
            ],
        }
        .to_string();
        assert_eq!(
            message,
            "the cache database at `/cache/db` belongs to another machine or user, so kizz \
             refuses to use it: host name: \"old-host\" in the cache, \"new-host\" now; umask: \
             0022 in the cache, 0077 now; the cache is not portable between machines and users; \
             run `kizz cache reset --all` to wipe the cache, or set `KIZZ_CACHE_DIR` to use \
             another cache"
        );
    }

    /// The missing-identity error says that the identity is missing, that the cache
    /// is not portable, and gives both ways on.
    #[test]
    fn identity_missing_says_so_and_gives_both_ways_on() {
        let message = StoreError::IdentityMissing {
            path: PathBuf::from("/cache/db"),
        }
        .to_string();
        assert_eq!(
            message,
            "the cache database at `/cache/db` has no cache identity, so kizz cannot tell \
             which machine and user it belongs to and refuses to use it; the cache is not \
             portable between machines and users; run `kizz cache reset --all` to wipe the \
             cache, or set `KIZZ_CACHE_DIR` to use another cache"
        );
    }

    /// The check that every open makes refuses a stored identity whose CPU models
    /// differ from the current ones, also for the lists that the joined text of earlier
    /// versions showed alike, naming the CPU model alone; its error text shows the
    /// stored and the current list differently; and it accepts the stored models
    /// themselves.
    #[test]
    fn the_open_check_refuses_and_tells_apart_cpu_models_that_once_showed_alike() {
        let base = CacheIdentity::current();
        for collision in crate::identity::colliding_cpu_models() {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            let stored = CacheIdentity {
                cpu_models: collision.stored,
                ..base.clone()
            };
            let current = CacheIdentity {
                cpu_models: collision.current,
                ..base.clone()
            };
            let mut txn = store.write().unwrap();
            txn.put(&IdentityKey, &stored).unwrap();
            txn.commit().unwrap();
            store.check_stamp(&stored).unwrap();
            let error = store.check_stamp(&current).unwrap_err();
            let message = error.to_string();
            match error {
                StoreError::IdentityMismatch { path, differences } => {
                    assert_eq!(path, dir.path().join(DATABASE_DIR));
                    assert_eq!(differences, vec![IdentityDifference {
                        part: crate::IdentityPart::CpuModel,
                        stored: String::from(collision.stored_text),
                        current: String::from(collision.current_text),
                    }]);
                },
                other => panic!("expected an identity mismatch, got {other:?}"),
            }
            assert_ne!(collision.stored_text, collision.current_text);
            let shown = format!(
                "refuses to use it: CPU model: {} in the cache, {} now; the cache is not",
                collision.stored_text, collision.current_text
            );
            assert!(message.contains(&shown), "{message}");
            store.close().unwrap();
        }
    }

    /// Version 20 adds explicit untraced execution to step specifications and
    /// explanations.
    #[test]
    fn schema_version_20_records_untraced_execution() {
        assert_eq!(SCHEMA_VERSION, 20);
    }

    /// The store's thresholds keep write pacing from running: `SurrealKV` paces a
    /// write only while a count of immutable memory tables passes its threshold less
    /// 2, or a count of level-0 files its threshold less 4 (`StallThresholds::new`
    /// and `WriteStallController::check` in `vendor/surrealkv/src/stall.rs`), and
    /// both counts are lengths of collections in memory, which never pass
    /// `isize::MAX`.
    #[test]
    fn write_pacing_cannot_run_under_the_store_thresholds() {
        let options = tree_options(PathBuf::from("/cache/db"));
        assert_eq!(options.memtable_stall_threshold, STALL_THRESHOLD);
        assert_eq!(options.l0_stall_threshold, STALL_THRESHOLD);
        let memtable_soft_limit = options.memtable_stall_threshold.saturating_sub(2).max(1);
        let l0_soft_limit = options.l0_stall_threshold.saturating_sub(4).max(1);
        let longest = usize::try_from(isize::MAX).unwrap();
        assert!(memtable_soft_limit > longest);
        assert!(l0_soft_limit > longest);
    }
}
