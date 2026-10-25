// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The contents of step records.
//!
//! A [`StepRecord`] holds what kizz knows about one run of a step: the full step
//! specification, every [`Observation`] with its recorded [`PathState`], how the
//! command ended, its raw captured output, how long it ran, when it ran, why it
//! ran, and the tasks that own it.
//!
//! Every type here is encoded by the rules of "Step keys": definite lengths, fields
//! in a fixed order, and maps sorted by key. Paths and names are byte strings, so
//! paths that are not UTF-8 survive unchanged.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

use minicbor::{Decode, Decoder, Encode, Encoder, bytes::ByteArray, decode, encode};

use crate::records::{Family, Record, StepKey, TaskId};

/// Encodes and decodes a path as a CBOR byte string of its bytes.
pub(crate) mod path_bytes {
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

/// Encodes and decodes a list of paths as a definite-length CBOR array of byte
/// strings, one per path.
pub(crate) mod path_list {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _, path::PathBuf};

    use minicbor::{Decoder, Encoder, decode::Error as DecodeError, encode::Error as EncodeError};

    /// Encodes `paths` as an array of byte strings.
    pub fn encode<C, W: minicbor::encode::Write, P: AsRef<[PathBuf]>>(
        paths: &P,
        e: &mut Encoder<W>,
        _ctx: &mut C,
    ) -> Result<(), EncodeError<W::Error>> {
        let paths = paths.as_ref();
        e.array(u64::try_from(paths.len()).unwrap_or(u64::MAX))?;
        for path in paths {
            e.bytes(path.as_os_str().as_bytes())?;
        }
        Ok(())
    }

    /// Decodes a list of paths from an array of byte strings.
    pub fn decode<C>(
        d: &mut Decoder<'_>,
        _ctx: &mut C,
    ) -> Result<Vec<PathBuf>, DecodeError> {
        let Some(len) = d.array()? else {
            return Err(DecodeError::message("a path list has a definite length"));
        };
        let mut paths = Vec::new();
        for _ in 0..len {
            paths.push(PathBuf::from(OsStr::from_bytes(d.bytes()?)));
        }
        Ok(paths)
    }
}

/// A BLAKE3 hash: of a file's content, of a directory's entries, or of a task
/// program.
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
    /// Whether a different modification or change time of a file or directory that
    /// the step read, ran, listed, or checked is a change.
    #[n(12)]
    pub timestamps: bool,
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

/// The permission bits of a mode: the low twelve bits, the set-user-ID,
/// set-group-ID, and sticky bits included.
pub const PERMISSION_BITS: u32 = 0o7777;

/// The permission bits of `mode`, without its file type.
#[must_use]
pub const fn permission_bits(mode: u32) -> u32 {
    mode & PERMISSION_BITS
}

/// The owner of a file or directory: its user and group numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct Owner {
    /// The user number.
    #[n(0)]
    pub uid: u32,
    /// The group number.
    #[n(1)]
    pub gid: u32,
}

impl fmt::Display for Owner {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// The modification and change times of a regular file or a directory, in
/// nanoseconds since the Unix epoch. Access times are never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct Times {
    /// The modification time.
    #[n(0)]
    pub mtime_nanos: i64,
    /// The change time.
    #[n(1)]
    pub ctime_nanos: i64,
}

/// Write a time in nanoseconds since the Unix epoch as seconds with nine decimals.
fn write_time(
    f: &mut fmt::Formatter<'_>,
    nanos: i64,
) -> fmt::Result {
    let sign = if nanos < 0 { "-" } else { "" };
    let magnitude = nanos.unsigned_abs();
    write!(
        f,
        "{sign}{}.{:09}",
        magnitude / 1_000_000_000,
        magnitude % 1_000_000_000
    )
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
    /// The owner.
    #[n(6)]
    pub owner: Owner,
}

