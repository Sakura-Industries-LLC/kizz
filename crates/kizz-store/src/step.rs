// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The contents of step records.
//!
//! A [`StepRecord`] holds what kizz knows about one run of a step: the full step
//! specification, every [`Observation`] with its recorded [`PathState`], how the
//! command ended, its raw captured output, how long it ran, when it ran, and why it
//! ran.
//!
//! Every type here is encoded by the rules of "Step keys": definite lengths, fields
//! in a fixed order, and maps sorted by key. Paths and names are byte strings, so
//! paths that are not UTF-8 survive unchanged.

use std::{collections::BTreeMap, fmt, path::PathBuf};

use minicbor::{Decode, Decoder, Encode, Encoder, bytes::ByteArray, decode, encode};

use crate::records::{Family, Record, StepKey};

/// Encodes and decodes a path as a CBOR byte string of its bytes.
mod path_bytes {
    use std::{
        ffi::OsStr,
        os::unix::ffi::OsStrExt as _,
        path::{Path, PathBuf},
    };

    use minicbor::{Decoder, Encoder, decode::Error as DecodeError, encode::Error as EncodeError};

    /// Encodes `path` as a byte string.
    pub fn encode<C, W: minicbor::encode::Write, P: AsRef<Path>>(
        path: &P,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), EncodeError<W::Error>> {
        e.bytes(path.as_ref().as_os_str().as_bytes())?.ok()
    }

    /// Decodes a path from a byte string.
    pub fn decode<C>(
        d: &mut Decoder<'_>,
        _ctx: &mut C,
    ) -> Result<PathBuf, DecodeError> {
        Ok(PathBuf::from(OsStr::from_bytes(d.bytes()?)))
    }
}

/// A BLAKE3 hash: of a file's content, of a directory's sorted entries, or of a
/// task program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(pub [u8; 32]);

impl fmt::Display for ContentHash {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl<C> Encode<C> for ContentHash {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        e.bytes(&self.0)?.ok()
    }
}

impl<'b, C> Decode<'b, C> for ContentHash {
    fn decode(
        d: &mut Decoder<'b>,
        ctx: &mut C,
    ) -> Result<Self, decode::Error> {
        ByteArray::<32>::decode(d, ctx).map(|bytes| Self(bytes.into()))
    }
}

/// How a step captured its output, as its specification gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum Capture {
    /// Standard output and standard error separately.
    #[n(0)]
    Split,
    /// Both streams through one pipe, in exact order.
    #[n(1)]
    Merged,
}

/// The full specification of a step, as its `run()` call gave it, with the values
/// that place it in its task: the task program hash and the occurrence number.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct StepSpecRecord {
    /// The hash of the task program: its block and every module it loads.
    #[n(0)]
    pub program_hash: ContentHash,
    /// The absolute working directory.
    #[cbor(n(1), with = "path_bytes")]
    pub cwd: PathBuf,
    /// The argument vector as written: the program, then its arguments.
    #[n(2)]
    pub argv: Vec<String>,
    /// The exact environment passed to the process, sorted by name.
    #[n(3)]
    pub env: BTreeMap<String, String>,
    /// The text for standard input, or `None` for `/dev/null`.
    #[n(4)]
    pub stdin: Option<String>,
    /// How the output was captured.
    #[n(5)]
    pub capture: Capture,
    /// Whether output was also echoed live.
    #[n(6)]
    pub stream: bool,
    /// Whether the step runs on every build.
    #[n(7)]
    pub always: bool,
    /// The ignore patterns, as written.
    #[n(8)]
    pub ignore: Vec<String>,
    /// The `track` patterns, as written.
    #[n(9)]
    pub track: Vec<String>,
    /// The acceptable exit codes, or `None` when the exit status never fails the
    /// task.
    #[n(10)]
    pub expect: Option<Vec<i32>>,
    /// The number of identical commands earlier in the same task run.
    #[n(11)]
    pub occurrence: u32,
}

