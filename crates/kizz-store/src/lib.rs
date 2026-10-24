// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz cache: its location, its lock file, and the store of typed records.
//!
//! * [`location`]: the cache is `KIZZ_CACHE_DIR` when set, otherwise
//!   `$XDG_CACHE_HOME/kizz`, otherwise `~/.cache/kizz`.
//! * [`lock`]: one process at a time holds `kizz.lock` in the cache directory, and any
//!   other waits and says what it is waiting for.
//! * [`store`]: the records live in `SurrealKV`, embedded, in the `db` directory of the
//!   cache, as CBOR encoded with `minicbor`.
//! * [`records`]: the record families, their key layout, and their typed records.

pub mod location;
pub mod lock;
pub mod records;
pub mod store;

pub use location::{LocationError, cache_dir, cache_dir_from_env};
pub use lock::{CacheLock, LOCK_FILE_NAME, LockDetails, LockError, LockOptions};
pub use records::{
    Family, LastWriter, LearnedOrder, PathKey, PositionKey, Record, RecordKey, SchemaKey,
    SchemaVersion, StepKey, StepPosition, StepRecord, TaskId, TaskRecord, database_key,
    path_prefix,
};
pub use store::{DATABASE_DIR, SCHEMA_VERSION, Store, StoreError, Txn};
