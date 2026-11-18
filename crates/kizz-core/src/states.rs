// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The in-memory concurrent cache of file states and hashes of one build.
//!
//! Recording a step and checking the cache both need the BLAKE3 hash of files that
//! were read, and many steps often read the same files. [`FileStates`] keeps, for
//! each path, the metadata of the regular file there and the hash of its content, in
//! a [`DashMap`] that every thread of the build shares, so a file that many steps
//! depend on is hashed once per build.
//!
//! An entry is dropped when a step records a write to its path. A lookup also
//! hashes the file again when its current metadata differs from the metadata the
//! entry holds, so a change that no step recorded is never hidden behind a stale
//! hash.
//!
//! `/proc` and `/sys` files that a `track` pattern matches never go through the
//! cache: their metadata says nothing about their content, so they are read and
//! hashed again whenever they are recorded or checked.
//!
//! [`FileStates`] holds the build's [`WorkerPool`]: every hash runs inside it, so
//! BLAKE3's parallel hashing of a large file splits its work on the pool, and the
//! cache check and the recording of observations run their parallel work there too.

use std::{
    io,
    path::{Path, PathBuf},
};

use dashmap::DashMap;
use kizz_store::{ContentHash, FileMeta};
use kizz_trace::{AccessKind, Accesses};

use crate::{observe::HashCounter, pool::WorkerPool};

/// The file states and hashes of one build, shared by every thread.
#[derive(Debug)]
pub struct FileStates {
    /// For each path, the metadata of the regular file there when it was hashed,
    /// and the hash of its content.
    entries: DashMap<PathBuf, (FileMeta, ContentHash)>,
    /// Counts the files whose content was hashed.
    hashed: HashCounter,
    /// The pool that runs the build's parallel work, hashing included.
    pool: WorkerPool,
}

impl FileStates {
    /// An empty cache, whose hashing and parallel work run on `pool`.
    #[must_use]
    pub fn new(pool: WorkerPool) -> Self {
        Self {
            entries: DashMap::new(),
            hashed: HashCounter::default(),
            pool,
        }
    }

    /// The pool that runs the build's parallel work.
    #[must_use]
    pub const fn pool(&self) -> &WorkerPool {
        &self.pool
    }

    /// The number of files whose content was hashed so far.
    #[must_use]
    pub fn files_hashed(&self) -> u64 {
        self.hashed.files()
    }

    /// The number of paths with a cached hash.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no path has a cached hash.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The hash of the content of the regular file at `path`, whose current
    /// metadata is `meta`: the cached hash when the cache holds one for `path` with
    /// the same metadata, otherwise a new hash, which the cache then keeps.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the file.
    pub fn hash(
        &self,
        path: &Path,
        meta: &FileMeta,
    ) -> io::Result<ContentHash> {
        let cached = self.entries.get(path).map(|entry| *entry.value());
        if let Some((cached_meta, hash)) = cached
            && cached_meta == *meta
        {
            return Ok(hash);
        }
        let hash = self.hash_uncached(path)?;
        self.entries.insert(path.to_path_buf(), (*meta, hash));
        Ok(hash)
    }

    /// The hash of the content of the file at `path`, read now, without the cache.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the file.
    pub fn hash_uncached(
        &self,
        path: &Path,
    ) -> io::Result<ContentHash> {
        self.pool.install(|| hash_file(path, &self.hashed))
    }

    /// Forget all cached file hashes after a command whose writes were not observed.
    pub fn clear(&self) {
        self.entries.clear();
    }

    /// Drop the entry of every path that `accesses` records a write to.
    pub fn forget_written(
        &self,
        accesses: &Accesses,
    ) {
        for (path, kinds) in accesses {
            if kinds.contains(AccessKind::Write) {
                self.entries.remove(path.as_path());
            }
        }
    }
}

/// The BLAKE3 hash of the content of the file at `path`, read with a memory map and
/// several threads of the pool that runs it when the file is large, and read
/// normally otherwise.
fn hash_file(
    path: &Path,
    counter: &HashCounter,
) -> io::Result<ContentHash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_mmap_rayon(path)?;
    counter.count();
    Ok(ContentHash(*hasher.finalize().as_bytes()))
}

/// Unit tests for the cache of file states and hashes.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::fs;

    use kizz_trace::AbsolutePath;

    use super::*;

    /// The metadata of the file at `path`, as the cache compares it.
    fn meta(path: &Path) -> FileMeta {
        crate::observe::file_meta(&fs::metadata(path).unwrap())
    }

    #[test]
    fn a_file_is_hashed_once_until_a_write_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.h");
        fs::write(&path, "int x;").unwrap();
        let states = FileStates::new(WorkerPool::new().unwrap());
        assert!(states.is_empty());

        let first = states.hash(&path, &meta(&path)).unwrap();
        let second = states.hash(&path, &meta(&path)).unwrap();
        assert_eq!(first, second);
        assert_eq!(states.files_hashed(), 1);
        assert_eq!(states.len(), 1);

        // A read or a check does not drop the entry; a write does, even when the
        // metadata the lookup brings is the same.
        let mut accesses = Accesses::new();
        let absolute = AbsolutePath::new(&path).unwrap();
        accesses.record(absolute.clone(), AccessKind::Read);
        accesses.record(absolute.clone(), AccessKind::Probe);
        states.forget_written(&accesses);
        assert_eq!(states.len(), 1);
        accesses.record(absolute, AccessKind::Write);
        states.forget_written(&accesses);
        assert!(states.is_empty());
        assert_eq!(states.hash(&path, &meta(&path)).unwrap(), first);
        assert_eq!(states.files_hashed(), 2);
    }

    #[test]
    fn different_metadata_hashes_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        fs::write(&path, "one").unwrap();
        let states = FileStates::new(WorkerPool::new().unwrap());
        let first = states.hash(&path, &meta(&path)).unwrap();
        fs::write(&path, "two").unwrap();
        let mut changed = meta(&path);
        // Whatever the clock's resolution, the inode number below never matches.
        changed.inode = changed.inode.wrapping_add(1);
        let second = states.hash(&path, &changed).unwrap();
        assert_ne!(first, second);
        assert_eq!(states.files_hashed(), 2);
        // Tracked files bypass the cache.
        assert_eq!(states.hash_uncached(&path).unwrap(), second);
        assert_eq!(states.files_hashed(), 3);
        assert!(states.hash(&dir.path().join("missing"), &changed).is_err());
    }
}
