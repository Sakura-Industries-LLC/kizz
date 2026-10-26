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
//!   mode, and owner, plus a BLAKE3 hash of the content only when the file was read or
//!   run, hashed with several threads for large files, through the build's
//!   [`FileStates`], so a file is hashed once per build;
//! * directories: mode, owner, and modification and change times, and when they were
//!   listed, also a hash of the entry names and types in the order the file system
//!   returned them, a hash of the same entries sorted, and the entries in that order when
//!   there are at most [`MAX_LISTED_NAMES`]; entry inode numbers are not recorded;
//! * symbolic links: the target, plus the state of what the link points to;
//! * devices, FIFOs, and sockets: type only.
//!
//! A path whose extended attributes the step read or listed, the xattr kind, also
//! records them with its state (see [`record_attributes`]): each name with a BLAKE3
//! hash of its value, sorted by name, for the path itself and, for a symbolic link,
//! for what it points to. Kizz reads them only through the attribute query of the
//! tracer interface, [`Tracer::extended_attributes`], because the standard library
//! cannot read them.
//!
//! Paths under `/proc` and `/sys` are recorded as [`PathState::Untracked`] unless a
//! `track` pattern matches them; a tracked one gets its state, with the content of
//! a regular file read and hashed again whatever the access, never from the cache.
//! A path that symbolic links lead into `/proc` or `/sys`, such as `/dev/fd/3` or
//! `/dev/stdin`, leaves only that final object untracked (see [`kernel_links`]): the
//! links on the way stay ordinary dependencies, each recorded with its target, so
//! retargeting, removing, or replacing one of them changes the step. A final link is
//! recorded as [`PathState::Symlink`] with an untracked pointee, and every other link
//! on the way is recorded as such a link too, as a probe.
//!
//! [`changed_after_read`] compares the first-read metadata that the tracer noted
//! with the recorded states, to find the files that changed during the run after
//! the command first read them or ran them.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    fs::{self, FileType, Metadata},
    io,
    os::unix::{
        ffi::OsStringExt as _,
        fs::{FileTypeExt as _, MetadataExt as _},
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use kizz_store::{
    AccessBits, Attribute, Attributes, ChangedRead, ContentHash, DirEntry, EntryKind, FileMeta,
    Listing, MAX_LISTED_NAMES, Observation, Owner, PathState, SpecialKind, Times,
};
use kizz_trace::{
    AbsolutePath, AccessKind, AccessKinds, Accesses, ExtendedAttributes, FileMetadata, FirstReads,
    Tracer,
};
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};

use crate::{patterns::PathPatterns, states::FileStates};

/// The context string from which directory listing hashes are derived, both the
/// hash of the entries in the order the file system returned them and the hash of
/// the same entries sorted.
const LISTING_CONTEXT: &str = "kizz directory listing, version 2";

/// The kernel interfaces whose paths are recorded but not compared unless tracked.
const KERNEL_ROOTS: [&str; 2] = ["/proc", "/sys"];

/// Nanoseconds in a second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The most symbolic links [`kernel_links`] follows, as many as a Linux path lookup
/// follows.
const MAX_LINKS: usize = 40;

/// Whether `path` is under `/proc` or `/sys`, or is one of them.
#[must_use]
pub fn is_kernel_path(path: &Path) -> bool {
    KERNEL_ROOTS.iter().any(|root| path.starts_with(root))
}

/// Whether `path` is under `/proc` or `/sys`, or the symbolic links on the way to it
/// lead there, as `/dev/fd` leads to `/proc/self/fd` and `/dev/stdin` to
/// `/proc/self/fd/0` (see [`kernel_links`]).
#[must_use]
pub fn reaches_kernel(path: &Path) -> bool {
    kernel_links(path).is_some()
}