/// The combined kinds of access a step made to one path, one bit per kind.
///
/// The bits follow the order of the tracer interface's access kinds: read, probe,
/// list, write, exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Encode, Decode)]
#[cbor(transparent)]
pub struct AccessBits(#[n(0)] pub u8);

impl AccessBits {
    /// The path was run as a program.
    pub const EXEC: u8 = 0b0001_0000;
    /// A directory's entries were read.
    pub const LIST: u8 = 0b0000_0100;
    /// Existence or metadata was checked, including a lookup that found nothing.
    pub const PROBE: u8 = 0b0000_0010;
    /// Content was read.
    pub const READ: u8 = 0b0000_0001;
    /// The path was created, modified, renamed, removed, or had its metadata changed.
    pub const WRITE: u8 = 0b0000_1000;

    /// Whether every bit of `bit` is set.
    #[must_use]
    pub const fn contains(
        self,
        bit: u8,
    ) -> bool {
        self.0 & bit == bit
    }
}

/// The metadata of a regular file that a cache check compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct FileMeta {
    /// The size in bytes.
    #[n(0)]
    pub size: u64,
    /// The modification time, in nanoseconds since the Unix epoch.
    #[n(1)]
    pub mtime_nanos: i64,
    /// The change time, in nanoseconds since the Unix epoch.
    #[n(2)]
    pub ctime_nanos: i64,
    /// The inode number.
    #[n(3)]
    pub inode: u64,
    /// The device number.
    #[n(4)]
    pub device: u64,
    /// The mode: the file type and permission bits.
    #[n(5)]
    pub mode: u32,
}

/// The type of a directory entry, as the listing reports it, without following
/// symbolic links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum EntryKind {
    /// A regular file.
    #[n(0)]
    File,
    /// A directory.
    #[n(1)]
    Directory,
    /// A symbolic link.
    #[n(2)]
    Symlink,
    /// A block device.
    #[n(3)]
    BlockDevice,
    /// A character device.
    #[n(4)]
    CharDevice,
    /// A FIFO.
    #[n(5)]
    Fifo,
    /// A socket.
    #[n(6)]
    Socket,
    /// A type the listing could not determine.
    #[n(7)]
    Unknown,
}

impl EntryKind {
    /// The byte that stands for this kind in a listing hash.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::File => 0,
            Self::Directory => 1,
            Self::Symlink => 2,
            Self::BlockDevice => 3,
            Self::CharDevice => 4,
            Self::Fifo => 5,
            Self::Socket => 6,
            Self::Unknown => 7,
        }
    }
}

/// One entry of a listed directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct DirEntry {
    /// The entry's name, as bytes.
    #[cbor(n(0), with = "minicbor::bytes")]
    pub name: Vec<u8>,
    /// The entry's type.
    #[n(1)]
    pub kind: EntryKind,
}

/// The recorded contents of a listed directory.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Listing {
    /// The hash of the sorted entry names and types.
    #[n(0)]
    pub hash: ContentHash,
    /// The entries, sorted by name, when there are at most
    /// [`MAX_LISTED_NAMES`]; `None` above that.
    #[n(1)]
    pub entries: Option<Vec<DirEntry>>,
}

/// The most entries a listed directory may have for its names to be kept.
pub const MAX_LISTED_NAMES: usize = 1_000;

/// A device, FIFO, or socket, which is recorded by type only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum SpecialKind {
    /// A block device.
    #[n(0)]
    BlockDevice,
    /// A character device.
    #[n(1)]
    CharDevice,
    /// A FIFO.
    #[n(2)]
    Fifo,
    /// A socket.
    #[n(3)]
    Socket,
    /// A type that is none of the others.
    #[n(4)]
    Other,
}

/// The state of a touched path when its step finished.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum PathState {
    /// Nothing exists at the path.
    #[n(0)]
    Absent,
    /// A regular file: its metadata, and the hash of its content when it was read
    /// or run as a program. A file that was only checked or written is never
    /// hashed.
    #[n(1)]
    File {
        /// The metadata.
        #[n(0)]
        meta: FileMeta,
        /// The hash of the content, when the file was read or run.
        #[n(1)]
        hash: Option<ContentHash>,
    },
    /// A directory: its sorted entries when it was listed, otherwise only its
    /// existence and type.
    #[n(2)]
    Directory {
        /// The listing, when the directory was listed.
        #[n(0)]
        listing: Option<Listing>,
    },
    /// A symbolic link: its target, and the state of what it points to.
    #[n(3)]
    Symlink {
        /// The link's target, as stored in the link.
        #[cbor(n(0), with = "path_bytes")]
        target: PathBuf,
        /// The state of what the link points to, following every link.
        #[n(1)]
        pointee: Box<PathState>,
    },
    /// A device, FIFO, or socket: its type only.
    #[n(4)]
    Special {
        /// The type.
        #[n(0)]
        kind: SpecialKind,
    },
    /// A path under `/proc` or `/sys` that no `track` pattern matched: the access is
    /// recorded, but its state is not, and it is never compared.
    #[n(5)]
    Untracked,
    /// The path exists but its state could not be read.
    #[n(6)]
    Inaccessible {
        /// The operating system's error number.
        #[n(0)]
        errno: i32,
    },
}

