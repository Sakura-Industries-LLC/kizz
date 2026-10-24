// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The typed records of the cache database and the layout of their keys.
//!
//! Every record belongs to a [`Family`], and every key in the database starts with
//! its family's prefix, so a family, or a part of one, is a prefix scan. The rest of
//! the key is the record's [`RecordKey`]:
//!
//! | Family | Prefix | Rest of the key | Record |
//! | --- | --- | --- | --- |
//! | schema version | `meta/` | `schema-version` | [`SchemaVersion`] |
//! | step records | `step/` | the 32 bytes of the [`StepKey`] | [`StepRecord`](crate::step::StepRecord) |
//! | step positions | `position/` | the [`TaskId`], then the position | [`StepPosition`] |
//! | task records | `task/` | the [`TaskId`] | [`TaskRecord`] |
//! | last writers | `writer/` | the absolute path, as a [`PathKey`] | [`LastWriter`] |
//! | learned order | `order/` | the [`TaskId`] | [`LearnedOrder`] |
//!
//! A [`TaskId`] is the bytes of the absolute path of the task's `kizz.md`, a zero
//! byte, which no path contains, and the task's index in the file as four big-endian
//! bytes. Positions are four big-endian bytes too, so keys sort by path, then index,
//! then position. Because keys start with absolute paths, every entry for the tasks
//! and paths under a directory is the prefix scan of [`path_prefix`].
//!
//! Records are CBOR, encoded with `minicbor` by the rules of "Step keys": definite
//! lengths, fields in a fixed order (struct fields are arrays, in index order), and
//! sets and maps sorted by key, so the same record always has the same bytes.
//!
//! Step records hold the full step specification and the observations; their
//! contents are in [`step`](crate::step). Later phases extend the other record
//! contents, and a change of encoding raises [`SCHEMA_VERSION`](crate::SCHEMA_VERSION).

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fmt,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use minicbor::{Decode, Decoder, Encode, Encoder, bytes::ByteArray, decode, encode};

/// The record families of the cache database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// Facts about the database itself: the schema version.
    Meta,
    /// Step records, by step key.
    Step,
    /// Step positions: the step key each task used last time at each position.
    Position,
    /// Task records: the task program hash and a summary of the last run.
    Task,
    /// Last writers: for each path, the step that wrote it most recently.
    Writer,
    /// Learned order: for each task, the tasks whose outputs it consumed.
    Order,
}

impl Family {
    /// Every family.
    pub const ALL: [Self; 6] = [
        Self::Meta,
        Self::Step,
        Self::Position,
        Self::Task,
        Self::Writer,
        Self::Order,
    ];

    /// The prefix of every key of this family. No prefix is a prefix of another.
    #[must_use]
    pub const fn prefix(self) -> &'static [u8] {
        match self {
            Self::Meta => b"meta/",
            Self::Step => b"step/",
            Self::Position => b"position/",
            Self::Task => b"task/",
            Self::Writer => b"writer/",
            Self::Order => b"order/",
        }
    }

    /// The name of the family in messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Meta => "schema version",
            Self::Step => "step record",
            Self::Position => "step position",
            Self::Task => "task record",
            Self::Writer => "last writer",
            Self::Order => "learned order",
        }
    }
}

impl fmt::Display for Family {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The part of a database key after the family prefix.
pub trait RecordKey: Sized {
    /// Appends the key's bytes to `out`.
    fn encode_key(
        &self,
        out: &mut Vec<u8>,
    );

    /// The key from its bytes, or `None` when they are not a key of this type.
    fn decode_key(bytes: &[u8]) -> Option<Self>;
}

/// A typed record stored in the cache database.
pub trait Record: Encode<()> + for<'b> Decode<'b, ()> {
    /// The key of a record of this type.
    type Key: RecordKey;

    /// The family of this record type.
    const FAMILY: Family;
}

/// The full database key of the record of type `R` at `key`.
#[must_use]
pub fn database_key<R: Record>(key: &R::Key) -> Vec<u8> {
    let mut out = R::FAMILY.prefix().to_vec();
    key.encode_key(&mut out);
    out
}

/// The key prefix that selects every entry for the paths and tasks under `dir`, for
/// the families keyed by absolute paths: step positions, task records, last writers,
/// and learned order. The entry for `dir` itself is not under it.
#[must_use]
pub fn path_prefix(dir: &Path) -> Vec<u8> {
    let mut out = dir.as_os_str().as_bytes().to_vec();
    if out.last() != Some(&b'/') {
        out.push(b'/');
    }
    out
}

/// The BLAKE3 hash that identifies a step, the key of its step record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepKey(pub [u8; 32]);

impl RecordKey for StepKey {
    fn encode_key(
        &self,
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(&self.0);
    }