impl FileMeta {
    /// The modification and change times.
    #[must_use]
    pub const fn times(&self) -> Times {
        Times {
            mtime_nanos: self.mtime_nanos,
            ctime_nanos: self.ctime_nanos,
        }
    }
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

/// The recorded contents of a listed directory, in the order the file system
/// returned its entries when kizz read it. Entry inode numbers are not recorded.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Listing {
    /// The hash of the entry names and types, in the order the file system
    /// returned them.
    #[n(0)]
    pub hash: ContentHash,
    /// The entries, in the order the file system returned them, when there are at
    /// most [`MAX_LISTED_NAMES`]; `None` above that.
    #[n(1)]
    pub entries: Option<Vec<DirEntry>>,
    /// The hash of the same entries sorted by name and type, which tells a change
    /// of order alone from a change of entries, in a directory of any size.
    #[n(2)]
    pub sorted_hash: ContentHash,
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
    /// A directory: its mode, owner, and times, and its listing, in the order the
    /// file system returned its entries, when it was listed.
    #[n(2)]
    Directory {
        /// The mode: the file type and permission bits.
        #[n(0)]
        mode: u32,
        /// The owner.
        #[n(1)]
        owner: Owner,
        /// The listing, when the directory was listed.
        #[n(2)]
        listing: Option<Listing>,
        /// The modification and change times.
        #[n(3)]
        times: Times,
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
    /// The command could not start, for a reason other than a missing or
    /// non-executable program, so its result is not cached.
    #[n(4)]
    NotStarted,
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

/// What changed about one recorded path, as a cache check found it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub enum Change {
    /// Something exists where nothing did.
    #[n(0)]
    Appeared,
    /// Nothing exists where something did.
    #[n(1)]
    Disappeared,
    /// The type changed, such as a file that became a directory, or the path can or
    /// can no longer be read.
    #[n(2)]
    Type,
    /// A regular file's or a directory's permission bits changed, found without
    /// hashing.
    #[n(8)]
    Permissions {
        /// The recorded permission bits.
        #[n(0)]
        old: u32,
        /// The current permission bits.
        #[n(1)]
        new: u32,
    },
    /// A regular file's or a directory's owner changed, found without hashing.
    #[n(9)]
    Owner {
        /// The recorded owner.
        #[n(0)]
        old: Owner,
        /// The current owner.
        #[n(1)]
        new: Owner,
    },
    /// A regular file's or a directory's modification or change time changed, found
    /// without hashing, for a step run with `timestamps`.
    #[n(10)]
    Timestamps {
        /// The recorded times.
        #[n(0)]
        old: Times,
        /// The current times.
        #[n(1)]
        new: Times,
    },
    /// A regular file's size changed, found without hashing.
    #[n(3)]
    Size {
        /// The recorded size in bytes.
        #[n(0)]
        old: u64,
        /// The current size in bytes.
        #[n(1)]
        new: u64,
    },
    /// A regular file that was read has the same size and different content, found
    /// by hashing.
    #[n(4)]
    Content,
    /// A regular file that was not read, and so has no hash, has the same size and
    /// different metadata.
    #[n(5)]
    Metadata,
    /// The entries of a listed directory changed. The entries that appeared,
    /// disappeared, or changed type are named when both listings kept their
    /// entries; otherwise all three lists are empty.
    #[n(6)]
    Entries {
        /// The entries that appeared, sorted by name.
        #[n(0)]
        appeared: Vec<DirEntry>,
        /// The entries that disappeared, as recorded, sorted by name.
        #[n(1)]
        disappeared: Vec<DirEntry>,
        /// The entries whose type changed, with their current type, sorted by name.
        #[n(2)]
        retyped: Vec<DirEntry>,
    },
    /// A listed directory has the same entries in a different order.
    #[n(11)]
    Order,
    /// A symbolic link points somewhere else.
    #[n(7)]
    LinkTarget {
        /// The recorded target.
        #[cbor(n(0), with = "path_bytes")]
        old: PathBuf,
        /// The current target.
        #[cbor(n(1), with = "path_bytes")]
        new: PathBuf,
    },
}

impl fmt::Display for Change {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Appeared => f.write_str("appeared"),
            Self::Disappeared => f.write_str("disappeared"),
            Self::Type => f.write_str("type changed"),
            Self::Permissions { old, new } => {
                write!(f, "permissions changed from {old:04o} to {new:04o}")
            },
            Self::Owner { old, new } => write!(f, "owner changed from {old} to {new}"),
            Self::Timestamps { old, new } => write_timestamps(f, old, new),
            Self::Size { old, new } => write!(f, "size changed from {old} to {new} bytes"),
            Self::Content => f.write_str("content changed"),
            Self::Metadata => f.write_str("metadata changed"),
            Self::Entries {
                appeared,
                disappeared,
                retyped,
            } => write_entries(f, appeared, disappeared, retyped),
            Self::Order => f.write_str("the order of its entries changed"),
            Self::LinkTarget { old, new } => {
                write!(
                    f,
                    "link target changed from {} to {}",
                    old.display(),
                    new.display()
                )
            },
        }
    }
}

