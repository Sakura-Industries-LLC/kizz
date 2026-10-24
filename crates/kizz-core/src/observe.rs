// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The state of every path a step touched, recorded when the step finishes.
//!
//! [`observe`] turns the accesses a tracer reported into observations, recording
//! the paths concurrently. Paths that an ignore pattern matches are left out and
//! counted. Each other path gets the [`PathState`] that "Path states" specifies:
//!
//! * absent paths: [`PathState::Absent`];
//! * regular files: size, modification and change times in nanoseconds, inode, device,
//!   and mode, plus a BLAKE3 hash of the content only when the file was read or run,
//!   hashed with several threads for large files, through the build's [`FileStates`], so
//!   a file is hashed once per build;
//! * directories: a hash of the sorted entry names and types when they were listed, plus
//!   the entries when there are at most [`MAX_LISTED_NAMES`], and otherwise only
//!   existence and type;
//! * symbolic links: the target, plus the state of what the link points to;
//! * devices, FIFOs, and sockets: type only.
//!
//! Paths under `/proc` and `/sys` are recorded as [`PathState::Untracked`] unless a
//! `track` pattern matches them; a tracked one gets its state, with the content of
//! a regular file read and hashed again whatever the access, never from the cache.

use std::{
    fs::{self, FileType, Metadata},
    io,
    os::unix::{
        ffi::OsStringExt as _,
        fs::{FileTypeExt as _, MetadataExt as _},
    },
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use kizz_store::{
    AccessBits, ContentHash, DirEntry, EntryKind, FileMeta, Listing, MAX_LISTED_NAMES, Observation,
    PathState, SpecialKind,
};
use kizz_trace::{AbsolutePath, AccessKind, AccessKinds, Accesses};
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};

use crate::{patterns::PathPatterns, states::FileStates};

/// The context string from which directory listing hashes are derived.
const LISTING_CONTEXT: &str = "kizz directory listing, version 1";

/// The kernel interfaces whose paths are recorded but not compared unless tracked.
const KERNEL_ROOTS: [&str; 2] = ["/proc", "/sys"];

/// Nanoseconds in a second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Whether `path` is under `/proc` or `/sys`, or is one of them.
#[must_use]
pub fn is_kernel_path(path: &Path) -> bool {
    KERNEL_ROOTS.iter().any(|root| path.starts_with(root))
}

/// Counts the files whose content was hashed, so tests can observe hashing.
#[derive(Debug, Default)]
pub struct HashCounter {
    /// The number of files hashed.
    files: AtomicU64,
}

impl HashCounter {
    /// A counter at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of files hashed so far.
    #[must_use]
    pub fn files(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
    }

    /// Count one hashed file.
    pub(crate) fn count(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
    }
}

/// The observations of one step.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observed {
    /// Every touched path that no ignore pattern matched, sorted by path, with its
    /// state.
    pub observations: Vec<Observation>,
    /// The number of touched paths that ignore patterns left out.
    pub ignored: u64,
}

/// The access kinds `kinds` as the store records them.
#[must_use]
pub fn access_bits(kinds: AccessKinds) -> AccessBits {
    AccessBits(kinds.iter().fold(0, |bits, kind| {
        bits | match kind {
            AccessKind::Read => AccessBits::READ,
            AccessKind::Probe => AccessBits::PROBE,
            AccessKind::List => AccessBits::LIST,
            AccessKind::Write => AccessBits::WRITE,
            AccessKind::Exec => AccessBits::EXEC,
        }
    }))
}

/// Record the state of every path in `accesses`, concurrently, leaving out and
/// counting the paths that `ignore` matches, and hashing the `/proc` and `/sys`
/// paths that `track` matches. Hashes go through `states`.
#[must_use]
pub fn observe(
    accesses: &Accesses,
    ignore: &PathPatterns,
    track: &PathPatterns,
    states: &FileStates,
) -> Observed {
    let (kept, ignored): (Vec<_>, Vec<_>) = accesses
        .iter()
        .map(|(path, kinds)| (path, *kinds))
        .partition(|(path, _)| !ignore.matches(path));
    let observations = kept
        .par_iter()
        .map(|&(path, kinds)| observation(path, kinds, track, states))
        .collect();
    Observed {
        observations,
        ignored: u64::try_from(ignored.len()).unwrap_or(u64::MAX),
    }
}

/// The observation of one path that a step touched with `kinds`.
fn observation(
    path: &AbsolutePath,
    kinds: AccessKinds,
    track: &PathPatterns,
    states: &FileStates,
) -> Observation {
    let kernel = is_kernel_path(path.as_path());
    let tracked = kernel && track.matches(path);
    let state = if kernel && !tracked {
        PathState::Untracked
    } else {
        path_state(path.as_path(), kinds, tracked, states)
    };
    Observation {
        path: path.as_path().to_path_buf(),
        access: access_bits(kinds),
        tracked,
        state,
    }
}

