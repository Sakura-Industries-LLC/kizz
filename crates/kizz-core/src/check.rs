// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache check: comparing a step's recorded observations with the file system.
//!
//! [`check`] compares every recorded observation with the current state of its
//! path, concurrently, and lists every [`Difference`]. The step is a cache hit only
//! when there is none. The rules of "Cache check":
//!
//! * A regular file or a directory whose type, permission bits, or owner differ has
//!   changed, found without hashing, whatever its content: a command may check them. The
//!   permission bits are the low twelve bits of the mode, the set-user-ID, set-group-ID,
//!   and sticky bits included.
//! * A regular file that was read has a hash. A different size is a change, found without
//!   hashing. The same size and the same metadata (modification and change times, inode,
//!   device, mode, and owner) is unchanged. The same size, mode, and owner with different
//!   times, inode, or device is hashed: an equal hash means unchanged, and the check
//!   returns the new metadata so the record can store it.
//! * When the recorded modification time is within two seconds of when the record was
//!   written, or later, metadata cannot prove the file unchanged, so it is hashed even
//!   when the metadata matches, as git does for racily clean files.
//! * A regular file that was not read has no hash: any difference in size or metadata,
//!   the owner included, is a change.
//! * A directory's mode and owner are always compared. A listed directory is listed again
//!   and compared by its entries, in the order the file system returns them: an equal
//!   listing hash is unchanged; otherwise an equal hash of the sorted entries means only
//!   the order changed, and a different one that the entries changed, naming those that
//!   appeared, disappeared, or changed type when both listings kept their entries.
//! * For a step run with `timestamps`, a regular file that was read or run, or a
//!   directory that was read, listed, or checked, whose modification or change time
//!   differs has changed, found without hashing, after its permission bits and owner and,
//!   for a file, its size. A file that was only checked or written already compares its
//!   times with the rest of its metadata. Access times are never compared, and neither
//!   are the times of a symbolic link itself: its pointee follows the rule. Without
//!   `timestamps`, the bytes decide for a file that was read, and directory times are
//!   never compared.
//! * A symbolic link is compared by its target, then by the state of what it points to.
//! * Devices, FIFOs, and sockets are compared by type.
//! * A path whose extended attributes were recorded is compared by them too, once the
//!   rest of its state matches: they are read again through the attribute query of the
//!   tracer interface (see [`record_attributes`]), and an attribute that appeared,
//!   disappeared, or changed value, of the path itself or, for a symbolic link, of what
//!   it points to, is a change.
//! * Untracked paths under `/proc` and `/sys` are never compared. Tracked ones are read
//!   and hashed again, without the cache of file states, because their metadata says
//!   nothing about their content. A symbolic link that leads into `/proc` or `/sys` is
//!   compared by its target, and its untracked pointee is not compared.
//!
//! Hashes of files that were not tracked go through the build's [`FileStates`], so a
//! file is hashed once per build.

use std::{
    collections::BTreeMap,
    fs::{self, Metadata},
    os::unix::fs::MetadataExt as _,
    path::Path,
};

use kizz_store::{
    AccessBits, Attribute, Attributes, Change, ContentHash, Difference, DirEntry, EntryKind,
    FileMeta, Listing, Observation, Owner, PathState, Times, permission_bits,
};
use kizz_trace::Tracer;
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};

use crate::{
    observe::{self, failed, file_meta, list, record_attributes, special_kind},
    states::FileStates,
};

/// How close to when a record was written a recorded modification time must be,
/// in nanoseconds, for metadata not to prove a file unchanged: two seconds.
pub const RACY_WINDOW_NANOS: i64 = 2_000_000_000;

/// The result of comparing a step's recorded observations with the file system.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Checked {
    /// Every recorded path that changed, sorted by path. The step is a cache hit
    /// only when this is empty.
    pub differences: Vec<Difference>,
    /// The recorded observations, with the new metadata of every file whose metadata
    /// changed while its content did not.
    pub observations: Vec<Observation>,
    /// Whether the record should be written again: a file's metadata changed while
    /// its content did not, or a racily clean file was hashed and found unchanged.
    pub refresh: bool,
}

impl Checked {
    /// Whether nothing changed, so the step is a cache hit.
    #[must_use]
    pub fn is_hit(&self) -> bool {
        self.differences.is_empty()
    }
}

/// How one recorded path compares with the file system.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Compared {
    /// Unchanged, proved by metadata or without needing a hash.
    Same,
    /// Unchanged, proved by hashing a racily clean file whose metadata matched.
    Verified,
    /// Unchanged content with new metadata, which this state holds.
    Refreshed(PathState),
    /// Changed.
    Changed(Change),
}

/// What every comparison of one check shares.
#[derive(Debug, Clone, Copy)]
struct Context<'a> {
    /// Recorded modification times at or after this time, in nanoseconds since the
    /// Unix epoch, cannot prove a file unchanged.
    racy_from: i64,
    /// Whether the step was run with `timestamps`.
    timestamps: bool,
    /// The build's file states and hashes.
    states: &'a FileStates,
}

/// Whether a step run with `timestamps` compares the times of a path it touched
/// with `access`: one it read, ran, listed, or checked, and not one it only wrote.
const fn uses(access: AccessBits) -> bool {
    (access.0 & !AccessBits::WRITE) != 0
}

/// The timestamp change from `old` to `new`, or `None` when they match.
fn times_change(
    old: Times,
    new: Times,
) -> Option<Change> {
    (old != new).then_some(Change::Timestamps { old, new })
}