/// The symbolic links outside `/proc` and `/sys`, each with its target, that lead
/// `path` into `/proc` or `/sys`, in the order a path lookup follows them; `None`
/// when `path` does not reach `/proc` or `/sys`, and no links when it is under one
/// of them already.
///
/// A path that reaches `/proc` or `/sys` describes whichever process looks at it:
/// read when kizz records or checks a step, it would describe kizz, not the
/// command, so the object it reaches is recorded like any other `/proc` or `/sys`
/// path. The links that lead there are ordinary files of the project or the
/// system, so they are not. The lookup resolves each component in turn, follows
/// at most 40 links, and stops at the first component under `/proc` or `/sys`,
/// never reading anything there. A path that cannot be resolved, such as a missing
/// one, is judged by its own form.
#[must_use]
pub fn kernel_links(path: &Path) -> Option<Vec<(PathBuf, PathBuf)>> {
    kernel_route(path).map(|route| route.links)
}

/// How a path reaches `/proc` or `/sys`.
#[derive(Debug, Default)]
struct KernelRoute {
    /// The symbolic links outside `/proc` and `/sys` on the way, each with its
    /// target, in lookup order.
    links: Vec<(PathBuf, PathBuf)>,
    /// The target of the path's own final component, when that is a symbolic link
    /// outside `/proc` and `/sys`.
    own_target: Option<PathBuf>,
}

/// How `path` reaches `/proc` or `/sys`, or `None` when it does not, as
/// [`kernel_links`] describes.
fn kernel_route(path: &Path) -> Option<KernelRoute> {
    let mut route = KernelRoute::default();
    if is_kernel_path(path) {
        return Some(route);
    }
    // Each name to look up, marked when it is the path's own final component.
    let mut pending: VecDeque<(OsString, bool)> = VecDeque::new();
    push_components(&mut pending, path, true);
    let mut resolved = PathBuf::from("/");
    while let Some((name, own)) = pending.pop_front() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&name);
        if is_kernel_path(&candidate) {
            return Some(route);
        }
        let metadata = fs::symlink_metadata(&candidate).ok()?;
        if !metadata.file_type().is_symlink() {
            resolved = candidate;
            continue;
        }
        if route.links.len() >= MAX_LINKS {
            return None;
        }
        let target = fs::read_link(&candidate).ok()?;
        if target.is_absolute() {
            resolved = PathBuf::from("/");
        }
        let mut rest = VecDeque::new();
        push_components(&mut rest, &target, false);
        rest.append(&mut pending);
        pending = rest;
        if own {
            route.own_target = Some(target.clone());
        }
        route.links.push((candidate, target));
    }
    is_kernel_path(&resolved).then_some(route)
}

/// Append the names and `..` components of `path` to `pending`, leaving out the
/// root and `.`, and marking the last one with `own`.
fn push_components(
    pending: &mut VecDeque<(OsString, bool)>,
    path: &Path,
    own: bool,
) {
    for component in path.components() {
        match component {
            Component::Normal(name) => pending.push_back((name.to_os_string(), false)),
            Component::ParentDir => pending.push_back((OsString::from(".."), false)),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {},
        }
    }
    if let Some(last) = pending.back_mut() {
        last.1 = own;
    }
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
            AccessKind::Xattr => AccessBits::XATTR,
        }
    }))
}

