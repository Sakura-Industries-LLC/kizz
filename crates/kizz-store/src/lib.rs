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
//! * [`step`]: the contents of step records: the step specification, the observations
//!   with their path states, and how the step ended.
//! * [`explanation`]: the contents of explanations: why a step ran, or that it was
//!   cached, as the step-position records keep them for `kizz why`.

pub mod explanation;
pub mod location;
pub mod lock;
pub mod records;
pub mod step;
pub mod store;

pub use explanation::{
    BuildId, ChangedInput, EnvChange, Explanation, ModuleChange, ModuleChangeKind, Reason,
    SelfInflicted, TaskLabel, Verdict, Writer,
};
pub use location::{LocationError, cache_dir, cache_dir_from_env};
pub use lock::{CacheLock, LOCK_FILE_NAME, LockDetails, LockError, LockOptions};
pub use records::{
    Family, LastWriter, LearnedOrder, ModuleHash, PathKey, PositionKey, Record, RecordKey,
    SchemaKey, SchemaVersion, StepKey, StepPosition, TaskId, TaskRecord, WrittenBy, database_key,
    path_prefix,
};
pub use step::{
    AccessBits, Capture, Change, ContentHash, Difference, DirEntry, EntryKind, FileMeta, Listing,
    MAX_LISTED_NAMES, NotCachedReason, Observation, Owner, PERMISSION_BITS, PathState, ProgramPath,
    RecordedOutput, RunReason, SpecialKind, StepExit, StepRecord, StepSpecRecord, Times,
    permission_bits,
};
pub use store::{DATABASE_DIR, EntryCounts, Inventory, SCHEMA_VERSION, Store, StoreError, Txn};