/// The most entry names an entries change shows for each of appeared,
/// disappeared, and changed type; the rest are counted.
const SHOWN_ENTRY_NAMES: usize = 10;

/// Write an entries change: the entries that appeared, disappeared, and changed
/// type, each group when it is not empty, with at most [`SHOWN_ENTRY_NAMES`] names
/// each.
fn write_entries(
    f: &mut fmt::Formatter<'_>,
    appeared: &[DirEntry],
    disappeared: &[DirEntry],
    retyped: &[DirEntry],
) -> fmt::Result {
    f.write_str("directory entries changed")?;
    let mut separator = ": ";
    for (entries, what) in [
        (appeared, "appeared"),
        (disappeared, "disappeared"),
        (retyped, "changed type"),
    ] {
        if entries.is_empty() {
            continue;
        }
        f.write_str(separator)?;
        separator = "; ";
        for (index, entry) in entries.iter().take(SHOWN_ENTRY_NAMES).enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "`{}`", String::from_utf8_lossy(&entry.name))?;
        }
        let more = entries.len().saturating_sub(SHOWN_ENTRY_NAMES);
        if more > 0 {
            write!(f, " and {more} more")?;
        }
        write!(f, " {what}")?;
    }
    Ok(())
}

/// Write a timestamp change: each time that differs, old and new, or both times
/// when neither differs.
fn write_timestamps(
    f: &mut fmt::Formatter<'_>,
    old: &Times,
    new: &Times,
) -> fmt::Result {
    let mtime = old.mtime_nanos != new.mtime_nanos;
    let ctime = old.ctime_nanos != new.ctime_nanos;
    f.write_str("timestamps changed: ")?;
    if mtime || !ctime {
        f.write_str("modification time from ")?;
        write_time(f, old.mtime_nanos)?;
        f.write_str(" to ")?;
        write_time(f, new.mtime_nanos)?;
    }
    if mtime && ctime {
        f.write_str(", ")?;
    }
    if ctime || !mtime {
        f.write_str("change time from ")?;
        write_time(f, old.ctime_nanos)?;
        f.write_str(" to ")?;
        write_time(f, new.ctime_nanos)?;
    }
    Ok(())
}

/// One recorded path that a cache check found changed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct Difference {
    /// The absolute path.
    #[cbor(n(0), with = "path_bytes")]
    pub path: PathBuf,
    /// What changed about it.
    #[n(1)]
    pub change: Change,
}

impl fmt::Display for Difference {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.change)
    }
}

/// Why the result of a run was not cached.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub enum NotCachedReason {
    /// A signal killed the command.
    #[n(0)]
    Signaled {
        /// The signal number.
        #[n(0)]
        signal: i32,
    },
    /// The build was interrupted, and a signal stopped the command.
    #[n(1)]
    Interrupted {
        /// The signal number.
        #[n(0)]
        signal: i32,
    },
    /// The tracer could not observe every access of the run.
    #[n(2)]
    Incomplete {
        /// What the tracer could not observe.
        #[n(0)]
        detail: String,
    },
    /// The command could not start, for a reason other than a missing or
    /// non-executable program.
    #[n(3)]
    StartFailed {
        /// The error that starting the command reported.
        #[n(0)]
        message: String,
    },
    /// The build was interrupted, and the command exited with a code that it chose,
    /// for example after it handled the interrupt.
    #[n(4)]
    InterruptedExited {
        /// The exit code.
        #[n(0)]
        code: i32,
    },
}