/// Compare every observation in `observations`, recorded with the record written at
/// `recorded_at_nanos`, with the file system, concurrently, hashing through
/// `states`. With `timestamps`, the times of what the step read, ran, listed, or
/// checked are compared too. Recorded extended attributes are read again through
/// the attribute query of `tracer`.
#[must_use]
pub fn check(
    observations: &[Observation],
    recorded_at_nanos: u64,
    timestamps: bool,
    states: &FileStates,
    tracer: &dyn Tracer,
) -> Checked {
    let context = Context {
        racy_from: i64::try_from(recorded_at_nanos)
            .unwrap_or(i64::MAX)
            .saturating_sub(RACY_WINDOW_NANOS),
        timestamps,
        states,
    };
    let compared: Vec<Compared> = observations
        .par_iter()
        .map(|observation| {
            let compared = compare(
                &observation.path,
                &observation.state,
                false,
                observation.tracked,
                Context {
                    timestamps: context.timestamps && uses(observation.access),
                    ..context
                },
            );
            match (&compared, &observation.attributes) {
                (Compared::Changed(_), _) | (_, None) => compared,
                (_, Some(recorded)) => {
                    let current = record_attributes(&observation.path, tracer);
                    attributes_change(recorded, &current).map_or(compared, Compared::Changed)
                },
            }
        })
        .collect();
    let mut checked = Checked {
        observations: Vec::with_capacity(observations.len()),
        ..Checked::default()
    };
    for (observation, compared) in observations.iter().zip(compared) {
        let mut observation = observation.clone();
        match compared {
            Compared::Same => {},
            Compared::Verified => checked.refresh = true,
            Compared::Refreshed(state) => {
                observation.state = state;
                checked.refresh = true;
            },
            Compared::Changed(change) => {
                checked.differences.push(Difference {
                    path: observation.path.clone(),
                    change,
                });
            },
        }
        checked.observations.push(observation);
    }
    checked
}

/// The change from the recorded extended attributes `old` of a path to its
/// `current` ones, or `None` when they match: the attributes that appeared,
/// disappeared, or changed value, of the path itself and, for a symbolic link, of
/// what it points to, each sorted by name. Attributes that could not be read count
/// as none, so a change only in whether they could be read lists no attribute.
fn attributes_change(
    old: &Attributes,
    current: &Attributes,
) -> Option<Change> {
    if old == current {
        return None;
    }
    let mut appeared = Vec::new();
    let mut disappeared = Vec::new();
    let mut changed = Vec::new();
    let pairs = [
        (old.own.as_deref(), current.own.as_deref()),
        (old.pointee.as_deref(), current.pointee.as_deref()),
    ];
    for (old, current) in pairs {
        let hashes = |attributes: Option<&[Attribute]>| -> BTreeMap<Vec<u8>, ContentHash> {
            attributes
                .into_iter()
                .flatten()
                .map(|attribute| (attribute.name.clone(), attribute.hash))
                .collect()
        };
        let old = hashes(old);
        let current = hashes(current);
        for (name, &hash) in &current {
            let attribute = Attribute {
                name: name.clone(),
                hash,
            };
            match old.get(name) {
                None => appeared.push(attribute),
                Some(recorded) if *recorded != hash => changed.push(attribute),
                Some(_) => {},
            }
        }
        for (name, &hash) in &old {
            if !current.contains_key(name) {
                disappeared.push(Attribute {
                    name: name.clone(),
                    hash,
                });
            }
        }
    }
    for list in [&mut appeared, &mut disappeared, &mut changed] {
        list.sort();
    }
    Some(Change::Attributes {
        appeared,
        disappeared,
        changed,
    })
}

/// Compare the recorded state `recorded` of `path` with its current state,
/// following a final symbolic link when `follow` is set. `tracked` marks a `/proc`
/// or `/sys` path that a `track` pattern matched.
fn compare(
    path: &Path,
    recorded: &PathState,
    follow: bool,
    tracked: bool,
    context: Context<'_>,
) -> Compared {
    if matches!(recorded, PathState::Untracked) {
        return Compared::Same;
    }
    let metadata = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) => return compare_missing(recorded, &failed(&error)),
    };
    let file_type = metadata.file_type();
    match recorded {
        PathState::Absent => Compared::Changed(Change::Appeared),
        PathState::File { meta, hash } if file_type.is_file() => {
            compare_file(path, meta, hash.as_ref(), &metadata, tracked, context)
        },
        PathState::Directory {
            mode,
            owner,
            listing,
            times,
        } if file_type.is_dir() => {
            if let Some(change) =
                access_change(*mode, *owner, metadata.mode(), observe::owner(&metadata))
            {
                return Compared::Changed(change);
            }
            if context.timestamps
                && let Some(change) = times_change(*times, observe::times(&metadata))
            {
                return Compared::Changed(change);
            }
            let Some(listing) = listing else {
                return Compared::Same;
            };
            match list(path) {
                Ok(current) => {
                    listing_change(listing, &current).map_or(Compared::Same, Compared::Changed)
                },
                Err(_) => Compared::Changed(Change::Type),
            }
        },
        PathState::Symlink { target, pointee } if file_type.is_symlink() => {
            match fs::read_link(path) {
                Ok(current) if current == *target => {
                    match compare(path, pointee, true, tracked, context) {
                        Compared::Refreshed(state) => {
                            Compared::Refreshed(PathState::Symlink {
                                target: target.clone(),
                                pointee: Box::new(state),
                            })
                        },
                        other => other,
                    }
                },
                Ok(current) => {
                    Compared::Changed(Change::LinkTarget {
                        old: target.clone(),
                        new: current,
                    })
                },
                Err(_) => Compared::Changed(Change::Type),
            }
        },
        PathState::Special { kind }
            if !file_type.is_file()
                && !file_type.is_dir()
                && !file_type.is_symlink()
                && special_kind(file_type) == *kind =>
        {
            Compared::Same
        },
        _ => Compared::Changed(Change::Type),
    }
}