/// One path a step touched, how it touched it, and its state when the step
/// finished.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Observation {
    /// The absolute path.
    #[cbor(n(0), with = "path_bytes")]
    pub path: PathBuf,
    /// The combined kinds of access.
    #[n(1)]
    pub access: AccessBits,
    /// Whether the path is under `/proc` or `/sys` and a `track` pattern matched it,
    /// so its content counts as an input.
    #[n(2)]
    pub tracked: bool,
    /// The state of the path when the step finished.
    #[n(3)]
    pub state: PathState,
}

/// How a recorded step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub enum StepExit {
    /// The command exited with this exit code.
    #[n(0)]
    Exited {
        /// The exit code.
        #[n(0)]
        code: i32,
    },
    /// A signal with this number killed the command.
    #[n(1)]
    Signaled {
        /// The signal number.
        #[n(0)]
        signal: i32,
    },
    /// The program was not found, so the command never started.
    #[n(2)]
    ProgramNotFound,
    /// The program exists but is not executable, so the command never started.
    #[n(3)]
    ProgramNotExecutable,
}

/// A step's raw captured output, exactly as the command wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum RecordedOutput {
    /// Standard output and standard error separately.
    #[n(0)]
    Split {
        /// Standard output.
        #[cbor(n(0), with = "minicbor::bytes")]
        stdout: Vec<u8>,
        /// Standard error.
        #[cbor(n(1), with = "minicbor::bytes")]
        stderr: Vec<u8>,
    },
    /// Both streams, in exact order.
    #[n(1)]
    Merged {
        /// The interleaved output.
        #[cbor(n(0), with = "minicbor::bytes")]
        output: Vec<u8>,
    },
}

/// Why a step ran.
///
/// The cache check and the explanations add the reasons that compare a record with
/// the current state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub enum RunReason {
    /// There was no record for the step key: the step never ran with this exact
    /// specification.
    #[n(0)]
    NeverRun,
    /// The step is marked `always`.
    #[n(1)]
    Always,
    /// A record existed, and the step ran without comparing it, because this
    /// engine has no cache check yet.
    #[n(2)]
    Unchecked,
}

/// What is recorded about one run of a step, by step key.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct StepRecord {
    /// The full step specification.
    #[n(0)]
    pub spec: StepSpecRecord,
    /// The program that ran, after the lookup through `PATH`, or `None` when no
    /// program was found.
    #[n(1)]
    pub program: Option<ProgramPath>,
    /// Every touched path that no ignore pattern matched, sorted by path, with its
    /// state when the step finished.
    #[n(2)]
    pub observations: Vec<Observation>,
    /// The number of touched paths that ignore patterns left out.
    #[n(3)]
    pub ignored: u64,
    /// How the command ended.
    #[n(4)]
    pub exit: StepExit,
    /// The raw captured output.
    #[n(5)]
    pub output: RecordedOutput,
    /// How long the command ran, in nanoseconds.
    #[n(6)]
    pub duration_nanos: u64,
    /// When the command ran, in nanoseconds since the Unix epoch.
    #[n(7)]
    pub ran_at_nanos: u64,
    /// Why the step ran.
    #[n(8)]
    pub reason: RunReason,
}

impl Record for StepRecord {
    type Key = StepKey;

    const FAMILY: Family = Family::Step;
}

/// The absolute path of a program that a step ran.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProgramPath(pub PathBuf);

impl<C> Encode<C> for ProgramPath {
    fn encode<W: encode::Write>(
        &self,
        e: &mut Encoder<W>,
        ctx: &mut C,
    ) -> Result<(), encode::Error<W::Error>> {
        path_bytes::encode(&self.0, e, ctx)
    }
}

impl<'b, C> Decode<'b, C> for ProgramPath {
    fn decode(
        d: &mut Decoder<'b>,
        ctx: &mut C,
    ) -> Result<Self, decode::Error> {
        path_bytes::decode(d, ctx).map(Self)
    }
}

/// Unit tests for the encoding of step records.
#[cfg(test)]
mod tests {
    use super::*;