impl fmt::Display for NotCachedReason {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Signaled { signal } => write!(f, "killed by signal {signal}"),
            Self::Interrupted { signal } => {
                write!(f, "interrupted, and stopped by signal {signal}")
            },
            Self::InterruptedExited { code } => {
                write!(f, "interrupted, and exited with code {code}")
            },
            Self::Incomplete { detail } => write!(f, "incompletely traced: {detail}"),
            Self::StartFailed { message } => write!(f, "could not start: {message}"),
        }
    }
}

/// Why a step ran.
///
/// The explanations add the reasons that compare a step's specification with the
/// one its task used last time at the same position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub enum RunReason {
    /// There was no record for the step key: the step never ran with this exact
    /// specification.
    #[n(0)]
    NeverRun,
    /// The step is marked `always`, and this was its one run in the build.
    #[n(1)]
    Always,
    /// A record existed, and the cache check found these recorded paths changed.
    #[n(2)]
    InputsChanged {
        /// Every difference, sorted by path.
        #[n(0)]
        differences: Vec<Difference>,
    },
    /// The step's previous run could not be cached, and the cache check found these
    /// paths it touched changed since. In the build of that run, the check compares
    /// the paths with their states after it; in a later build, with the states the
    /// record of that run holds.
    #[n(3)]
    NotCached {
        /// Why the previous run was not cached.
        #[n(0)]
        why: NotCachedReason,
        /// Every difference, sorted by path.
        #[n(1)]
        differences: Vec<Difference>,
    },
}