    fn decode_key(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

impl<C> Encode<C> for StepKey {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        e.bytes(&self.0)?.ok()
    }
}

impl<'b, C> Decode<'b, C> for StepKey {
    fn decode(
        d: &mut Decoder<'b>,
        ctx: &mut C,
    ) -> Result<Self, decode::Error> {
        ByteArray::<32>::decode(d, ctx).map(|bytes| Self(bytes.into()))
    }
}

/// The identity of a task: the absolute path of its `kizz.md` and its index among the
/// task blocks of that file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId {
    /// The absolute path of the task's `kizz.md`.
    pub file: PathBuf,
    /// The index of the task block in the file, counting from zero.
    pub index: u32,
}

impl TaskId {
    /// The key prefix of every step position of this task.
    #[must_use]
    pub fn key_prefix(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_key(&mut out);
        out
    }
}

impl RecordKey for TaskId {
    fn encode_key(
        &self,
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(self.file.as_os_str().as_bytes());
        out.push(0);
        out.extend_from_slice(&self.index.to_be_bytes());
    }

    fn decode_key(bytes: &[u8]) -> Option<Self> {
        let (path, index) = bytes.split_last_chunk::<4>()?;
        let (separator, path) = path.split_last()?;
        if *separator != 0 || path.contains(&0) {
            return None;
        }
        Some(Self {
            file: PathBuf::from(OsStr::from_bytes(path)),
            index: u32::from_be_bytes(*index),
        })
    }
}

impl<C> Encode<C> for TaskId {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        e.array(2)?
            .bytes(self.file.as_os_str().as_bytes())?
            .u32(self.index)?
            .ok()
    }
}

impl<'b, C> Decode<'b, C> for TaskId {
    fn decode(
        d: &mut Decoder<'b>,
        _ctx: &mut C,
    ) -> Result<Self, decode::Error> {
        if d.array()? != Some(2) {
            return Err(decode::Error::message("a task ID is an array of two items"));
        }
        let file = PathBuf::from(OsStr::from_bytes(d.bytes()?));
        let index = d.u32()?;
        Ok(Self { file, index })
    }
}

/// The key of a step position: a task and the position of a step in its run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositionKey {
    /// The task.
    pub task: TaskId,
    /// The position of the step in the task's run, counting from zero.
    pub position: u32,
}

impl RecordKey for PositionKey {
    fn encode_key(
        &self,
        out: &mut Vec<u8>,
    ) {
        self.task.encode_key(out);
        out.extend_from_slice(&self.position.to_be_bytes());
    }

    fn decode_key(bytes: &[u8]) -> Option<Self> {
        let (task, position) = bytes.split_last_chunk::<4>()?;
        Some(Self {
            task: TaskId::decode_key(task)?,
            position: u32::from_be_bytes(*position),
        })
    }
}

/// The key of a last-writer entry: an absolute path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathKey(pub PathBuf);

impl RecordKey for PathKey {
    fn encode_key(
        &self,
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(self.0.as_os_str().as_bytes());
    }

    fn decode_key(bytes: &[u8]) -> Option<Self> {
        Some(Self(PathBuf::from(OsStr::from_bytes(bytes))))
    }
}

/// The key of the schema version, the only record of its family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaKey;

impl SchemaKey {
    /// The bytes of the key after the family prefix.
    const BYTES: &'static [u8] = b"schema-version";
}

impl RecordKey for SchemaKey {
    fn encode_key(
        &self,
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(Self::BYTES);
    }

    fn decode_key(bytes: &[u8]) -> Option<Self> {
        (bytes == Self::BYTES).then_some(Self)
    }
}

/// The version of the database's schema, written when the database is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct SchemaVersion {
    /// The version.
    #[n(0)]
    pub version: u32,
}

impl Record for SchemaVersion {
    type Key = SchemaKey;

    const FAMILY: Family = Family::Meta;
}

/// The step key a task used last time at one step position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct StepPosition {
    /// The step key.
    #[n(0)]
    pub step: StepKey,
}

impl Record for StepPosition {
    type Key = PositionKey;

    const FAMILY: Family = Family::Position;
}

/// What is recorded about a task: its program hash and a summary of its last run.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct TaskRecord {
    /// The hash of the task program: its block and every module it loads.
    #[cbor(n(0), with = "minicbor::bytes")]
    pub program_hash: [u8; 32],
    /// Whether the last run of the task succeeded.
    #[n(1)]
    pub succeeded: bool,
}

impl Record for TaskRecord {
    type Key = TaskId;

    const FAMILY: Family = Family::Task;
}

/// The step that most recently wrote a path.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct LastWriter {
    /// The task of the step.
    #[n(0)]
    pub task: TaskId,
    /// The step key.
    #[n(1)]
    pub step: StepKey,
}

impl Record for LastWriter {
    type Key = PathKey;

    const FAMILY: Family = Family::Writer;
}

/// The tasks whose outputs a task consumed, in sorted order.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct LearnedOrder {
    /// The producers.
    #[n(0)]
    pub producers: BTreeSet<TaskId>,
}

impl Record for LearnedOrder {
    type Key = TaskId;