/// Record the state of every path in `accesses`, concurrently on the worker pool of
/// `states`, leaving out and counting the paths that `ignore` matches, and hashing
/// the `/proc` and `/sys` paths that `track` matches. Hashes go through `states`, and
/// the extended attributes of the paths whose attributes were read or listed come
/// from the attribute query of `tracer`.
#[must_use]
pub fn observe(
    accesses: &Accesses,
    ignore: &PathPatterns,
    track: &PathPatterns,
    states: &FileStates,
    tracer: &dyn Tracer,
) -> Observed {
    let (kept, ignored): (Vec<_>, Vec<_>) = accesses
        .iter()
        .map(|(path, kinds)| (path, *kinds))
        .partition(|(path, _)| !ignore.matches(path));
    let recorded: Vec<(Observation, Vec<Observation>)> = states.pool().install(|| {
        kept.par_iter()
            .map(|&(path, kinds)| observation(path, kinds, track, states, tracer))
            .collect()
    });
    let mut observations = BTreeMap::new();
    let mut links = Vec::new();
    for (observation, on_the_way) in recorded {
        links.extend(on_the_way);
        observations.insert(observation.path.clone(), observation);
    }
    for link in links {
        let ignored_link = AbsolutePath::new(&link.path).is_ok_and(|path| ignore.matches(&path));
        if !ignored_link {
            observations.entry(link.path.clone()).or_insert(link);
        }
    }
    Observed {
        observations: observations.into_values().collect(),
        ignored: u64::try_from(ignored.len()).unwrap_or(u64::MAX),
    }
}

/// The observation of one path that a step touched with `kinds`, and the
/// observations of the symbolic links that lead it into `/proc` or `/sys`. A path
/// whose extended attributes were read or listed records them, unless it leads
/// into `/proc` or `/sys` and no `track` pattern matched it.
fn observation(
    path: &AbsolutePath,
    kinds: AccessKinds,
    track: &PathPatterns,
    states: &FileStates,
    tracer: &dyn Tracer,
) -> (Observation, Vec<Observation>) {
    let route = kernel_route(path.as_path());
    let tracked = route.is_some() && track.matches(path);
    let attributes = || {
        kinds
            .contains(AccessKind::Xattr)
            .then(|| record_attributes(path.as_path(), tracer))
    };
    let Some(route) = route else {
        let observation = Observation {
            path: path.as_path().to_path_buf(),
            access: access_bits(kinds),
            tracked,
            state: path_state(path.as_path(), kinds, false, states),
            attributes: attributes(),
        };
        return (observation, Vec::new());
    };
    let state = if tracked {
        path_state(path.as_path(), kinds, true, states)
    } else {
        // Only the object under `/proc` or `/sys` is untracked: a final link outside
        // them keeps its target.
        route
            .own_target
            .map_or(PathState::Untracked, untracked_link)
    };
    let on_the_way = route
        .links
        .into_iter()
        .map(|(link, target)| {
            Observation {
                path: link,
                access: AccessBits(AccessBits::PROBE),
                tracked: false,
                state: untracked_link(target),
                attributes: None,
            }
        })
        .collect();
    let observation = Observation {
        path: path.as_path().to_path_buf(),
        access: access_bits(kinds),
        tracked,
        state,
        attributes: if tracked { attributes() } else { None },
    };
    (observation, on_the_way)
}

/// The extended attributes of `path` now, as a path whose attributes were read or
/// listed records them, read through the attribute query of `tracer`: those of the
/// path itself, without following a final symbolic link, and, when the path is a
/// symbolic link, those of what it points to. Each list holds every name with a
/// BLAKE3 hash of its value, sorted by name, or is `None` when the query fails, as
/// it does when nothing is at the path; a file system without attributes has none.
///
/// A cache check reads them again the same way and compares the two.
#[must_use]
pub fn record_attributes(
    path: &Path,
    tracer: &dyn Tracer,
) -> Attributes {
    let hashed = |follow: bool| tracer.extended_attributes(path, follow).ok().map(hashes);
    let link = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
    Attributes {
        own: hashed(false),
        pointee: if link { hashed(true) } else { None },
    }
}

/// Each attribute of `attributes` with a BLAKE3 hash of its value, sorted by name.
fn hashes(attributes: ExtendedAttributes) -> Vec<Attribute> {
    attributes
        .into_iter()
        .map(|(name, value)| {
            Attribute {
                name,
                hash: ContentHash(*blake3::hash(&value).as_bytes()),
            }
        })
        .collect()
}