/// The state of `path`, which a step touched with `kinds`, without following a
/// final symbolic link. A regular file is hashed when it was read or run, through
/// the cache `states`, or when `force_hash` is set, read again without the cache.
#[must_use]
pub fn path_state(
    path: &Path,
    kinds: AccessKinds,
    force_hash: bool,
    states: &FileStates,
) -> PathState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return failed(&error),
    };
    if !metadata.file_type().is_symlink() {
        return state_of(path, &metadata, kinds, force_hash, states);
    }
    let target = match fs::read_link(path) {
        Ok(target) => target,
        Err(error) => return failed(&error),
    };
    let pointee = match fs::metadata(path) {
        Ok(metadata) => state_of(path, &metadata, kinds, force_hash, states),
        Err(error) => failed(&error),
    };
    PathState::Symlink {
        target,
        pointee: Box::new(pointee),
    }
}

/// The state for an error from reading a path's state: absent when nothing is
/// there, otherwise inaccessible.
pub(crate) fn failed(error: &io::Error) -> PathState {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => PathState::Absent,
        _ => {
            PathState::Inaccessible {
                errno: error.raw_os_error().unwrap_or(0),
            }
        },
    }
}

/// The state of `path`, whose metadata, with links followed or not, is `metadata`.
fn state_of(
    path: &Path,
    metadata: &Metadata,
    kinds: AccessKinds,
    force_hash: bool,
    states: &FileStates,
) -> PathState {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        let meta = file_meta(metadata);
        let hash = if force_hash {
            Some(states.hash_uncached(path))
        } else if kinds.contains(AccessKind::Read) || kinds.contains(AccessKind::Exec) {
            Some(states.hash(path, &meta))
        } else {
            None
        };
        let hash = match hash.transpose() {
            Ok(hash) => hash,
            Err(error) => return failed(&error),
        };
        PathState::File { meta, hash }
    } else if file_type.is_dir() {
        let listing = if kinds.contains(AccessKind::List) {
            match list(path) {
                Ok(listing) => Some(listing),
                Err(error) => return failed(&error),
            }
        } else {
            None
        };
        PathState::Directory { listing }
    } else {
        PathState::Special {
            kind: special_kind(file_type),
        }
    }
}

/// The type of a path that is neither a regular file, a directory, nor a link.
pub(crate) fn special_kind(file_type: FileType) -> SpecialKind {
    if file_type.is_block_device() {
        SpecialKind::BlockDevice
    } else if file_type.is_char_device() {
        SpecialKind::CharDevice
    } else if file_type.is_fifo() {
        SpecialKind::Fifo
    } else if file_type.is_socket() {
        SpecialKind::Socket
    } else {
        SpecialKind::Other
    }
}

/// A time in nanoseconds since the Unix epoch from its seconds and nanoseconds.
const fn nanos(
    seconds: i64,
    nanoseconds: i64,
) -> i64 {
    seconds
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(nanoseconds)
}

/// The metadata of a regular file that a cache check compares.
pub(crate) fn file_meta(metadata: &Metadata) -> FileMeta {
    FileMeta {
        size: metadata.size(),
        mtime_nanos: nanos(metadata.mtime(), metadata.mtime_nsec()),
        ctime_nanos: nanos(metadata.ctime(), metadata.ctime_nsec()),
        inode: metadata.ino(),
        device: metadata.dev(),
        mode: metadata.mode(),
    }
}

/// The type of a directory entry.
fn entry_kind(file_type: FileType) -> EntryKind {
    if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_block_device() {
        EntryKind::BlockDevice
    } else if file_type.is_char_device() {
        EntryKind::CharDevice
    } else if file_type.is_fifo() {
        EntryKind::Fifo
    } else if file_type.is_socket() {
        EntryKind::Socket
    } else {
        EntryKind::Unknown
    }
}

/// The listing of the directory at `path`: the hash of its sorted entry names and
/// types, and the entries when there are at most [`MAX_LISTED_NAMES`].
pub(crate) fn list(path: &Path) -> io::Result<Listing> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type().map_or(EntryKind::Unknown, entry_kind);
        entries.push(DirEntry {
            name: entry.file_name().into_vec(),
            kind,
        });
    }
    entries.sort();
    let mut hasher = blake3::Hasher::new_derive_key(LISTING_CONTEXT);
    for entry in &entries {
        let length = u64::try_from(entry.name.len()).unwrap_or(u64::MAX);
        hasher.update(&length.to_le_bytes());
        hasher.update(&entry.name);
        hasher.update(&[entry.kind.code()]);
    }
    Ok(Listing {
        hash: ContentHash(*hasher.finalize().as_bytes()),
        entries: (entries.len() <= MAX_LISTED_NAMES).then_some(entries),
    })
}