    const FAMILY: Family = Family::Order;
}

/// Unit tests for the key layout and the record encoding.
#[cfg(test)]
mod tests {
    use super::*;

    /// A task in `/work/<dir>/kizz.md`.
    fn task(
        dir: &str,
        index: u32,
    ) -> TaskId {
        TaskId {
            file: PathBuf::from(format!("/work/{dir}/kizz.md")),
            index,
        }
    }

    #[test]
    fn no_family_prefix_is_a_prefix_of_another() {
        for one in Family::ALL {
            for other in Family::ALL {
                if one != other {
                    assert!(
                        !other.prefix().starts_with(one.prefix()),
                        "{one} and {other}"
                    );
                }
            }
        }
    }

    #[test]
    fn keys_round_trip() {
        let step = StepKey([7; 32]);
        let mut bytes = Vec::new();
        step.encode_key(&mut bytes);
        assert_eq!(StepKey::decode_key(&bytes), Some(step));
        assert_eq!(StepKey::decode_key(&bytes[1..]), None);

        let id = task("a b", 3);
        assert_eq!(TaskId::decode_key(&id.key_prefix()), Some(id.clone()));
        assert_eq!(TaskId::decode_key(b"/x"), None);

        let position = PositionKey {
            task: id,
            position: 9,
        };
        let mut bytes = Vec::new();
        position.encode_key(&mut bytes);
        assert_eq!(PositionKey::decode_key(&bytes), Some(position));

        let path = PathKey(PathBuf::from("/work/out.bin"));
        let mut bytes = Vec::new();
        path.encode_key(&mut bytes);
        assert_eq!(PathKey::decode_key(&bytes), Some(path));

        let mut bytes = Vec::new();
        SchemaKey.encode_key(&mut bytes);
        assert_eq!(SchemaKey::decode_key(&bytes), Some(SchemaKey));
        assert_eq!(SchemaKey::decode_key(b"other"), None);
    }

    #[test]
    fn database_keys_start_with_the_family_prefix() {
        assert_eq!(
            database_key::<SchemaVersion>(&SchemaKey),
            b"meta/schema-version"
        );
        let key = database_key::<TaskRecord>(&task("a", 1));
        assert_eq!(key, b"task//work/a/kizz.md\0\0\0\0\x01");
    }

    #[test]
    fn task_keys_sort_by_path_then_index_then_position() {
        let key = |dir: &str, index: u32, position: u32| {
            database_key::<StepPosition>(&PositionKey {
                task: task(dir, index),
                position,
            })
        };
        let mut keys = vec![
            key("b", 0, 0),
            key("a", 1, 0),
            key("a", 0, 256),
            key("a", 0, 1),
            key("a", 256, 0),
        ];
        keys.sort();
        assert_eq!(keys, vec![
            key("a", 0, 1),
            key("a", 0, 256),
            key("a", 1, 0),
            key("a", 256, 0),
            key("b", 0, 0),
        ]);
    }

    #[test]
    fn path_prefix_selects_only_paths_under_the_directory() {
        let prefix = path_prefix(Path::new("/work/a"));
        assert_eq!(prefix, b"/work/a/");
        assert_eq!(path_prefix(Path::new("/work/a/")), b"/work/a/");
        assert_eq!(path_prefix(Path::new("/")), b"/");
        assert!(task("a", 0).key_prefix().starts_with(&prefix));
        assert!(!task("ab", 0).key_prefix().starts_with(&prefix));
    }

    /// Encodes `record` twice, checks that both encodings are equal and decode back to
    /// it, and returns them.
    fn encode_stably<R: Record + PartialEq + fmt::Debug>(record: &R) -> Vec<u8> {
        let first = minicbor::to_vec(record).unwrap();
        let second = minicbor::to_vec(record).unwrap();
        assert_eq!(first, second);
        assert_eq!(&minicbor::decode::<R>(&first).unwrap(), record);
        first
    }

    #[test]
    fn records_encode_deterministically_and_round_trip() {
        let version = encode_stably(&SchemaVersion { version: 1 });
        // A definite-length array of one item, then the unsigned integer 1.
        assert_eq!(version, [0x81, 0x01]);

        let position = encode_stably(&StepPosition {
            step: StepKey([1; 32]),
        });
        assert_eq!(position.get(..3), Some(&[0x81, 0x58, 0x20][..]));

        encode_stably(&TaskRecord {
            program_hash: [2; 32],
            succeeded: true,
        });
        encode_stably(&LastWriter {
            task: task("a", 0),
            step: StepKey([3; 32]),
        });
    }

    #[test]
    fn sets_encode_sorted_whatever_the_insertion_order() {
        let forward = LearnedOrder {
            producers: [task("a", 0), task("b", 0), task("c", 1)].into(),
        };
        let backward = LearnedOrder {
            producers: [task("c", 1), task("b", 0), task("a", 0)].into(),
        };
        assert_eq!(encode_stably(&forward), encode_stably(&backward));
    }
}