/// The state of a symbolic link to `target`, which leads into `/proc` or `/sys`:
/// the target, with an untracked pointee.
fn untracked_link(target: PathBuf) -> PathState {
    PathState::Symlink {
        target,
        pointee: Box::new(PathState::Untracked),
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
        PathState::Directory {
            mode: metadata.mode(),
            owner: owner(metadata),
            listing,
            times: times(metadata),
        }
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

/// The modification and change times of a regular file or a directory whose
/// metadata is `metadata`.
pub(crate) fn times(metadata: &Metadata) -> Times {
    Times {
        mtime_nanos: nanos(metadata.mtime(), metadata.mtime_nsec()),
        ctime_nanos: nanos(metadata.ctime(), metadata.ctime_nsec()),
    }
}

/// The metadata of a regular file that a cache check compares.
pub(crate) fn file_meta(metadata: &Metadata) -> FileMeta {
    let Times {
        mtime_nanos,
        ctime_nanos,
    } = times(metadata);
    FileMeta {
        size: metadata.size(),
        mtime_nanos,
        ctime_nanos,
        inode: metadata.ino(),
        device: metadata.dev(),
        mode: metadata.mode(),
        owner: owner(metadata),
    }
}

/// The owner of a file or directory whose metadata is `metadata`.
pub(crate) fn owner(metadata: &Metadata) -> Owner {
    Owner {
        uid: metadata.uid(),
        gid: metadata.gid(),
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

/// The listing of the directory at `path`, with its entries in the order the file
/// system returns them (see [`listing`]).
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
    Ok(listing(entries))
}

/// The listing of `entries`, in the order given: the hash of their names and types
/// in that order, the hash of the same entries sorted by name and type, and the
/// entries in that order when there are at most [`MAX_LISTED_NAMES`].
#[must_use]
pub fn listing(entries: Vec<DirEntry>) -> Listing {
    let mut sorted: Vec<&DirEntry> = entries.iter().collect();
    sorted.sort();
    let sorted_hash = listing_hash(sorted);
    Listing {
        hash: listing_hash(&entries),
        sorted_hash,
        entries: (entries.len() <= MAX_LISTED_NAMES).then_some(entries),
    }
}

/// The hash of `entries`, in the order given: for each entry, the length of its
/// name as eight little-endian bytes, the name, and the code of its type.
fn listing_hash<'a>(entries: impl IntoIterator<Item = &'a DirEntry>) -> ContentHash {
    let mut hasher = blake3::Hasher::new_derive_key(LISTING_CONTEXT);
    for entry in entries {
        let length = u64::try_from(entry.name.len()).unwrap_or(u64::MAX);
        hasher.update(&length.to_le_bytes());
        hasher.update(&entry.name);
        hasher.update(&[entry.kind.code()]);
    }
    ContentHash(*hasher.finalize().as_bytes())
}

/// The files that changed during a run after the command first read them or ran
/// them, sorted by path: each path in `first_reads` whose recorded state in
/// `observations`, following a final symbolic link, is not a regular file with the
/// noted device, inode, size, modification and change times, mode, and owner.
/// No file, another type, and an inaccessible path are changes too. Each says
/// whether the command wrote the path, as its access kinds show.
///
/// A path that no observation holds, because an ignore pattern matched it, is not
/// compared, and neither is a path under `/proc` or `/sys`, or one that symbolic
/// links lead there, tracked or not.
#[must_use]
pub fn changed_after_read(
    observations: &[Observation],
    first_reads: &FirstReads,
) -> Vec<ChangedRead> {
    first_reads
        .iter()
        .filter_map(|(path, noted)| {
            let path = path.as_path();
            let index = observations
                .binary_search_by(|observation| observation.path.as_path().cmp(path))
                .ok()?;
            let observation = observations.get(index)?;
            if observation.tracked || is_kernel_path(path) {
                return None;
            }
            let unchanged = same_file(&observation.state, noted)?;
            (!unchanged).then(|| {
                ChangedRead {
                    path: path.to_path_buf(),
                    written: observation.access.contains(AccessBits::WRITE),
                }
            })
        })
        .collect()
}

/// Whether the recorded `state`, following a final symbolic link, is the regular
/// file that `noted` describes, unchanged; `None` when the state is untracked, so
/// it is not compared.
fn same_file(
    state: &PathState,
    noted: &FileMetadata,
) -> Option<bool> {
    match state {
        PathState::Untracked => None,
        PathState::Symlink { pointee, .. } => same_file(pointee, noted),
        PathState::File { meta, .. } => {
            Some(
                meta.device == noted.device
                    && meta.inode == noted.inode
                    && meta.size == noted.size
                    && meta.mtime_nanos == noted.mtime_nanos
                    && meta.ctime_nanos == noted.ctime_nanos
                    && meta.mode == noted.mode
                    && meta.owner
                        == (Owner {
                            uid: noted.uid,
                            gid: noted.gid,
                        }),
            )
        },
        PathState::Absent
        | PathState::Directory { .. }
        | PathState::Special { .. }
        | PathState::Inaccessible { .. } => Some(false),
    }
}

/// Unit tests for listing hashes, for recognizing paths that lead into `/proc` and
/// `/sys`, and for comparing first reads with recorded states.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// A regular-file entry named `name`.
    fn entry(name: &str) -> DirEntry {
        DirEntry {
            name: name.as_bytes().to_vec(),
            kind: EntryKind::File,
        }
    }

    /// The noted metadata of a regular file with `inode`.
    fn noted(inode: u64) -> FileMetadata {
        FileMetadata {
            device: 1,
            inode,
            size: 2,
            mtime_nanos: 3,
            ctime_nanos: 4,
            mode: 0o100_644,
            uid: 5,
            gid: 6,
        }
    }

    /// The recorded state of the regular file that `noted(inode)` describes.
    fn file(inode: u64) -> PathState {
        PathState::File {
            meta: FileMeta {
                size: 2,
                mtime_nanos: 3,
                ctime_nanos: 4,
                inode,
                device: 1,
                mode: 0o100_644,
                owner: Owner { uid: 5, gid: 6 },
            },
            hash: None,
        }
    }

    /// An observation of `path` with `access` and `state`.
    fn observed(
        path: &str,
        access: u8,
        tracked: bool,
        state: PathState,
    ) -> Observation {
        Observation {
            path: PathBuf::from(path),
            access: AccessBits(access),
            tracked,
            state,
            attributes: None,
        }
    }

    /// Each noted file is compared with its final state through a final link; any
    /// differing field, another type, or no file is a change, with whether the
    /// command wrote the path; ignored, untracked and tracked kernel paths are not
    /// compared.
    #[test]
    fn first_reads_are_compared_with_the_recorded_states() {
        let read = AccessBits::READ;
        let written = AccessBits::READ | AccessBits::WRITE;
        let link = |state| {
            PathState::Symlink {
                target: PathBuf::from("t"),
                pointee: Box::new(state),
            }
        };
        let mut moved = file(1);
        if let PathState::File { meta, .. } = &mut moved {
            meta.mode = 0o100_600;
        }
        let observations = vec![
            observed("/p/absent", written, false, PathState::Absent),
            observed("/p/dir", read, false, PathState::Directory {
                mode: 0o040_755,
                owner: Owner { uid: 5, gid: 6 },
                listing: None,
                times: Times {
                    mtime_nanos: 3,
                    ctime_nanos: 4,
                },
            }),
            observed("/p/link-other", written, false, link(file(9))),
            observed("/p/link-same", read, false, link(file(1))),
            observed("/p/mode", read, false, moved),
            observed("/p/same", read, false, file(1)),
            observed("/proc/version", read, true, file(9)),
            observed("/sys/kernel", read, false, PathState::Untracked),
            observed("/x/untracked", read, false, link(PathState::Untracked)),
        ];
        let mut first_reads = FirstReads::new();
        let accesses = Accesses::new();
        for path in [
            "/p/absent",
            "/p/dir",
            "/p/ignored",
            "/p/link-other",
            "/p/link-same",
            "/p/mode",
            "/p/same",
            "/proc/version",
            "/sys/kernel",
            "/x/untracked",
        ] {
            first_reads.note(&AbsolutePath::new(path).unwrap(), noted(1), &accesses);
        }
        let changed: Vec<(String, bool)> = changed_after_read(&observations, &first_reads)
            .into_iter()
            .map(|file| (file.path.display().to_string(), file.written))
            .collect();
        assert_eq!(changed, [
            ("/p/absent".to_owned(), true),
            ("/p/dir".to_owned(), false),
            ("/p/link-other".to_owned(), true),
            ("/p/mode".to_owned(), false),
        ]);
    }

    /// The same entries in two orders have different listing hashes and the same
    /// sorted hash, and the same entries in the same order have the same hashes.
    #[test]
    fn order_changes_the_listing_hash_and_not_the_sorted_hash() {
        let forward = listing(vec![entry("a"), entry("b"), entry("c")]);
        let backward = listing(vec![entry("c"), entry("b"), entry("a")]);
        assert_ne!(forward.hash, backward.hash);
        assert_eq!(forward.sorted_hash, backward.sorted_hash);
        let again = listing(vec![entry("a"), entry("b"), entry("c")]);
        assert_eq!(again, forward);
        // Sorted entries hash the same both ways; other entries hash differently.
        assert_eq!(forward.hash, forward.sorted_hash);
        let other = listing(vec![entry("a"), entry("b")]);
        assert_ne!(other.sorted_hash, forward.sorted_hash);
        // A type is part of an entry.
        let retyped = listing(vec![entry("a"), entry("b"), DirEntry {
            name: b"c".to_vec(),
            kind: EntryKind::Directory,
        }]);
        assert_ne!(retyped.sorted_hash, forward.sorted_hash);
        assert_eq!(
            backward.entries,
            Some(vec![entry("c"), entry("b"), entry("a")])
        );
    }

    /// A listing keeps the entries in the order the file system returns them, and
    /// keeps them only up to the limit, while both hashes cover every entry.
    #[test]
    fn listings_keep_the_file_system_order_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["m", "a", "z", "k"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        fs::create_dir(dir.path().join("d")).unwrap();
        let returned: Vec<DirEntry> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                DirEntry {
                    name: entry.file_name().into_vec(),
                    kind: entry_kind(entry.file_type().unwrap()),
                }
            })
            .collect();
        let listed = list(dir.path()).unwrap();
        assert_eq!(listed, listing(returned.clone()));
        assert_eq!(listed.entries, Some(returned));

        let many: Vec<DirEntry> = (0..=MAX_LISTED_NAMES)
            .map(|index| entry(&index.to_string()))
            .collect();
        let large = listing(many.clone());
        assert_eq!(large.entries, None);
        let fewer = listing(many.iter().skip(1).cloned().collect());
        assert_ne!(large.hash, fewer.hash);
        assert_ne!(large.sorted_hash, fewer.sorted_hash);
    }

    /// Paths under `/proc` and `/sys`, and the standard links into them, reach a
    /// kernel interface.
    #[test]
    fn kernel_paths_and_the_standard_links_into_them_reach_the_kernel() {
        for path in ["/proc/version", "/sys/kernel", "/dev/fd/0", "/dev/stdin"] {
            assert!(reaches_kernel(Path::new(path)), "{path}");
        }
    }

    /// A directory link, a final link, a chain of links, and a relative link that
    /// lead into `/proc` or `/sys` reach a kernel interface.
    #[test]
    fn links_that_lead_into_proc_or_sys_reach_the_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        symlink("/proc/self/fd", root.join("fd")).unwrap();
        symlink("/proc/self/fd/0", root.join("stdin")).unwrap();
        symlink("stdin", root.join("chain")).unwrap();
        let up = "../".repeat(root.components().count().saturating_sub(1));
        symlink(format!("{up}sys/kernel"), root.join("relative")).unwrap();
        for name in ["fd/0", "fd/1", "stdin", "chain", "relative"] {
            assert!(reaches_kernel(&root.join(name)), "{name}");
        }
    }

    /// Files, links to files, missing paths, dangling links, and link loops do not
    /// reach a kernel interface, and the loop ends.
    #[test]
    fn other_paths_do_not_reach_the_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("file"), "content").unwrap();
        symlink("file", root.join("to-file")).unwrap();
        symlink("missing", root.join("dangling")).unwrap();
        symlink("loop-b", root.join("loop-a")).unwrap();
        symlink("loop-a", root.join("loop-b")).unwrap();
        for name in [
            "file",
            "to-file",
            "missing",
            "missing/below",
            "dangling",
            "loop-a",
        ] {
            assert!(!reaches_kernel(&root.join(name)), "{name}");
        }
        assert!(!reaches_kernel(Path::new("/")));
    }

    /// The links that lead a path into `/proc` or `/sys` are listed with their
    /// targets, in lookup order, at the places a lookup finds them, and no link
    /// under `/proc` or `/sys` is among them.
    #[test]
    fn the_links_that_lead_into_proc_or_sys_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir(root.join("real")).unwrap();
        symlink("/proc/self/fd", root.join("real/fd")).unwrap();
        symlink("real", root.join("alias")).unwrap();
        symlink("/proc/self/fd/0", root.join("stdin")).unwrap();
        symlink("stdin", root.join("chain")).unwrap();
        let link = |path: &Path, target: &str| (path.to_path_buf(), PathBuf::from(target));
        assert_eq!(kernel_links(Path::new("/proc/self/fd/0")), Some(Vec::new()));
        assert_eq!(
            kernel_links(&root.join("real/fd/1")),
            Some(vec![link(&root.join("real/fd"), "/proc/self/fd")])
        );
        assert_eq!(
            kernel_links(&root.join("alias/fd/1")),
            Some(vec![
                link(&root.join("alias"), "real"),
                link(&root.join("real/fd"), "/proc/self/fd"),
            ])
        );
        assert_eq!(
            kernel_links(&root.join("chain")),
            Some(vec![
                link(&root.join("chain"), "stdin"),
                link(&root.join("stdin"), "/proc/self/fd/0"),
            ])
        );
        assert_eq!(kernel_links(&root.join("real")), None);
    }

    /// Only a path whose own final component is a link outside `/proc` and `/sys`
    /// keeps a link target: a path reached through a directory link into them, such
    /// as `/dev/fd/0`, is the `/proc` object itself, even though that is a link too.
    #[test]
    fn only_a_final_link_outside_proc_and_sys_keeps_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir(root.join("real")).unwrap();
        symlink("/proc/self/fd", root.join("real/fd")).unwrap();
        symlink("real", root.join("alias")).unwrap();
        symlink("/proc/self/fd/0", root.join("real/stdin")).unwrap();
        let own = |path: &Path| kernel_route(path).map(|route| route.own_target);
        assert_eq!(own(Path::new("/proc/self/fd/0")), Some(None));
        assert_eq!(own(&root.join("real/fd/0")), Some(None));
        assert_eq!(own(&root.join("alias/fd/0")), Some(None));
        assert_eq!(
            own(&root.join("real/fd")),
            Some(Some(PathBuf::from("/proc/self/fd")))
        );
        assert_eq!(
            own(&root.join("alias/stdin")),
            Some(Some(PathBuf::from("/proc/self/fd/0")))
        );
        assert_eq!(own(&root.join("alias")), None);
    }
}