/// What is recorded about one run of a step, by step key, and the tasks that own
/// the record.
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
    /// state when the step finished, or when a cache check last refreshed it.
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
    /// When kizz began writing the observations, before it read the state of any
    /// path, in nanoseconds since the Unix epoch. A cache check that refreshes the
    /// observations moves it to when that check began. A recorded modification time
    /// within two seconds of it cannot prove a file unchanged.
    #[n(9)]
    pub recorded_at_nanos: u64,
    /// For a program that was missing or not executable, the message of the task
    /// failure, which a cache hit fails with again; for a command that could not
    /// start, the message of its task failure.
    #[n(10)]
    pub failure: Option<String>,
    /// Why the result of the run was not cached, or `None` when it was.
    ///
    /// A record with a reason here is never a cache hit: it replaces any older
    /// record of the step key, so no older result is replayed, and the next build
    /// runs the step again with this reason.
    #[n(11)]
    pub not_cached: Option<NotCachedReason>,
    /// The tasks that own the record, without repeats: every task that produced it
    /// or used it, by its [`TaskId`], the absolute path of its `kizz.md` and the
    /// index of its block in the file.
    ///
    /// The owners belong to the record of the step key, not to one run: a run of a
    /// task whose identity the engine knows adds the task and keeps the owners of
    /// the record it replaces, and a step that its task notes as cached adds the
    /// task. A path reset (`kizz cache reset PATH`) removes the record only when
    /// every owner is a task under the path, and otherwise only takes those tasks
    /// out; `kizz cache clean` takes out the tasks whose `kizz.md` no longer exists.
    /// A record with no owner is no task's entry, so no path reset changes it.
    #[n(12)]
    pub owners: BTreeSet<TaskId>,
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

    /// One path state of each kind, with both kinds of directory state.
    fn states() -> Vec<PathState> {
        let meta = FileMeta {
            size: 3,
            mtime_nanos: -1,
            ctime_nanos: 2,
            inode: 4,
            device: 5,
            mode: 0o100_644,
            owner: Owner {
                uid: 1_000,
                gid: 100,
            },
        };
        vec![
            PathState::Absent,
            PathState::File {
                meta,
                hash: Some(ContentHash([1; 32])),
            },
            PathState::File { meta, hash: None },
            PathState::Directory {
                mode: 0o041_777,
                owner: Owner { uid: 0, gid: 0 },
                listing: None,
                times: Times {
                    mtime_nanos: -3,
                    ctime_nanos: 4,
                },
            },
            PathState::Directory {
                mode: 0o040_750,
                owner: Owner {
                    uid: 1_000,
                    gid: 1_000,
                },
                listing: Some(Listing {
                    hash: ContentHash([2; 32]),
                    entries: Some(vec![
                        DirEntry {
                            name: b"a\xff".to_vec(),
                            kind: EntryKind::Symlink,
                        },
                        DirEntry {
                            name: b"0".to_vec(),
                            kind: EntryKind::File,
                        },
                    ]),
                    sorted_hash: ContentHash([3; 32]),
                }),
                times: Times {
                    mtime_nanos: 5,
                    ctime_nanos: 6,
                },
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
        ]
    }

    /// A step record with one observation of each state.
    fn record() -> StepRecord {
        let states = states();
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
                timestamps: true,
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
            recorded_at_nanos: 12,
            failure: Some("program `cc` is not executable".to_owned()),
            not_cached: Some(NotCachedReason::Signaled { signal: 9 }),
            owners: [
                TaskId {
                    file: PathBuf::from("/b/kizz.md"),
                    index: 0,
                },
                TaskId {
                    file: PathBuf::from("/a\u{e9}/kizz.md"),
                    index: 3,
                },
                TaskId {
                    file: PathBuf::from("/a\u{e9}/kizz.md"),
                    index: 1,
                },
            ]
            .into(),
        }
    }

    /// One run reason of each kind, with one difference of each kind.
    fn reasons() -> Vec<RunReason> {
        let differences: Vec<Difference> = [
            Change::Appeared,
            Change::Disappeared,
            Change::Type,
            Change::Permissions {
                old: 0o4755,
                new: 0o644,
            },
            Change::Owner {
                old: Owner {
                    uid: 1_000,
                    gid: 1_000,
                },
                new: Owner { uid: 0, gid: 0 },
            },
            Change::Timestamps {
                old: Times {
                    mtime_nanos: 1,
                    ctime_nanos: -2,
                },
                new: Times {
                    mtime_nanos: 3,
                    ctime_nanos: 4,
                },
            },
            Change::Size { old: 1, new: 2 },
            Change::Content,
            Change::Metadata,
            Change::Entries {
                appeared: Vec::new(),
                disappeared: Vec::new(),
                retyped: Vec::new(),
            },
            Change::Entries {
                appeared: vec![DirEntry {
                    name: b"new\xff".to_vec(),
                    kind: EntryKind::Directory,
                }],
                disappeared: vec![DirEntry {
                    name: b"old".to_vec(),
                    kind: EntryKind::File,
                }],
                retyped: vec![DirEntry {
                    name: b"kind".to_vec(),
                    kind: EntryKind::Symlink,
                }],
            },
            Change::Order,
            Change::LinkTarget {
                old: PathBuf::from("a"),
                new: PathBuf::from("b\u{e9}"),
            },
        ]
        .into_iter()
        .map(|change| {
            Difference {
                path: PathBuf::from("/in"),
                change,
            }
        })
        .collect();
        let whys = [
            NotCachedReason::Signaled { signal: 9 },
            NotCachedReason::Interrupted { signal: 2 },
            NotCachedReason::InterruptedExited { code: 130 },
            NotCachedReason::Incomplete {
                detail: "int 0x80".to_owned(),
            },
            NotCachedReason::StartFailed {
                message: "out of memory".to_owned(),
            },
        ];
        let mut reasons = vec![
            RunReason::NeverRun,
            RunReason::Always,
            RunReason::InputsChanged {
                differences: differences.clone(),
            },
        ];
        reasons.extend(whys.into_iter().map(|why| {
            RunReason::NotCached {
                why,
                differences: differences.clone(),
            }
        }));
        reasons
    }

    #[test]
    fn run_reasons_round_trip() {
        for reason in reasons() {
            let mut record = record();
            record.reason = reason;
            let bytes = minicbor::to_vec(&record).unwrap();
            assert_eq!(minicbor::decode::<StepRecord>(&bytes).unwrap(), record);
        }
    }

    #[test]
    fn exits_and_not_cached_markers_round_trip() {
        let exits = [
            StepExit::Exited { code: -1 },
            StepExit::Signaled { signal: 2 },
            StepExit::ProgramNotFound,
            StepExit::ProgramNotExecutable,
            StepExit::NotStarted,
        ];
        let markers = [
            None,
            Some(NotCachedReason::Interrupted { signal: 2 }),
            Some(NotCachedReason::InterruptedExited { code: 130 }),
            Some(NotCachedReason::Incomplete {
                detail: "int 0x80".to_owned(),
            }),
            Some(NotCachedReason::StartFailed {
                message: "out of memory".to_owned(),
            }),
        ];
        for exit in exits {
            for not_cached in markers.clone() {
                let mut record = record();
                record.exit = exit;
                record.not_cached = not_cached;
                let bytes = minicbor::to_vec(&record).unwrap();
                assert_eq!(minicbor::decode::<StepRecord>(&bytes).unwrap(), record);
            }
        }
    }

    /// A timestamp change names each time that changed, old and new, in seconds
    /// with nine decimals.
    #[test]
    fn timestamp_changes_display() {
        let times = |mtime_nanos, ctime_nanos| {
            Times {
                mtime_nanos,
                ctime_nanos,
            }
        };
        let shown: Vec<String> = [
            (
                times(1_700_000_000_000_000_000, 1_700_000_000_000_000_000),
                times(1_700_000_001_500_000_000, 1_700_000_000_000_000_000),
            ),
            (times(1, 2), times(1, -1_000_000_001)),
            (times(0, 0), times(1, 2)),
        ]
        .into_iter()
        .map(|(old, new)| Change::Timestamps { old, new }.to_string())
        .collect();
        assert_eq!(shown, [
            "timestamps changed: modification time from 1700000000.000000000 to \
             1700000001.500000000",
            "timestamps changed: change time from 0.000000002 to -1.000000001",
            "timestamps changed: modification time from 0.000000000 to 0.000000001, change \
             time from 0.000000000 to 0.000000002",
        ]);
    }

    #[test]
    fn changes_and_reasons_display() {
        let shown: Vec<String> = [
            Change::Appeared,
            Change::Disappeared,
            Change::Type,
            Change::Permissions {
                old: 0o755,
                new: 0o644,
            },
            Change::Permissions {
                old: 0o7777,
                new: 0,
            },
            Change::Owner {
                old: Owner {
                    uid: 1_000,
                    gid: 1_000,
                },
                new: Owner { uid: 0, gid: 0 },
            },
            Change::Size { old: 1, new: 2 },
            Change::Content,
            Change::Metadata,
            Change::Entries {
                appeared: Vec::new(),
                disappeared: Vec::new(),
                retyped: Vec::new(),
            },
            Change::Order,
            Change::LinkTarget {
                old: PathBuf::from("a"),
                new: PathBuf::from("b"),
            },
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(shown, [
            "appeared",
            "disappeared",
            "type changed",
            "permissions changed from 0755 to 0644",
            "permissions changed from 7777 to 0000",
            "owner changed from 1000:1000 to 0:0",
            "size changed from 1 to 2 bytes",
            "content changed",
            "metadata changed",
            "directory entries changed",
            "the order of its entries changed",
            "link target changed from a to b",
        ]);
        let difference = Difference {
            path: PathBuf::from("/in"),
            change: Change::Content,
        };
        assert_eq!(difference.to_string(), "/in: content changed");
        let whys: Vec<String> = [
            NotCachedReason::Signaled { signal: 9 },
            NotCachedReason::Interrupted { signal: 2 },
            NotCachedReason::InterruptedExited { code: 130 },
            NotCachedReason::Incomplete {
                detail: "lost a process".to_owned(),
            },
            NotCachedReason::StartFailed {
                message: "out of memory".to_owned(),
            },
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(whys, [
            "killed by signal 9",
            "interrupted, and stopped by signal 2",
            "interrupted, and exited with code 130",
            "incompletely traced: lost a process",
            "could not start: out of memory",
        ]);
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
        // A definite-length array of thirteen fields, then the specification as a
        // definite-length array of thirteen.
        assert_eq!(bytes.get(..2), Some(&[0x8D, 0x8D][..]));
        // The environment is a definite-length map of two entries, sorted by name
        // whatever the insertion order.
        let encoded_env = minicbor::to_vec(&record().spec.env).unwrap();
        assert_eq!(encoded_env, b"\xa2\x61A\x611\x61B\x612");
    }

    #[test]
    fn owners_round_trip_sorted_and_without_repeats() {
        let record = record();
        let bytes = minicbor::to_vec(&record).unwrap();
        let decoded = minicbor::decode::<StepRecord>(&bytes).unwrap();
        assert_eq!(decoded.owners, record.owners);
        let order: Vec<(&str, u32)> = decoded
            .owners
            .iter()
            .map(|owner| (owner.file.to_str().unwrap(), owner.index))
            .collect();
        assert_eq!(order, [
            ("/a\u{e9}/kizz.md", 1),
            ("/a\u{e9}/kizz.md", 3),
            ("/b/kizz.md", 0)
        ]);
        // The owners are the last field, a definite-length array of three task
        // identities, each an array of the file's bytes and the block index.
        let owners = minicbor::to_vec(&record.owners).unwrap();
        assert!(bytes.ends_with(&owners));
        assert_eq!(owners.first(), Some(&0x83));
        // A record with no owner keeps an empty set.
        let mut unowned = record;
        unowned.owners.clear();
        let bytes = minicbor::to_vec(&unowned).unwrap();
        assert_eq!(bytes.last(), Some(&0x80));
        assert_eq!(minicbor::decode::<StepRecord>(&bytes).unwrap(), unowned);
    }

    #[test]
    fn access_bits_report_their_kinds() {
        let bits = AccessBits(AccessBits::READ | AccessBits::EXEC);
        assert!(bits.contains(AccessBits::READ));
        assert!(bits.contains(AccessBits::EXEC));
        assert!(!bits.contains(AccessBits::PROBE));
        assert_eq!(minicbor::to_vec(bits).unwrap(), [0x11]);
    }

    /// File states carry the owner as the last field of their metadata, directory
    /// states carry their mode, owner, and times whether or not they were listed,
    /// and both encode deterministically and round-trip.
    #[test]
    fn file_and_directory_states_carry_mode_owner_and_times() {
        let owner = Owner {
            uid: 1_000,
            gid: 100,
        };
        let meta = FileMeta {
            size: 3,
            mtime_nanos: 1,
            ctime_nanos: 2,
            inode: 4,
            device: 5,
            mode: 0o104_755,
            owner,
        };
        let owner_bytes = minicbor::to_vec(owner).unwrap();
        assert_eq!(owner_bytes, [0x82, 0x19, 0x03, 0xE8, 0x18, 0x64]);
        let meta_bytes = minicbor::to_vec(meta).unwrap();
        // A definite-length array of seven fields, ending with the owner.
        assert_eq!(meta_bytes.first(), Some(&0x87));
        assert!(meta_bytes.ends_with(&owner_bytes));
        let times = meta.times();
        assert_eq!(times, Times {
            mtime_nanos: 1,
            ctime_nanos: 2,
        });
        let times_bytes = minicbor::to_vec(times).unwrap();
        // A definite-length array of the modification and change times.
        assert_eq!(times_bytes, [0x82, 0x01, 0x02]);

        let file = PathState::File {
            meta,
            hash: Some(ContentHash([1; 32])),
        };
        let checked = PathState::Directory {
            mode: 0o041_777,
            owner,
            listing: None,
            times,
        };
        let listed = PathState::Directory {
            mode: 0o040_755,
            owner,
            listing: Some(Listing {
                hash: ContentHash([2; 32]),
                entries: Some(Vec::new()),
                sorted_hash: ContentHash([2; 32]),
            }),
            times,
        };
        for state in [file, checked.clone(), listed] {
            let first = minicbor::to_vec(&state).unwrap();
            assert_eq!(first, minicbor::to_vec(&state).unwrap());
            assert_eq!(minicbor::decode::<PathState>(&first).unwrap(), state);
        }
        // The mode, the owner, and each time reach the bytes of a directory state,
        // whose times are its last field.
        let other_owner = PathState::Directory {
            mode: 0o041_777,
            owner: Owner { uid: 0, gid: 100 },
            listing: None,
            times,
        };
        let other_mode = PathState::Directory {
            mode: 0o040_777,
            owner,
            listing: None,
            times,
        };
        let later_modification = PathState::Directory {
            mode: 0o041_777,
            owner,
            listing: None,
            times: Times {
                mtime_nanos: 7,
                ..times
            },
        };
        let later_change = PathState::Directory {
            mode: 0o041_777,
            owner,
            listing: None,
            times: Times {
                ctime_nanos: 7,
                ..times
            },
        };
        let checked = minicbor::to_vec(&checked).unwrap();
        assert!(checked.ends_with(&times_bytes));
        for other in [other_owner, other_mode, later_modification, later_change] {
            assert_ne!(checked, minicbor::to_vec(&other).unwrap());
        }
        assert_eq!(permission_bits(meta.mode), 0o4755);
        assert_eq!(PERMISSION_BITS, 0o7777);
    }

    /// The `timestamps` option is the last field of the specification's encoding,
    /// and both values round-trip.
    #[test]
    fn the_timestamps_option_encodes_deterministically_and_round_trips() {
        for timestamps in [false, true] {
            let mut spec = record().spec;
            spec.timestamps = timestamps;
            let first = minicbor::to_vec(&spec).unwrap();
            assert_eq!(first, minicbor::to_vec(&spec).unwrap());
            assert_eq!(first.last(), Some(if timestamps { &0xF5 } else { &0xF4 }));
            assert_eq!(minicbor::decode::<StepSpecRecord>(&first).unwrap(), spec);
        }
    }

    /// An entries change names what appeared, disappeared, and changed type, each
    /// group only when it is not empty and with at most ten names, and an order
    /// change says that only the order changed.
    #[test]
    fn entries_and_order_changes_display() {
        let entry = |name: &str| {
            DirEntry {
                name: name.as_bytes().to_vec(),
                kind: EntryKind::File,
            }
        };
        let many: Vec<DirEntry> = (0..12)
            .map(|index| entry(&format!("f{index:02}")))
            .collect();
        let shown: Vec<String> = [
            Change::Entries {
                appeared: vec![entry("b.txt")],
                disappeared: vec![entry("a.txt"), entry("c.txt")],
                retyped: vec![entry("d")],
            },
            Change::Entries {
                appeared: Vec::new(),
                disappeared: vec![entry("gone")],
                retyped: Vec::new(),
            },
            Change::Entries {
                appeared: many,
                disappeared: Vec::new(),
                retyped: vec![DirEntry {
                    name: b"x\xff".to_vec(),
                    kind: EntryKind::Directory,
                }],
            },
            Change::Order,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let many_and_invalid = format!(
            "directory entries changed: `f00`, `f01`, `f02`, `f03`, `f04`, `f05`, `f06`, `f07`, \
             `f08`, `f09` and 2 more appeared; `x{}` changed type",
            char::REPLACEMENT_CHARACTER
        );
        assert_eq!(shown, [
            "directory entries changed: `b.txt` appeared; `a.txt`, `c.txt` disappeared; `d` \
             changed type",
            "directory entries changed: `gone` disappeared",
            many_and_invalid.as_str(),
            "the order of its entries changed",
        ]);
    }

    /// A listing keeps its entries in the order given, encodes deterministically as
    /// a definite-length array of the ordered hash, the entries, and the sorted
    /// hash, and round-trips, with or without its entries.
    #[test]
    fn listings_keep_their_order_encode_deterministically_and_round_trip() {
        let entries = vec![
            DirEntry {
                name: b"z".to_vec(),
                kind: EntryKind::File,
            },
            DirEntry {
                name: b"a".to_vec(),
                kind: EntryKind::Directory,
            },
        ];
        let listed = Listing {
            hash: ContentHash([4; 32]),
            entries: Some(entries.clone()),
            sorted_hash: ContentHash([5; 32]),
        };
        let large = Listing {
            entries: None,
            ..listed.clone()
        };
        for listing in [&listed, &large] {
            let first = minicbor::to_vec(listing).unwrap();
            assert_eq!(first, minicbor::to_vec(listing).unwrap());
            assert_eq!(first.first(), Some(&0x83));
            assert!(first.ends_with(&minicbor::to_vec(ContentHash([5; 32])).unwrap()));
            assert_eq!(&minicbor::decode::<Listing>(&first).unwrap(), listing);
        }
        let decoded = minicbor::decode::<Listing>(&minicbor::to_vec(&listed).unwrap()).unwrap();
        assert_eq!(decoded.entries, Some(entries.clone()));
        // The same entries in another order encode differently.
        let reversed = Listing {
            entries: Some(entries.into_iter().rev().collect()),
            ..listed.clone()
        };
        assert_ne!(
            minicbor::to_vec(&listed).unwrap(),
            minicbor::to_vec(&reversed).unwrap()
        );
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