/// Compare the recorded state `recorded` with `current`, the state of a path whose
/// metadata could not be read: absent, or inaccessible.
fn compare_missing(
    recorded: &PathState,
    current: &PathState,
) -> Compared {
    match (recorded, current) {
        (PathState::Absent, PathState::Absent) => Compared::Same,
        (PathState::Absent, _) => Compared::Changed(Change::Appeared),
        (_, PathState::Absent) => Compared::Changed(Change::Disappeared),
        (PathState::Inaccessible { errno }, PathState::Inaccessible { errno: now })
            if errno == now =>
        {
            Compared::Same
        },
        _ => Compared::Changed(Change::Type),
    }
}

/// The change between a recorded mode `old_mode` and owner `old_owner` of a regular
/// file or a directory and its current mode `new_mode` and owner `new_owner`, when
/// its type is the same: different permission bits, then a different owner, or
/// `None` when both match.
fn access_change(
    old_mode: u32,
    old_owner: Owner,
    new_mode: u32,
    new_owner: Owner,
) -> Option<Change> {
    let (old, new) = (permission_bits(old_mode), permission_bits(new_mode));
    if old != new {
        return Some(Change::Permissions { old, new });
    }
    (old_owner != new_owner).then_some(Change::Owner {
        old: old_owner,
        new: new_owner,
    })
}

/// The change from the recorded listing `old` of a directory to its `current`
/// listing, or `None` when the entries and their order match: an equal listing hash
/// is unchanged; otherwise an equal sorted hash is an order change, and a different
/// one is an entries change.
fn listing_change(
    old: &Listing,
    current: &Listing,
) -> Option<Change> {
    if current.hash == old.hash {
        None
    } else if current.sorted_hash == old.sorted_hash {
        Some(Change::Order)
    } else {
        Some(entries_change(
            old.entries.as_deref(),
            current.entries.as_deref(),
        ))
    }
}

/// The entries change from the recorded entries `old` to the `current` ones: the
/// entries that appeared, that disappeared, and whose type changed, each sorted by
/// name, or none of them when either listing kept no entries.
fn entries_change(
    old: Option<&[DirEntry]>,
    current: Option<&[DirEntry]>,
) -> Change {
    let mut appeared = Vec::new();
    let mut disappeared = Vec::new();
    let mut retyped = Vec::new();
    if let (Some(old), Some(current)) = (old, current) {
        let kinds = |entries: &[DirEntry]| -> BTreeMap<Vec<u8>, EntryKind> {
            entries
                .iter()
                .map(|entry| (entry.name.clone(), entry.kind))
                .collect()
        };
        let old = kinds(old);
        let current = kinds(current);
        for (name, &kind) in &current {
            let entry = DirEntry {
                name: name.clone(),
                kind,
            };
            match old.get(name) {
                None => appeared.push(entry),
                Some(&recorded) if recorded != kind => retyped.push(entry),
                Some(_) => {},
            }
        }
        for (name, &kind) in &old {
            if !current.contains_key(name) {
                disappeared.push(DirEntry {
                    name: name.clone(),
                    kind,
                });
            }
        }
    }
    Change::Entries {
        appeared,
        disappeared,
        retyped,
    }
}

/// Compare a regular file at `path`, recorded with `meta` and, when it was read,
/// `hash`, with its current `metadata`. Its permission bits and owner come first,
/// so a refresh of its metadata never hides a change of either; with `timestamps`,
/// the times of a file that was read come next, after its size, and before any hash.
fn compare_file(
    path: &Path,
    meta: &FileMeta,
    hash: Option<&ContentHash>,
    metadata: &Metadata,
    tracked: bool,
    context: Context<'_>,
) -> Compared {
    let current = file_meta(metadata);
    if let Some(change) = access_change(meta.mode, meta.owner, current.mode, current.owner) {
        return Compared::Changed(change);
    }
    if tracked {
        if hash.is_some()
            && context.timestamps
            && let Some(change) = times_change(meta.times(), current.times())
        {
            return Compared::Changed(change);
        }
        return match context.states.hash_uncached(path) {
            Ok(now) if Some(&now) == hash => Compared::Same,
            Ok(_) => Compared::Changed(Change::Content),
            Err(_) => Compared::Changed(Change::Type),
        };
    }
    if current.size != meta.size {
        return Compared::Changed(Change::Size {
            old: meta.size,
            new: current.size,
        });
    }
    let Some(hash) = hash else {
        return if current == *meta {
            Compared::Same
        } else {
            Compared::Changed(Change::Metadata)
        };
    };
    if context.timestamps
        && let Some(change) = times_change(meta.times(), current.times())
    {
        return Compared::Changed(change);
    }
    let racy = meta.mtime_nanos >= context.racy_from;
    if current == *meta && !racy {
        return Compared::Same;
    }
    match context.states.hash(path, &current) {
        Ok(now) if now == *hash => {
            if current == *meta {
                Compared::Verified
            } else {
                Compared::Refreshed(PathState::File {
                    meta: current,
                    hash: Some(now),
                })
            }
        },
        Ok(_) => Compared::Changed(Change::Content),
        Err(_) => Compared::Changed(Change::Type),
    }
}