    /// A step record with one observation of each state.
    fn record() -> StepRecord {
        let meta = FileMeta {
            size: 3,
            mtime_nanos: -1,
            ctime_nanos: 2,
            inode: 4,
            device: 5,
            mode: 0o100_644,
        };
        let states = vec![
            PathState::Absent,
            PathState::File {
                meta,
                hash: Some(ContentHash([1; 32])),
            },
            PathState::File { meta, hash: None },
            PathState::Directory { listing: None },
            PathState::Directory {
                listing: Some(Listing {
                    hash: ContentHash([2; 32]),
                    entries: Some(vec![DirEntry {
                        name: b"a\xff".to_vec(),
                        kind: EntryKind::Symlink,
                    }]),
                }),
            },
            PathState::Symlink {
                target: PathBuf::from("../x"),
                pointee: Box::new(PathState::Absent),
            },
            PathState::Special {
                kind: SpecialKind::Fifo,
            },
            PathState::Untracked,
            PathState::Inaccessible { errno: 13 },
        ];
        StepRecord {
            spec: StepSpecRecord {
                program_hash: ContentHash([9; 32]),
                cwd: PathBuf::from("/work"),
                argv: vec!["cc".to_owned(), "-c".to_owned()],
                env: [
                    ("B".to_owned(), "2".to_owned()),
                    ("A".to_owned(), "1".to_owned()),
                ]
                .into(),
                stdin: Some("in".to_owned()),
                capture: Capture::Merged,
                stream: true,
                always: false,
                ignore: vec!["re:x".to_owned()],
                track: vec!["/proc/cpuinfo".to_owned()],
                expect: Some(vec![0, 1]),
                occurrence: 2,
            },
            program: Some(ProgramPath(PathBuf::from("/usr/bin/cc"))),
            observations: states
                .into_iter()
                .enumerate()
                .map(|(index, state)| {
                    Observation {
                        path: PathBuf::from(format!("/p/{index}")),
                        access: AccessBits(AccessBits::READ | AccessBits::PROBE),
                        tracked: index == 1,
                        state,
                    }
                })
                .collect(),
            ignored: 7,
            exit: StepExit::Signaled { signal: 9 },
            output: RecordedOutput::Split {
                stdout: b"out".to_vec(),
                stderr: vec![0xFF],
            },
            duration_nanos: 10,
            ran_at_nanos: 11,
            reason: RunReason::NeverRun,
        }
    }

    #[test]
    fn step_records_round_trip_with_stable_bytes() {
        let record = record();
        let first = minicbor::to_vec(&record).unwrap();
        let second = minicbor::to_vec(record.clone()).unwrap();
        assert_eq!(first, second);
        assert_eq!(minicbor::decode::<StepRecord>(&first).unwrap(), record);
    }

    #[test]
    fn records_use_definite_lengths() {
        let bytes = minicbor::to_vec(record()).unwrap();
        // A definite-length array of nine fields, then the specification as a
        // definite-length array of twelve.
        assert_eq!(bytes.get(..2), Some(&[0x89, 0x8C][..]));
        // The environment is a definite-length map of two entries, sorted by name
        // whatever the insertion order.
        let encoded_env = minicbor::to_vec(&record().spec.env).unwrap();
        assert_eq!(encoded_env, b"\xa2\x61A\x611\x61B\x612");
    }

    #[test]
    fn access_bits_report_their_kinds() {
        let bits = AccessBits(AccessBits::READ | AccessBits::EXEC);
        assert!(bits.contains(AccessBits::READ));
        assert!(bits.contains(AccessBits::EXEC));
        assert!(!bits.contains(AccessBits::PROBE));
        assert_eq!(minicbor::to_vec(bits).unwrap(), [0x11]);
    }

    #[test]
    fn entry_kinds_have_distinct_codes() {
        let kinds = [
            EntryKind::File,
            EntryKind::Directory,
            EntryKind::Symlink,
            EntryKind::BlockDevice,
            EntryKind::CharDevice,
            EntryKind::Fifo,
            EntryKind::Socket,
            EntryKind::Unknown,
        ];
        let codes: std::collections::BTreeSet<u8> = kinds.iter().map(|kind| kind.code()).collect();
        assert_eq!(codes.len(), kinds.len());
        assert_eq!(ContentHash([0xAB; 32]).to_string(), "ab".repeat(32));
    }
}
