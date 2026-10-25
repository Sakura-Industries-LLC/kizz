// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache check: comparing a step's recorded observations with the file system.
//!
//! [`check`] compares every recorded observation with the current state of its
//! path, concurrently, and lists every [`Difference`]. The step is a cache hit only
//! when there is none. The rules of "Cache check":
//!
//! * A regular file that was read has a hash. A different size is a change, found without
//!   hashing. The same size and the same metadata (modification and change times, inode,
//!   device, and mode) is unchanged. The same size with different metadata is hashed: an
//!   equal hash means unchanged, and the check returns the new metadata so the record can
//!   store it.
//! * When the recorded modification time is within two seconds of when the record was
//!   written, or later, metadata cannot prove the file unchanged, so it is hashed even
//!   when the metadata matches, as git does for racily clean files.
//! * A regular file that was not read has no hash: any difference in size or metadata is
//!   a change.
//! * A listed directory is listed again and compared by its listing hash; one that was
//!   only opened or checked is compared by existence and type.
//! * A symbolic link is compared by its target, then by the state of what it points to.
//! * Devices, FIFOs, and sockets are compared by type.
//! * Untracked paths under `/proc` and `/sys` are never compared. Tracked ones are read
//!   and hashed again, without the cache of file states, because their metadata says
//!   nothing about their content. A symbolic link that leads into `/proc` or `/sys` is
//!   compared by its target, and its untracked pointee is not compared.
//!
//! Hashes of files that were not tracked go through the build's [`FileStates`], so a
//! file is hashed once per build.

use std::{
    fs::{self, Metadata},
    path::Path,
};

use kizz_store::{Change, ContentHash, Difference, FileMeta, Observation, PathState};
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};

use crate::{
    observe::{failed, file_meta, list, special_kind},
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
    /// The build's file states and hashes.
    states: &'a FileStates,
}

/// Compare every observation in `observations`, recorded with the record written at
/// `recorded_at_nanos`, with the file system, concurrently, hashing through
/// `states`.
#[must_use]
pub fn check(
    observations: &[Observation],
    recorded_at_nanos: u64,
    states: &FileStates,
) -> Checked {
    let context = Context {
        racy_from: i64::try_from(recorded_at_nanos)
            .unwrap_or(i64::MAX)
            .saturating_sub(RACY_WINDOW_NANOS),
        states,
    };
    let compared: Vec<Compared> = observations
        .par_iter()
        .map(|observation| {
            compare(
                &observation.path,
                &observation.state,
                false,
                observation.tracked,
                context,
            )
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
        PathState::Directory { listing } if file_type.is_dir() => {
            let Some(listing) = listing else {
                return Compared::Same;
            };
            match list(path) {
                Ok(current) if current.hash == listing.hash => Compared::Same,
                Ok(_) => Compared::Changed(Change::Entries),
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

/// Compare a regular file at `path`, recorded with `meta` and, when it was read,
/// `hash`, with its current `metadata`.
fn compare_file(
    path: &Path,
    meta: &FileMeta,
    hash: Option<&ContentHash>,
    metadata: &Metadata,
    tracked: bool,
    context: Context<'_>,
) -> Compared {
    let current = file_meta(metadata);
    if tracked {
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
        os::unix::fs::{PermissionsExt as _, symlink},
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use kizz_store::{AccessBits, SpecialKind};
    use kizz_trace::{AccessKind, AccessKinds};

    use super::*;
    use crate::observe::path_state;

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
                access: AccessBits::default(),
                tracked: false,
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
        let checked = check(&observations, now(), &states);
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
        let checked = check(&observations, now(), &states);
        assert_eq!(changes(&checked), [Change::Size { old: 7, new: 14 }]);
        assert_eq!(states.files_hashed(), 0);

        fs::write(dir.path("read"), "CONTENT").unwrap();
        let checked = check(&observations, now(), &states);
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
        let checked = check(&observations, now(), &states);
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
        let checked = check(&observations, now(), &states);
        assert!(checked.is_hit());
        assert!(checked.refresh);
        assert_eq!(checked.observations, observations);
        assert_eq!(states.files_hashed(), 1);
    }

    #[test]
    fn files_that_were_not_read_compare_by_metadata() {
        let dir = Dir::new();
        dir.write_old("checked", "content");
        let observations = vec![dir.observe("checked", AccessKind::Probe)];
        fs::set_permissions(dir.path("checked"), fs::Permissions::from_mode(0o600)).unwrap();
        let states = FileStates::new();
        let checked = check(&observations, now(), &states);
        assert_eq!(changes(&checked), [Change::Metadata]);
        fs::write(dir.path("checked"), "longer content").unwrap();
        let checked = check(&observations, now(), &states);
        assert_eq!(changes(&checked), [Change::Size { old: 7, new: 14 }]);
        assert_eq!(states.files_hashed(), 0);
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
        let checked = check(&observations, now(), &FileStates::new());
        assert_eq!(changes(&checked), [
            Change::Appeared,
            Change::Disappeared,
            Change::Disappeared,
            Change::Entries,
            Change::Type,
        ]);

        fs::remove_file(dir.path("link")).unwrap();
        symlink("appears", dir.path("link")).unwrap();
        let checked = check(&observations[2..3], now(), &FileStates::new());
        assert_eq!(changes(&checked), [Change::LinkTarget {
            old: PathBuf::from("gone"),
            new: PathBuf::from("appears"),
        }]);
    }

    #[test]
    fn untracked_kernel_paths_are_never_compared() {
        let observation = Observation {
            path: PathBuf::from("/proc/kizz-test-never-there"),
            access: AccessBits(AccessBits::READ),
            tracked: false,
            state: PathState::Untracked,
        };
        let special = Observation {
            path: PathBuf::from("/dev/null"),
            access: AccessBits(AccessBits::WRITE),
            tracked: false,
            state: PathState::Special {
                kind: SpecialKind::CharDevice,
            },
        };
        let checked = check(&[observation, special], now(), &FileStates::new());
        assert!(checked.is_hit(), "{checked:?}");
    }
}