/// Unit tests for the comparison rules.
#[cfg(test)]
mod tests {
    use std::{
        fs::{File, FileTimes},
        io,
        os::unix::fs::{PermissionsExt as _, symlink},
        path::PathBuf,
        sync::{Mutex, PoisonError},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use kizz_store::{AccessBits, MAX_LISTED_NAMES, SpecialKind};
    use kizz_trace::{
        AccessKind, AccessKinds, Cancellation, EchoSink, ExtendedAttributes, RunRequest, RunResult,
        TraceError,
    };

    use super::*;
    use crate::observe::{access_bits, path_state};

    /// A fake tracer that runs nothing, and whose attribute query finds no
    /// attributes anywhere, for the tests that record none.
    struct NoAttributes;

    impl Tracer for NoAttributes {
        fn run(
            &self,
            _request: &RunRequest,
            _echo: &dyn EchoSink,
            _cancel: &Cancellation,
        ) -> Result<RunResult, TraceError> {
            Err(TraceError::Tracer {
                message: "the fake runs nothing".to_owned(),
                source: None,
            })
        }

        fn extended_attributes(
            &self,
            _path: &Path,
            _follow: bool,
        ) -> io::Result<ExtendedAttributes> {
            Ok(ExtendedAttributes::new())
        }
    }

    /// A fake tracer that runs nothing, and whose attribute query answers with the
    /// attributes that a test sets for each path, with and without following a
    /// final link, and fails with `NotFound` for a path that it knows nothing of.
    #[derive(Default)]
    struct FakeAttributes {
        /// The attributes of each path, by the path and whether a final link is
        /// followed.
        attributes: Mutex<BTreeMap<(PathBuf, bool), ExtendedAttributes>>,
    }

    impl FakeAttributes {
        /// Set the attributes of `path`, with or without following a final link, to
        /// `pairs` of a name and a value.
        fn set(
            &self,
            path: &Path,
            follow: bool,
            pairs: &[(&str, &str)],
        ) {
            let attributes = pairs
                .iter()
                .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
                .collect();
            self.attributes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert((path.to_path_buf(), follow), attributes);
        }
    }

    impl Tracer for FakeAttributes {
        fn run(
            &self,
            request: &RunRequest,
            echo: &dyn EchoSink,
            cancel: &Cancellation,
        ) -> Result<RunResult, TraceError> {
            NoAttributes.run(request, echo, cancel)
        }

        fn extended_attributes(
            &self,
            path: &Path,
            follow: bool,
        ) -> io::Result<ExtendedAttributes> {
            self.attributes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&(path.to_path_buf(), follow))
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    /// A temporary directory with helpers to record and check observations.
    struct Dir {
        /// The directory.
        dir: tempfile::TempDir,
        /// The cache of file states and hashes.
        states: FileStates,
    }

    impl Dir {
        /// A new empty directory.
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                states: FileStates::new(),
            }
        }

        /// The path of `name` in the directory.
        fn path(
            &self,
            name: &str,
        ) -> PathBuf {
            self.dir.path().join(name)
        }

        /// Write `content` to `name` with a modification time an hour ago.
        fn write_old(
            &self,
            name: &str,
            content: &str,
        ) {
            let path = self.path(name);
            fs::write(&path, content).unwrap();
            age(&path);
        }

        /// The observation of `name`, touched with `kind`.
        fn observe(
            &self,
            name: &str,
            kind: AccessKind,
        ) -> Observation {
            let path = self.path(name);
            Observation {
                state: path_state(&path, AccessKinds::only(kind), false, &self.states),
                path,
                access: access_bits(AccessKinds::only(kind)),
                tracked: false,
                attributes: None,
            }
        }
    }

    /// Set the modification time of `path` to an hour ago.
    fn age(path: &Path) {
        let hour_ago = SystemTime::now()
            .checked_sub(Duration::from_secs(3_600))
            .unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(hour_ago))
            .unwrap();
    }

    /// Now, in nanoseconds since the Unix epoch.
    fn now() -> u64 {
        u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap()
    }

    /// The changes `check` finds.
    fn changes(checked: &Checked) -> Vec<Change> {
        checked
            .differences
            .iter()
            .map(|difference| difference.change.clone())
            .collect()
    }

    #[test]
    fn unchanged_paths_match_without_hashing() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        dir.write_old("checked", "content");
        fs::create_dir(dir.path("listed")).unwrap();
        let observations = vec![
            dir.observe("read", AccessKind::Read),
            dir.observe("checked", AccessKind::Probe),
            dir.observe("listed", AccessKind::List),
            dir.observe("missing", AccessKind::Probe),
        ];
        assert_eq!(dir.states.files_hashed(), 1);
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert!(checked.is_hit(), "{checked:?}");
        assert!(!checked.refresh);
        assert_eq!(checked.observations, observations);
        assert_eq!(states.files_hashed(), 0);
    }

    #[test]
    fn sizes_are_compared_before_hashes() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        let observations = vec![dir.observe("read", AccessKind::Read)];
        fs::write(dir.path("read"), "longer content").unwrap();
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Size { old: 7, new: 14 }]);
        assert_eq!(states.files_hashed(), 0);

        fs::write(dir.path("read"), "CONTENT").unwrap();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Content]);
        assert_eq!(states.files_hashed(), 1);
    }

    #[test]
    fn new_metadata_with_the_same_content_is_refreshed() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        let observations = vec![dir.observe("read", AccessKind::Read)];
        fs::write(dir.path("read"), "content").unwrap();
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert!(checked.is_hit());
        assert!(checked.refresh);
        assert_eq!(states.files_hashed(), 1);
        let current = path_state(
            &dir.path("read"),
            AccessKinds::only(AccessKind::Read),
            false,
            &states,
        );
        assert_eq!(checked.observations[0].state, current);
        assert_ne!(checked.observations[0].state, observations[0].state);
    }

    #[test]
    fn racily_clean_files_are_hashed() {
        let dir = Dir::new();
        fs::write(dir.path("fresh"), "content").unwrap();
        let observations = vec![dir.observe("fresh", AccessKind::Read)];
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert!(checked.is_hit());
        assert!(checked.refresh);
        assert_eq!(checked.observations, observations);
        assert_eq!(states.files_hashed(), 1);
    }

    #[test]
    fn files_that_were_not_read_compare_by_metadata() {
        let dir = Dir::new();
        dir.write_old("checked", "content");
        fs::set_permissions(dir.path("checked"), fs::Permissions::from_mode(0o644)).unwrap();
        let observations = vec![dir.observe("checked", AccessKind::Probe)];
        set_modified(&dir.path("checked"), 7_200);
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Metadata]);
        fs::write(dir.path("checked"), "longer content").unwrap();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Size { old: 7, new: 14 }]);
        fs::set_permissions(dir.path("checked"), fs::Permissions::from_mode(0o600)).unwrap();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Permissions {
            old: 0o644,
            new: 0o600,
        }]);
        assert_eq!(states.files_hashed(), 0);
    }

    /// Set the modification time of `path`, a file or a directory, to `seconds`
    /// seconds ago.
    fn set_modified(
        path: &Path,
        seconds: u64,
    ) {
        let then = SystemTime::now()
            .checked_sub(Duration::from_secs(seconds))
            .unwrap();
        File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(then))
            .unwrap();
    }

    /// The recorded times of `observation`, a regular file, a directory, or a link to
    /// either.
    fn recorded_times(observation: &Observation) -> Times {
        match &observation.state {
            PathState::File { meta, .. } => meta.times(),
            PathState::Directory { times, .. } => *times,
            PathState::Symlink { pointee, .. } => {
                recorded_times(&Observation {
                    state: (**pointee).clone(),
                    ..observation.clone()
                })
            },
            other => panic!("no times in {other:?}"),
        }
    }

    /// The timestamp change from what `observation` recorded to the current times of
    /// `path`, following symbolic links.
    fn timestamps_change(
        observation: &Observation,
        path: &Path,
    ) -> Change {
        Change::Timestamps {
            old: recorded_times(observation),
            new: observe::times(&fs::metadata(path).unwrap()),
        }
    }

    #[test]
    fn timestamps_are_compared_without_hashing_only_with_the_option() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        dir.write_old("checked", "content");
        fs::create_dir(dir.path("listed")).unwrap();
        fs::create_dir(dir.path("opened")).unwrap();
        fs::create_dir(dir.path("written")).unwrap();
        symlink("read", dir.path("link")).unwrap();
        let observations = vec![
            dir.observe("checked", AccessKind::Probe),
            dir.observe("link", AccessKind::Read),
            dir.observe("listed", AccessKind::List),
            dir.observe("opened", AccessKind::Probe),
            dir.observe("read", AccessKind::Read),
            dir.observe("written", AccessKind::Write),
        ];
        for name in ["read", "checked", "listed", "opened", "written"] {
            set_modified(&dir.path(name), 7_200);
        }

        // Without the option, the bytes decide for a read file, which is hashed, once
        // through the link and once by its own path, and refreshed; directory times are
        // never compared; a checked file compares all of its metadata, as before.
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Metadata]);
        assert!(checked.refresh);
        assert_eq!(states.files_hashed(), 2);

        // With it, every time that changed is the change, found without hashing; a
        // directory that was only written is not compared by its times.
        let states = FileStates::new();
        let checked = check(&observations, now(), true, &states, &NoAttributes);
        assert_eq!(changes(&checked), [
            Change::Metadata,
            timestamps_change(&observations[1], &dir.path("read")),
            timestamps_change(&observations[2], &dir.path("listed")),
            timestamps_change(&observations[3], &dir.path("opened")),
            timestamps_change(&observations[4], &dir.path("read")),
        ]);
        assert!(!checked.refresh);
        assert_eq!(states.files_hashed(), 0);
    }

    #[test]
    fn with_the_option_unchanged_times_and_a_new_link_are_a_hit() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        fs::create_dir(dir.path("listed")).unwrap();
        symlink("read", dir.path("link")).unwrap();
        let observations = vec![
            dir.observe("link", AccessKind::Read),
            dir.observe("listed", AccessKind::List),
            dir.observe("read", AccessKind::Read),
        ];
        // A link made again has new times of its own, which are never compared.
        fs::remove_file(dir.path("link")).unwrap();
        symlink("read", dir.path("link")).unwrap();
        let states = FileStates::new();
        let checked = check(&observations, now(), true, &states, &NoAttributes);
        assert!(checked.is_hit(), "{checked:?}");
        assert!(!checked.refresh);
        assert_eq!(states.files_hashed(), 0);
    }

    #[test]
    fn with_the_option_a_size_or_permission_change_comes_before_the_times() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        fs::set_permissions(dir.path("read"), fs::Permissions::from_mode(0o644)).unwrap();
        let observations = vec![dir.observe("read", AccessKind::Read)];
        fs::write(dir.path("read"), "longer content").unwrap();
        let states = FileStates::new();
        let checked = check(&observations, now(), true, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Size { old: 7, new: 14 }]);
        fs::set_permissions(dir.path("read"), fs::Permissions::from_mode(0o600)).unwrap();
        let checked = check(&observations, now(), true, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Permissions {
            old: 0o644,
            new: 0o600,
        }]);
        assert_eq!(states.files_hashed(), 0);
    }

    /// The owner every observation of `observations` records, replaced by one that
    /// no file has: one more than the current user and group numbers.
    fn other_owner(observations: &mut [Observation]) -> Owner {
        let mut replaced = None;
        for observation in observations {
            let owner = match &mut observation.state {
                PathState::File { meta, .. } => &mut meta.owner,
                PathState::Directory { owner, .. } => owner,
                other => panic!("no owner in {other:?}"),
            };
            *owner = Owner {
                uid: owner.uid.wrapping_add(1),
                gid: owner.gid.wrapping_add(1),
            };
            replaced = Some(*owner);
        }
        replaced.unwrap()
    }

    #[test]
    fn permission_changes_are_found_without_hashing_whatever_the_content() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        fs::set_permissions(dir.path("read"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::create_dir(dir.path("listed")).unwrap();
        fs::set_permissions(dir.path("listed"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(dir.path("opened")).unwrap();
        fs::set_permissions(dir.path("opened"), fs::Permissions::from_mode(0o755)).unwrap();
        let observations = vec![
            dir.observe("listed", AccessKind::List),
            dir.observe("opened", AccessKind::Probe),
            dir.observe("read", AccessKind::Read),
        ];
        for name in ["read", "listed", "opened"] {
            fs::set_permissions(dir.path(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [
            Change::Permissions {
                old: 0o755,
                new: 0o700,
            },
            Change::Permissions {
                old: 0o755,
                new: 0o700,
            },
            Change::Permissions {
                old: 0o644,
                new: 0o700,
            },
        ]);
        assert!(!checked.refresh);
        assert_eq!(states.files_hashed(), 0);
    }

    #[test]
    fn a_file_whose_times_and_mode_changed_reports_the_mode_and_is_not_refreshed() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        fs::set_permissions(dir.path("read"), fs::Permissions::from_mode(0o644)).unwrap();
        let observations = vec![dir.observe("read", AccessKind::Read)];
        set_modified(&dir.path("read"), 7_200);
        fs::set_permissions(dir.path("read"), fs::Permissions::from_mode(0o600)).unwrap();
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        assert_eq!(changes(&checked), [Change::Permissions {
            old: 0o644,
            new: 0o600,
        }]);
        assert!(!checked.refresh);
        assert_eq!(checked.observations, observations);
        assert_eq!(states.files_hashed(), 0);
    }

    #[test]
    fn a_recorded_owner_that_differs_is_a_change_found_without_hashing() {
        let dir = Dir::new();
        dir.write_old("read", "content");
        dir.write_old("checked", "content");
        fs::create_dir(dir.path("listed")).unwrap();
        fs::create_dir(dir.path("opened")).unwrap();
        let mut observations = vec![
            dir.observe("checked", AccessKind::Probe),
            dir.observe("listed", AccessKind::List),
            dir.observe("opened", AccessKind::Probe),
            dir.observe("read", AccessKind::Read),
        ];
        let current = fs::metadata(dir.path("read")).unwrap();
        let current = Owner {
            uid: current.uid(),
            gid: current.gid(),
        };
        let recorded = other_owner(&mut observations);
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &NoAttributes);
        let owner = Change::Owner {
            old: recorded,
            new: current,
        };
        assert_eq!(changes(&checked), [
            owner.clone(),
            owner.clone(),
            owner.clone(),
            owner
        ]);
        assert_eq!(states.files_hashed(), 0);
    }

    #[test]
    fn special_bits_count_as_permission_bits_and_the_type_does_not() {
        let owner = Owner { uid: 1, gid: 2 };
        for special in [0o4000, 0o2000, 0o1000] {
            assert_eq!(
                access_change(0o100_755, owner, 0o100_755 | special, owner),
                Some(Change::Permissions {
                    old: 0o755,
                    new: 0o755 | special,
                }),
                "{special:o}"
            );
            assert_eq!(
                access_change(0o040_755 | special, owner, 0o040_755, owner),
                Some(Change::Permissions {
                    old: 0o755 | special,
                    new: 0o755,
                }),
                "{special:o}"
            );
        }
        // The type bits are compared as the type, never as permissions.
        assert_eq!(access_change(0o100_644, owner, 0o040_644, owner), None);
        assert_eq!(access_change(0o100_644, owner, 0o100_644, owner), None);
        // A permission change is reported before an owner change.
        let root = Owner { uid: 0, gid: 0 };
        assert_eq!(
            access_change(0o100_644, owner, 0o100_600, root),
            Some(Change::Permissions {
                old: 0o644,
                new: 0o600,
            })
        );
        assert_eq!(
            access_change(0o100_644, owner, 0o100_644, root),
            Some(Change::Owner {
                old: owner,
                new: root,
            })
        );
    }

    #[test]
    fn appearing_disappearing_and_retyped_paths_change() {
        let dir = Dir::new();
        dir.write_old("gone", "content");
        dir.write_old("retyped", "content");
        fs::create_dir(dir.path("listed")).unwrap();
        symlink("gone", dir.path("link")).unwrap();
        let observations = vec![
            dir.observe("appears", AccessKind::Probe),
            dir.observe("gone", AccessKind::Read),
            dir.observe("link", AccessKind::Read),
            dir.observe("listed", AccessKind::List),
            dir.observe("retyped", AccessKind::Probe),
        ];
        fs::write(dir.path("appears"), "").unwrap();
        fs::write(dir.path("listed/entry"), "").unwrap();
        fs::remove_file(dir.path("gone")).unwrap();
        fs::remove_file(dir.path("retyped")).unwrap();
        fs::create_dir(dir.path("retyped")).unwrap();
        let checked = check(
            &observations,
            now(),
            false,
            &FileStates::new(),
            &NoAttributes,
        );
        assert_eq!(changes(&checked), [
            Change::Appeared,
            Change::Disappeared,
            Change::Disappeared,
            Change::Entries {
                appeared: vec![DirEntry {
                    name: b"entry".to_vec(),
                    kind: EntryKind::File,
                }],
                disappeared: Vec::new(),
                retyped: Vec::new(),
            },
            Change::Type,
        ]);

        fs::remove_file(dir.path("link")).unwrap();
        symlink("appears", dir.path("link")).unwrap();
        let checked = check(
            &observations[2..3],
            now(),
            false,
            &FileStates::new(),
            &NoAttributes,
        );
        assert_eq!(changes(&checked), [Change::LinkTarget {
            old: PathBuf::from("gone"),
            new: PathBuf::from("appears"),
        }]);
    }

    /// The recorded listing of `observation`, a listed directory.
    fn recorded_listing(observation: &Observation) -> &Listing {
        match &observation.state {
            PathState::Directory {
                listing: Some(listing),
                ..
            } => listing,
            other => panic!("no listing in {other:?}"),
        }
    }

    /// `observation`, a listed directory, recorded with `listing` instead.
    fn with_listing(
        observation: &Observation,
        listing: Listing,
    ) -> Observation {
        let mut observation = observation.clone();
        if let PathState::Directory {
            listing: recorded, ..
        } = &mut observation.state
        {
            *recorded = Some(listing);
        }
        observation
    }

    /// A regular-file entry named `name`.
    fn file_entry(name: &str) -> DirEntry {
        DirEntry {
            name: name.as_bytes().to_vec(),
            kind: EntryKind::File,
        }
    }

    #[test]
    fn an_order_change_is_told_apart_from_an_entries_change() {
        let dir = Dir::new();
        fs::create_dir(dir.path("listed")).unwrap();
        for name in ["a", "b", "c"] {
            fs::write(dir.path(&format!("listed/{name}")), "").unwrap();
        }
        let observed = dir.observe("listed", AccessKind::List);
        let current = recorded_listing(&observed).entries.clone().unwrap();
        let check_with = |entries: Vec<DirEntry>, keep: bool| {
            let mut listing = observe::listing(entries);
            if !keep {
                listing.entries = None;
            }
            changes(&check(
                &[with_listing(&observed, listing)],
                now(),
                false,
                &FileStates::new(),
                &NoAttributes,
            ))
        };
        assert_eq!(check_with(current.clone(), true), []);

        // The current entries recorded in reverse order: only the order changed.
        let reversed: Vec<DirEntry> = current.iter().rev().cloned().collect();
        assert_eq!(check_with(reversed, true), [Change::Order]);

        // A recorded listing that lacks `b`, and has a `gone` directory and a `c`
        // directory, names what appeared, disappeared, and changed type.
        let mut other: Vec<DirEntry> = current
            .iter()
            .filter(|entry| entry.name != b"b")
            .cloned()
            .collect();
        for entry in &mut other {
            if entry.name == b"c" {
                entry.kind = EntryKind::Directory;
            }
        }
        other.push(DirEntry {
            name: b"gone".to_vec(),
            kind: EntryKind::Directory,
        });
        assert_eq!(check_with(other.clone(), true), [Change::Entries {
            appeared: vec![file_entry("b")],
            disappeared: vec![DirEntry {
                name: b"gone".to_vec(),
                kind: EntryKind::Directory,
            }],
            retyped: vec![file_entry("c")],
        }]);

        // Without recorded entries, an entries change names none.
        assert_eq!(check_with(other, false), [Change::Entries {
            appeared: Vec::new(),
            disappeared: Vec::new(),
            retyped: Vec::new(),
        }]);
    }

    #[test]
    fn above_the_limit_a_matching_sorted_hash_is_an_order_change() {
        let dir = Dir::new();
        fs::create_dir(dir.path("listed")).unwrap();
        for index in 0..=MAX_LISTED_NAMES {
            fs::write(dir.path(&format!("listed/{index}")), "").unwrap();
        }
        let observed = dir.observe("listed", AccessKind::List);
        let listing = recorded_listing(&observed).clone();
        assert_eq!(listing.entries, None);
        let check_with = |listing: Listing| {
            changes(&check(
                &[with_listing(&observed, listing)],
                now(),
                false,
                &FileStates::new(),
                &NoAttributes,
            ))
        };
        assert_eq!(check_with(listing.clone()), []);

        // An ordered hash that differs with a sorted hash that matches.
        let reordered = Listing {
            hash: ContentHash([0; 32]),
            ..listing.clone()
        };
        assert_eq!(check_with(reordered), [Change::Order]);

        // Both hashes differ: the entries changed, and no names are known.
        let different = Listing {
            hash: ContentHash([0; 32]),
            sorted_hash: ContentHash([0; 32]),
            entries: None,
        };
        assert_eq!(check_with(different), [Change::Entries {
            appeared: Vec::new(),
            disappeared: Vec::new(),
            retyped: Vec::new(),
        }]);
    }

    #[test]
    fn untracked_kernel_paths_are_never_compared() {
        let observation = Observation {
            path: PathBuf::from("/proc/kizz-test-never-there"),
            access: AccessBits(AccessBits::READ),
            tracked: false,
            state: PathState::Untracked,
            attributes: None,
        };
        let special = Observation {
            path: PathBuf::from("/dev/null"),
            access: AccessBits(AccessBits::WRITE),
            tracked: false,
            state: PathState::Special {
                kind: SpecialKind::CharDevice,
            },
            attributes: None,
        };
        let checked = check(
            &[observation, special],
            now(),
            false,
            &FileStates::new(),
            &NoAttributes,
        );
        assert!(checked.is_hit(), "{checked:?}");
    }

    /// The observation of `name`, whose extended attributes the step read, with
    /// its attributes recorded through `query`.
    fn observe_attributes(
        dir: &Dir,
        name: &str,
        query: &FakeAttributes,
    ) -> Observation {
        let mut observation = dir.observe(name, AccessKind::Xattr);
        observation.attributes = Some(record_attributes(&observation.path, query));
        observation
    }

    /// The recorded attribute `name` with the hash of `value`.
    fn hashed(
        name: &str,
        value: &str,
    ) -> Attribute {
        Attribute {
            name: name.as_bytes().to_vec(),
            hash: ContentHash(*blake3::hash(value.as_bytes()).as_bytes()),
        }
    }

    /// A file, a directory, and a symbolic link whose attributes were read record
    /// them with their state: each name with a hash of its value, sorted by name,
    /// and for the link also those of what it points to; a missing path records
    /// none it could read, and a file system without attributes records none.
    #[test]
    fn extended_attributes_are_recorded_with_the_state() {
        let dir = Dir::new();
        dir.write_old("file", "content");
        fs::create_dir(dir.path("dir")).unwrap();
        dir.write_old("target", "pointee");
        symlink("target", dir.path("link")).unwrap();
        let query = FakeAttributes::default();
        query.set(&dir.path("file"), false, &[
            ("user.b", "2"),
            ("user.a", "1"),
        ]);
        query.set(&dir.path("link"), false, &[]);
        query.set(&dir.path("link"), true, &[("user.origin", "x")]);

        let file = observe_attributes(&dir, "file", &query);
        assert!(file.access.contains(AccessBits::XATTR));
        assert_eq!(
            file.attributes,
            Some(Attributes {
                own: Some(vec![hashed("user.a", "1"), hashed("user.b", "2")]),
                pointee: None,
            })
        );
        assert!(matches!(file.state, PathState::File { hash: None, .. }));
        let link = observe_attributes(&dir, "link", &query);
        assert_eq!(
            link.attributes,
            Some(Attributes {
                own: Some(Vec::new()),
                pointee: Some(vec![hashed("user.origin", "x")]),
            })
        );
        let missing = observe_attributes(&dir, "missing", &query);
        assert_eq!(
            missing.attributes,
            Some(Attributes {
                own: None,
                pointee: None,
            })
        );
        assert_eq!(
            record_attributes(&dir.path("dir"), &NoAttributes),
            Attributes {
                own: Some(Vec::new()),
                pointee: None,
            }
        );
    }

    /// Recorded attributes that did not change are a hit; an attribute that
    /// appeared on a file, one that disappeared from a directory, and one whose
    /// value changed on what a link points to are changes that name them, and
    /// attributes that can no longer be read count as none, so they disappeared.
    #[test]
    fn extended_attributes_that_appear_disappear_or_change_are_changes() {
        let dir = Dir::new();
        dir.write_old("file", "content");
        fs::create_dir(dir.path("dir")).unwrap();
        dir.write_old("target", "pointee");
        symlink("target", dir.path("link")).unwrap();
        let query = FakeAttributes::default();
        query.set(&dir.path("file"), false, &[("user.a", "1")]);
        query.set(&dir.path("dir"), false, &[
            ("user.kept", "k"),
            ("user.gone", "g"),
        ]);
        query.set(&dir.path("link"), false, &[]);
        query.set(&dir.path("link"), true, &[("user.origin", "x")]);
        let observations: Vec<Observation> = ["dir", "file", "link"]
            .into_iter()
            .map(|name| observe_attributes(&dir, name, &query))
            .collect();
        let states = FileStates::new();
        let checked = check(&observations, now(), false, &states, &query);
        assert!(checked.is_hit(), "{checked:?}");
        assert!(!checked.refresh);

        query.set(&dir.path("file"), false, &[
            ("user.a", "1"),
            ("user.new", "n"),
        ]);
        query.set(&dir.path("dir"), false, &[("user.kept", "k")]);
        query.set(&dir.path("link"), true, &[("user.origin", "y")]);
        let checked = check(&observations, now(), false, &states, &query);
        assert_eq!(changes(&checked), [
            Change::Attributes {
                appeared: Vec::new(),
                disappeared: vec![hashed("user.gone", "g")],
                changed: Vec::new(),
            },
            Change::Attributes {
                appeared: vec![hashed("user.new", "n")],
                disappeared: Vec::new(),
                changed: Vec::new(),
            },
            Change::Attributes {
                appeared: Vec::new(),
                disappeared: Vec::new(),
                changed: vec![hashed("user.origin", "y")],
            },
        ]);
        let shown: Vec<String> = changes(&checked).iter().map(ToString::to_string).collect();
        assert_eq!(shown, [
            "extended attributes changed: `user.gone` disappeared",
            "extended attributes changed: `user.new` appeared",
            "extended attributes changed: `user.origin` changed",
        ]);

        // Attributes that the query can no longer read count as none, and an
        // observation without recorded attributes never queries.
        let unreadable = FakeAttributes::default();
        let checked = check(&observations[1..2], now(), false, &states, &unreadable);
        assert_eq!(changes(&checked), [Change::Attributes {
            appeared: Vec::new(),
            disappeared: vec![hashed("user.a", "1")],
            changed: Vec::new(),
        }]);
        let plain = dir.observe("file", AccessKind::Read);
        let checked = check(&[plain], now(), false, &states, &unreadable);
        assert!(checked.is_hit(), "{checked:?}");
    }

    /// A change of the rest of the state is found before the attributes, which are
    /// then not compared.
    #[test]
    fn state_changes_come_before_attribute_changes() {
        let dir = Dir::new();
        dir.write_old("file", "content");
        fs::set_permissions(dir.path("file"), fs::Permissions::from_mode(0o644)).unwrap();
        let query = FakeAttributes::default();
        query.set(&dir.path("file"), false, &[("user.a", "1")]);
        let observations = vec![observe_attributes(&dir, "file", &query)];
        fs::set_permissions(dir.path("file"), fs::Permissions::from_mode(0o600)).unwrap();
        query.set(&dir.path("file"), false, &[("user.a", "2")]);
        let checked = check(&observations, now(), false, &FileStates::new(), &query);
        assert_eq!(changes(&checked), [Change::Permissions {
            old: 0o644,
            new: 0o600,
        }]);
    }
}
