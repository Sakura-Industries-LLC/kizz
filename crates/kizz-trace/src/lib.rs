// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz tracer interface.
//!
//! The engine never starts a process itself and knows nothing about system calls.
//! It describes one command in a [`RunRequest`]: the exact program, the argument
//! vector, the exact environment, the absolute working directory, the `stdin`
//! content, the capture mode, and whether output is echoed live. A [`Tracer`] runs
//! the command and returns a [`RunResult`]: how the command ended, its raw captured
//! output, every path its process tree touched with the combined [`AccessKinds`]
//! for each path, the [`FirstReads`] of the run, the system calls of the run that
//! the tracer does not know, the [`Connection`]s of the run, and a report when the
//! trace is incomplete.
//!
//! The first reads hold, for each path whose first read or run of a regular file
//! came before any write of that path in the run, the file's [`FileMetadata`] at
//! that moment. The engine compares them with the final state of each path, so a
//! file that changed after the command read it, whether the command rewrote it or
//! something else changed it while the command ran, keeps the result out of the
//! cache.
//!
//! A system call that the tracer does not know is not a reason for an incomplete
//! trace: the tracer reports its number in [`RunResult::unknown_system_calls`], and
//! the engine decides whether such a run is cached.
//!
//! A connection to an IPv4 or IPv6 address, or to a Unix socket by path or abstract
//! name, is an input that the tracer cannot see: the tracer reports it in
//! [`RunResult::connections`], and the engine records it, warns about it and still
//! caches the result.
//!
//! A request can also ask, with [`RunRequest::tree`], for the [`ProcessTree`] of the
//! run, which the result then holds besides the combined accesses: each traced
//! process, a thread group whose threads fold into it, with its number, its process
//! ID, the number of the process that created it, the programs it ran and the
//! status it ended with. Each [`Program`] holds what the tracer attributes to it:
//! its [`Observations`], each path with its access kinds and each kind with its
//! [`Outcome`]s, `ok`, the name of an error such as `ENOENT`, or `unknown`, and its
//! connections, unknown system calls and every reason that the trace is incomplete.
//! The kinds of each path in the tree, combined over every program, are exactly
//! that path's kinds in the combined accesses. The request flag is off by default,
//! and builds never set it, so a build's tracer does no more work than before, and
//! a failed start carries the tree in its [`TraceError`] only when it is set.
//!
//! A request runs in one of two ways. A command of a build runs in a process
//! group of its own, with pipes or `/dev/null` as its standard streams, so its
//! output is captured and a terminal's Ctrl-C reaches only kizz, which interrupts
//! the command exactly once through [`Cancellation`]. A command that keeps the
//! terminal, which a request asks for with [`RunRequest::terminal`], as
//! `kizz execute` does, uses the standard input, output and error of the process
//! that runs the tracer, and stays in that process's process group and session, so
//! a terminal's input and signals reach it as they would without kizz; nothing of
//! its output is captured. In both ways the command starts with an empty signal
//! mask and the default action for `SIGPIPE`, and every other disposition is
//! inherited, so a signal that kizz ignores stays ignored in the command.
//!
//! The caller can forward a termination [`Signal`] to a running request with
//! [`Cancellation::forward`], which the tracer delivers once: to the command's first
//! process while it lives, and afterwards to each live process of the tree. A
//! signal sent to the whole process group of a command that keeps the terminal
//! reaches it directly, and once more if the caller forwards it, since the caller
//! cannot tell it from a signal sent to the caller alone.
//!
//! A tracer may not keep a traced process stopped: the Linux tracer resumes a
//! process that a stop signal stopped, because with `PTRACE_TRACEME` it cannot learn
//! when `SIGCONT` would end the stop. A terminal's Ctrl-Z therefore stops kizz
//! itself, which keeps the default action of `SIGTSTP`, `SIGTTIN` and `SIGTTOU`,
//! and each traced process then waits at its next traced system call or signal
//! until kizz goes on; a stop signal sent to one traced process alone does not keep
//! it stopped.
//!
//! Through the same interface, the engine reads what the standard library cannot:
//! the extended attributes of a path, with [`Tracer::extended_attributes`], the
//! [`MachineState`] that a command starts with, with [`Tracer::machine_state`], and
//! the name of an error number, with [`Tracer::error_name`], whose default answer
//! is `errno` and the number, [`unnamed_error`].
//!
//! Version 1 has one tracer, for Linux, in `kizz_trace_linux`; tests of the engine
//! implement [`Tracer`] with a fake, which notes first reads with
//! [`FirstReads::note_at`] as it reads or runs files, as the Linux tracer does,
//! answers the attribute query from attributes that the test sets and the machine
//! query with a state that the test scripts, and builds the tree of one process
//! that a request asks for with [`ProcessTree::of_request`]. A fake whose run a
//! test builds an observation report from builds the tree of each of its results
//! with [`ProcessTree::of_run`] instead, which also puts the run's reason, unknown
//! system calls and connections on the one program, as the Linux tracer puts them
//! on the program that made them, so the report says what the run reported.
//! Nothing in the engine changes when the tracer does.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map},
    ffi::OsString,
    fmt, fs,
    io::{self, Write},
    net::SocketAddr,
    ops::{BitOr, BitOrAssign},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

/// One way a command touched a path.
///
/// A path touched in several ways has every one of them recorded, combined in
/// [`AccessKinds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessKind {
    /// The path's content was read.
    Read,
    /// The path's existence or metadata was checked, including a lookup that found
    /// nothing (`ENOENT`).
    Probe,
    /// The entries of a directory were read.
    List,
    /// The path was created, modified, renamed, removed, or had its metadata
    /// changed.
    Write,
    /// The path was run as a program.
    Exec,
    /// The path's extended attributes were read or listed, including a read that
    /// found no such attribute (`ENODATA`), a buffer too small for the answer
    /// (`ERANGE`), or a file system without attributes (`EOPNOTSUPP`).
    Xattr,
}

impl AccessKind {
    /// Every access kind, in the order [`AccessKinds`] lists them.
    pub const ALL: [Self; 6] = [
        Self::Read,
        Self::Probe,
        Self::List,
        Self::Write,
        Self::Exec,
        Self::Xattr,
    ];

    /// The bit that represents this kind in [`AccessKinds`].
    const fn bit(self) -> u8 {
        match self {
            Self::Read => 0b0000_0001,
            Self::Probe => 0b0000_0010,
            Self::List => 0b0000_0100,
            Self::Write => 0b0000_1000,
            Self::Exec => 0b0001_0000,
            Self::Xattr => 0b0010_0000,
        }
    }

    /// The lowercase name of this kind, as explanations show it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Probe => "probe",
            Self::List => "list",
            Self::Write => "write",
            Self::Exec => "exec",
            Self::Xattr => "xattr",
        }
    }
}

impl fmt::Display for AccessKind {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The combined access kinds of one path.
///
/// Combining is a set union: recording the same kind twice changes nothing, and
/// the order in which kinds are recorded does not matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct AccessKinds {
    /// One bit per [`AccessKind`], as [`AccessKind::bit`] assigns them.
    bits: u8,
}

impl AccessKinds {
    /// No access kinds.
    pub const EMPTY: Self = Self { bits: 0 };

    /// The set that holds only `kind`.
    #[must_use]
    pub const fn only(kind: AccessKind) -> Self {
        Self { bits: kind.bit() }
    }

    /// Whether the set holds `kind`.
    #[must_use]
    pub const fn contains(
        self,
        kind: AccessKind,
    ) -> bool {
        self.bits & kind.bit() != 0
    }

    /// Whether the set holds no kind.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Add `kind` to the set.
    pub const fn insert(
        &mut self,
        kind: AccessKind,
    ) {
        self.bits |= kind.bit();
    }

    /// The set with `kind` added.
    #[must_use]
    pub const fn with(
        self,
        kind: AccessKind,
    ) -> Self {
        Self {
            bits: self.bits | kind.bit(),
        }
    }

    /// The union of both sets.
    #[must_use]
    pub const fn union(
        self,
        other: Self,
    ) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }

    /// The kinds in the set, in the order of [`AccessKind::ALL`].
    pub fn iter(self) -> impl Iterator<Item = AccessKind> {
        AccessKind::ALL
            .into_iter()
            .filter(move |kind| self.contains(*kind))
    }
}

impl From<AccessKind> for AccessKinds {
    fn from(kind: AccessKind) -> Self {
        Self::only(kind)
    }
}

impl FromIterator<AccessKind> for AccessKinds {
    fn from_iter<I: IntoIterator<Item = AccessKind>>(kinds: I) -> Self {
        kinds.into_iter().fold(Self::EMPTY, Self::with)
    }
}

impl BitOr for AccessKinds {
    type Output = Self;

    fn bitor(
        self,
        other: Self,
    ) -> Self {
        self.union(other)
    }
}

impl BitOr<AccessKind> for AccessKinds {
    type Output = Self;

    fn bitor(
        self,
        kind: AccessKind,
    ) -> Self {
        self.with(kind)
    }
}

impl BitOrAssign for AccessKinds {
    fn bitor_assign(
        &mut self,
        other: Self,
    ) {
        *self = self.union(other);
    }
}

impl BitOrAssign<AccessKind> for AccessKinds {
    fn bitor_assign(
        &mut self,
        kind: AccessKind,
    ) {
        self.insert(kind);
    }
}

impl fmt::Display for AccessKinds {
    /// Lists the kinds joined with `+`, such as `read+probe`, or `none`.
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        for (position, kind) in self.iter().enumerate() {
            if position != 0 {
                f.write_str("+")?;
            }
            f.write_str(kind.name())?;
        }
        Ok(())
    }
}

/// The error for a path that must be absolute but is not.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("path is not absolute: {}", .path.display())]
pub struct NotAbsoluteError {
    /// The path that was given.
    pub path: PathBuf,
}

/// An absolute path, normalized as the tracer reports touched paths.
///
/// Normalizing removes `.` components, repeated separators, and a trailing
/// separator. It keeps `..` components, because collapsing them is wrong when an
/// earlier component is a symbolic link. The path is never resolved against the
/// file system.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbsolutePath {
    /// The normalized absolute path.
    path: PathBuf,
}

impl AbsolutePath {
    /// Normalize an absolute path.
    ///
    /// # Errors
    ///
    /// Returns [`NotAbsoluteError`] when `path` is relative.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, NotAbsoluteError> {
        let path = path.as_ref();
        if path.is_absolute() {
            Ok(Self {
                path: path.components().collect(),
            })
        } else {
            Err(NotAbsoluteError {
                path: path.to_path_buf(),
            })
        }
    }

    /// Resolve `path` against this path, as a relative path resolves against a
    /// working directory or a directory descriptor, and normalize the result.
    ///
    /// An absolute `path` replaces this path.
    #[must_use]
    pub fn join(
        &self,
        path: impl AsRef<Path>,
    ) -> Self {
        Self {
            path: self.path.join(path).components().collect(),
        }
    }

    /// The path as a [`Path`].
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// The path as a [`PathBuf`].
    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.path
    }
}

impl AsRef<Path> for AbsolutePath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl TryFrom<PathBuf> for AbsolutePath {
    type Error = NotAbsoluteError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::new(path)
    }
}

impl fmt::Display for AbsolutePath {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.path.display().fmt(f)
    }
}

/// Every path a command's process tree touched, each with its combined access
/// kinds, in sorted path order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Accesses {
    /// The combined access kinds of each touched path.
    paths: BTreeMap<AbsolutePath, AccessKinds>,
}

impl Accesses {
    /// No accesses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one access, combining it with earlier accesses to the same path.
    pub fn record(
        &mut self,
        path: AbsolutePath,
        kind: AccessKind,
    ) {
        self.record_kinds(path, AccessKinds::only(kind));
    }

    /// Record several kinds of access to one path, combining them with earlier
    /// accesses to the same path.
    pub fn record_kinds(
        &mut self,
        path: AbsolutePath,
        kinds: AccessKinds,
    ) {
        *self.paths.entry(path).or_default() |= kinds;
    }

    /// Combine every access of `other` into this set.
    pub fn merge(
        &mut self,
        other: Self,
    ) {
        for (path, kinds) in other.paths {
            self.record_kinds(path, kinds);
        }
    }

    /// The combined access kinds of `path`, if it was touched.
    #[must_use]
    pub fn get(
        &self,
        path: &AbsolutePath,
    ) -> Option<AccessKinds> {
        self.paths.get(path).copied()
    }

    /// The number of touched paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether no path was touched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The touched paths with their combined access kinds, in sorted path order.
    pub fn iter(&self) -> btree_map::Iter<'_, AbsolutePath, AccessKinds> {
        self.paths.iter()
    }
}

impl<'a> IntoIterator for &'a Accesses {
    type IntoIter = btree_map::Iter<'a, AbsolutePath, AccessKinds>;
    type Item = (&'a AbsolutePath, &'a AccessKinds);

    fn into_iter(self) -> Self::IntoIter {
        self.paths.iter()
    }
}

impl IntoIterator for Accesses {
    type IntoIter = btree_map::IntoIter<AbsolutePath, AccessKinds>;
    type Item = (AbsolutePath, AccessKinds);

    fn into_iter(self) -> Self::IntoIter {
        self.paths.into_iter()
    }
}

impl FromIterator<(AbsolutePath, AccessKind)> for Accesses {
    fn from_iter<I: IntoIterator<Item = (AbsolutePath, AccessKind)>>(accesses: I) -> Self {
        let mut combined = Self::new();
        for (path, kind) in accesses {
            combined.record(path, kind);
        }
        combined
    }
}

/// Nanoseconds in a second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// A time in nanoseconds since the Unix epoch from its seconds and nanoseconds.
const fn nanos(
    seconds: i64,
    nanoseconds: i64,
) -> i64 {
    seconds
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(nanoseconds)
}

/// The metadata of a regular file at one moment, as a tracer notes it when a
/// command first reads or runs the file.
///
/// It holds what a change to the file changes: the device and inode that identify
/// it, its size, its modification and change times, its mode, and its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileMetadata {
    /// The device that holds the file.
    pub device: u64,
    /// The file's inode number on its device.
    pub inode: u64,
    /// The size in bytes.
    pub size: u64,
    /// The modification time, in nanoseconds since the Unix epoch.
    pub mtime_nanos: i64,
    /// The change time, in nanoseconds since the Unix epoch.
    pub ctime_nanos: i64,
    /// The mode: the file type and the permission bits.
    pub mode: u32,
    /// The user number of the owner.
    pub uid: u32,
    /// The group number of the owner.
    pub gid: u32,
}

impl FileMetadata {
    /// The metadata that `metadata` describes, or `None` when it does not describe
    /// a regular file.
    #[must_use]
    pub fn of(metadata: &fs::Metadata) -> Option<Self> {
        metadata.file_type().is_file().then(|| {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.size(),
                mtime_nanos: nanos(metadata.mtime(), metadata.mtime_nsec()),
                ctime_nanos: nanos(metadata.ctime(), metadata.ctime_nsec()),
                mode: metadata.mode(),
                uid: metadata.uid(),
                gid: metadata.gid(),
            }
        })
    }

    /// The metadata of the regular file at `path` now, following symbolic links, or
    /// `None` when `path` names no regular file or its metadata cannot be read.
    #[must_use]
    pub fn at(path: &Path) -> Option<Self> {
        fs::metadata(path).ok().as_ref().and_then(Self::of)
    }
}

/// The first reads of a run: for each path whose first read or run of a regular
/// file came before any write of that path in the run, the file's
/// [`FileMetadata`] at that moment, in sorted path order.
///
/// A tracer notes a file when the command opens it for reading or runs it, and the
/// kernel's loads of interpreters and loaders count as runs. It notes nothing for
/// a path that the command wrote earlier in the run, for a path it already noted,
/// and for a call that created or truncated the file, which writes it first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FirstReads {
    /// The first-read metadata of each noted path.
    files: BTreeMap<AbsolutePath, FileMetadata>,
}

impl FirstReads {
    /// No first reads.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a read or run of `path` now would be its first read: the path has
    /// no note yet, and `accesses`, every access recorded earlier in the run, shows
    /// no write of it.
    #[must_use]
    pub fn is_first(
        &self,
        path: &AbsolutePath,
        accesses: &Accesses,
    ) -> bool {
        !self.files.contains_key(path)
            && !accesses
                .get(path)
                .is_some_and(|kinds| kinds.contains(AccessKind::Write))
    }

    /// Note `metadata` as the first-read metadata of `path`, which the command is
    /// reading or running now, when that is its first read by
    /// [`FirstReads::is_first`]; return whether it was noted.
    pub fn note(
        &mut self,
        path: &AbsolutePath,
        metadata: FileMetadata,
        accesses: &Accesses,
    ) -> bool {
        if !self.is_first(path, accesses) {
            return false;
        }
        self.files.insert(path.clone(), metadata);
        true
    }

    /// Note the metadata of the regular file at `path`, read now with
    /// [`FileMetadata::at`], under the rules of [`FirstReads::note`]; nothing is
    /// noted when `path` names no regular file. Return whether it was noted.
    ///
    /// A tracer calls it when the command reads or runs `path`, before it records
    /// that access in `accesses`.
    pub fn note_at(
        &mut self,
        path: &AbsolutePath,
        accesses: &Accesses,
    ) -> bool {
        if !self.is_first(path, accesses) {
            return false;
        }
        FileMetadata::at(path.as_path()).is_some_and(|metadata| self.note(path, metadata, accesses))
    }

    /// The first-read metadata of `path`, if it was noted.
    #[must_use]
    pub fn get(
        &self,
        path: &AbsolutePath,
    ) -> Option<&FileMetadata> {
        self.files.get(path)
    }

    /// The number of noted paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether no path was noted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The noted paths with their first-read metadata, in sorted path order.
    pub fn iter(&self) -> btree_map::Iter<'_, AbsolutePath, FileMetadata> {
        self.files.iter()
    }
}

impl<'a> IntoIterator for &'a FirstReads {
    type IntoIter = btree_map::Iter<'a, AbsolutePath, FileMetadata>;
    type Item = (&'a AbsolutePath, &'a FileMetadata);

    fn into_iter(self) -> Self::IntoIter {
        self.files.iter()
    }
}

/// How a command's standard output and standard error are captured.
///
/// Output is always captured for a request that does not keep the terminal; the
/// mode only decides how. A request that keeps the terminal
/// ([`RunRequest::terminal`]) captures nothing, and its output is only the empty
/// output in the shape of the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CaptureMode {
    /// Standard output and standard error are captured separately.
    #[default]
    Split,
    /// Both streams go through one pipe, so their order is exact.
    Merged,
}

/// One command for a [`Tracer`] to run.
///
/// The request is exact: the tracer adds nothing to it and looks nothing up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The exact path of the program to run. The engine has already resolved a
    /// bare program name through the task's `PATH`.
    pub program: AbsolutePath,
    /// The argument vector the program receives, starting with its `argv[0]`.
    pub args: Vec<OsString>,
    /// The exact environment of the process; nothing else is inherited.
    pub env: BTreeMap<OsString, OsString>,
    /// The absolute working directory of the process.
    pub cwd: AbsolutePath,
    /// The content written to standard input, or `None` to connect `/dev/null`;
    /// a request that keeps the terminal ([`RunRequest::terminal`]) connects
    /// neither, and its command reads the standard input of the process that runs
    /// the tracer.
    pub stdin: Option<Vec<u8>>,
    /// How standard output and standard error are captured.
    pub capture: CaptureMode,
    /// Whether output is also echoed live, through the [`EchoSink`] given to
    /// [`Tracer::run`], while it is captured.
    pub echo: bool,
    /// Whether the result also holds the [`ProcessTree`] of the run, and a failed
    /// start the tree of its one process. Off unless set: builds never set it, so the
    /// tracer does not read the arguments of `exec` calls or keep each program's
    /// observations for them.
    pub tree: bool,
    /// Whether the command keeps the terminal: it uses the standard input, output
    /// and error of the process that runs the tracer, and stays in that process's
    /// process group and session, so a terminal's input and signals reach it as they
    /// would without kizz. Nothing is captured or echoed, the result's output is
    /// empty, and [`RunRequest::stdin`], [`RunRequest::capture`] and
    /// [`RunRequest::echo`] mean nothing: the output stays in the empty shape of the
    /// capture mode. Off unless set: builds never set it, so a command of a build
    /// runs in a process group of its own, with pipes or `/dev/null` as its
    /// standard streams, and a terminal's Ctrl-C reaches only kizz.
    pub terminal: bool,
}

/// The stream a chunk of live output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputStream {
    /// Standard output, with [`CaptureMode::Split`].
    Stdout,
    /// Standard error, with [`CaptureMode::Split`].
    Stderr,
    /// The one interleaved stream of [`CaptureMode::Merged`].
    Merged,
}

/// Where live output goes when a request asks for echo.
///
/// The tracer calls [`EchoSink::echo`] with each chunk as it arrives, in order,
/// and only when [`RunRequest::echo`] is set. Tests observe echo by implementing
/// this trait.
pub trait EchoSink: Send + Sync {
    /// Echo one chunk of output.
    ///
    /// # Errors
    ///
    /// Returns the error of the underlying write. A failed echo never fails the
    /// run, and the captured output stays complete.
    fn echo(
        &self,
        stream: OutputStream,
        bytes: &[u8],
    ) -> io::Result<()>;
}

/// Echoes live output to the terminal: standard error to standard error, and
/// everything else to standard output.
///
/// It writes with `Write::write_all` and `Write::flush` on the locked stream, which
/// return the error of a write that fails, such as one to a closed pipe, where the
/// `print!` macros would panic ("Kizz never panics"); the tracer drops that error,
/// so the run goes on and its captured output stays complete.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdioEcho;

impl EchoSink for StdioEcho {
    fn echo(
        &self,
        stream: OutputStream,
        bytes: &[u8],
    ) -> io::Result<()> {
        match stream {
            OutputStream::Stderr => {
                let mut stderr = io::stderr().lock();
                stderr.write_all(bytes)?;
                stderr.flush()
            },
            OutputStream::Stdout | OutputStream::Merged => {
                let mut stdout = io::stdout().lock();
                stdout.write_all(bytes)?;
                stdout.flush()
            },
        }
    }
}

/// A termination signal that the caller can forward to a running request, with
/// [`Cancellation::forward`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Signal {
    /// The hangup signal, `SIGHUP`.
    Hangup,
    /// The termination signal, `SIGTERM`.
    Terminate,
}

impl Signal {
    /// Every signal that can be forwarded, in the order the tracer delivers those
    /// that are pending together.
    pub const ALL: [Self; 2] = [Self::Hangup, Self::Terminate];

    /// The bit of the signal in the pending set of a [`Cancellation`].
    const fn bit(self) -> u8 {
        match self {
            Self::Hangup => 0b01,
            Self::Terminate => 0b10,
        }
    }
}

impl fmt::Display for Signal {
    /// The signal's name, `SIGHUP` or `SIGTERM`.
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(match self {
            Self::Hangup => "SIGHUP",
            Self::Terminate => "SIGTERM",
        })
    }
}

/// A shared handle that cancels a running request, or forwards signals to it.
///
/// Clones share the handle, so the engine keeps one clone and hands another to the
/// tracer. When the flag is set, the tracer interrupts the request's process tree,
/// kills whatever remains of it after a grace period, and returns its result once
/// the whole tree has exited. The result reports how the command ended: by a
/// signal, or with an exit code that the command chose after it handled the
/// interrupt.
///
/// The caller can also forward a [`Signal`] with [`Cancellation::forward`], as
/// `kizz execute` forwards the `SIGTERM` and `SIGHUP` that kizz receives. A tracer
/// takes each forwarded signal with [`Cancellation::take_forwarded`] while the
/// request runs, and delivers it once: to the command's first process while that
/// process lives, and afterwards to each live process of the tree. A signal that
/// is forwarded again before the tracer takes it is delivered once, as a pending
/// signal of the operating system is, and one forwarded while no request runs
/// waits for the next request that holds a clone. Once the request is cancelled,
/// the tracer forwards nothing more. Builds never forward a signal.
///
/// A signal sent to the whole process group of a command that keeps the terminal
/// ([`RunRequest::terminal`]) reaches the command directly, and once more when the
/// caller, which received it too, forwards it: neither the caller nor the tracer can
/// tell such a signal from one sent to the caller alone.
///
/// The handle uses only atomic operations, so the caller can forward a signal
/// from anywhere, a signal handler included.
#[derive(Debug, Clone, Default)]
pub struct Cancellation {
    /// Whether cancellation was requested.
    cancelled: Arc<AtomicBool>,
    /// The forwarded signals that the tracer has not taken yet, one bit for each
    /// [`Signal`], as `Signal::bit` assigns them.
    forwarded: Arc<AtomicU8>,
}

impl Cancellation {
    /// A flag that is not set, with no signal forwarded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation of every request that holds a clone of this flag.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Forward `signal` to the request that holds a clone of this handle, for its
    /// tracer to deliver once, as the type's documentation says.
    pub fn forward(
        &self,
        signal: Signal,
    ) {
        self.forwarded.fetch_or(signal.bit(), Ordering::SeqCst);
    }

    /// Take every signal forwarded since the last call, in the order of
    /// [`Signal::ALL`], so each one is delivered once.
    #[must_use]
    pub fn take_forwarded(&self) -> Vec<Signal> {
        let pending = self.forwarded.swap(0, Ordering::SeqCst);
        Signal::ALL
            .into_iter()
            .filter(|signal| pending & signal.bit() != 0)
            .collect()
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Termination {
    /// The command exited with this exit code.
    Exited(i32),
    /// A signal with this number killed the command.
    Signaled(i32),
}

impl Termination {
    /// The exit code, or `None` when a signal killed the command.
    #[must_use]
    pub const fn code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            Self::Signaled(_) => None,
        }
    }

    /// The terminating signal, or `None` when the command exited.
    #[must_use]
    pub const fn signal(self) -> Option<i32> {
        match self {
            Self::Exited(_) => None,
            Self::Signaled(signal) => Some(signal),
        }
    }
}

impl fmt::Display for Termination {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Exited(code) => write!(f, "exit code {code}"),
            Self::Signaled(signal) => write!(f, "killed by signal {signal}"),
        }
    }
}

/// A command's raw captured output, exactly as the process wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedOutput {
    /// Output captured with [`CaptureMode::Split`].
    Split {
        /// The bytes written to standard output.
        stdout: Vec<u8>,
        /// The bytes written to standard error.
        stderr: Vec<u8>,
    },
    /// Output captured with [`CaptureMode::Merged`]: both streams, in exact order.
    Merged {
        /// The bytes written to either stream.
        output: Vec<u8>,
    },
}

impl CapturedOutput {
    /// No output yet, in the shape of `mode`.
    #[must_use]
    pub const fn empty(mode: CaptureMode) -> Self {
        match mode {
            CaptureMode::Split => {
                Self::Split {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                }
            },
            CaptureMode::Merged => Self::Merged { output: Vec::new() },
        }
    }

    /// The capture mode this output was captured with.
    #[must_use]
    pub const fn mode(&self) -> CaptureMode {
        match self {
            Self::Split { .. } => CaptureMode::Split,
            Self::Merged { .. } => CaptureMode::Merged,
        }
    }
}

/// Why a run was traced incompletely.
///
/// An incompletely traced run is never cached, and kizz warns loudly about it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IncompleteReason {
    /// A process made a system call through a non-native interface, such as the
    /// 32-bit `int 0x80` interface or x32, which the tracer does not decode.
    NonNativeSystemCall {
        /// A description of the interface and the call.
        detail: String,
    },
    /// A process made a system call that can change what a path means, such as
    /// `chroot`, `mount` or a change of mount namespace, or that uses a path in a way
    /// the tracer does not record, such as `uselib` or `swapon`.
    UntraceableSystemCall {
        /// The process and the call, named in backticks.
        detail: String,
    },
    /// Any other reason the tracer could not observe every access.
    Other {
        /// A description of what the tracer could not observe.
        detail: String,
    },
}

impl fmt::Display for IncompleteReason {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::NonNativeSystemCall { detail } => {
                write!(f, "a system call used a non-native interface: {detail}")
            },
            Self::UntraceableSystemCall { detail } => {
                write!(f, "a system call that kizz cannot trace: {detail}")
            },
            Self::Other { detail } => f.write_str(detail),
        }
    }
}

/// A connection that a process of the run made to a service outside the trace,
/// whose answer is an input that the tracer cannot see.
///
/// Connections are ordered by their kind, then by address or path, so a set of
/// them is sorted and without repeats.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Connection {
    /// An IPv4 or IPv6 address and port. An IPv6 address keeps its scope, and its
    /// flow information is zero, because it does not name a destination.
    Ip(SocketAddr),
    /// A Unix socket by its path, absolute and normalized like every recorded
    /// path.
    UnixPath(AbsolutePath),
    /// A Unix socket in the abstract namespace, by its name without the zero byte
    /// that starts it; the name may hold more zero bytes.
    UnixAbstract(Vec<u8>),
}

impl fmt::Display for Connection {
    /// An IP address with its port, such as `127.0.0.1:80` or `[fe80::1%2]:80`;
    /// `Unix socket /path`; or `abstract Unix socket @name`, with each byte of the
    /// name that is not printable ASCII escaped, such as `\x00`.
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Ip(address) => write!(f, "{address}"),
            Self::UnixPath(path) => write!(f, "Unix socket {path}"),
            Self::UnixAbstract(name) => {
                write!(f, "abstract Unix socket @{}", name.escape_ascii())
            },
        }
    }
}

/// The answer of [`Tracer::error_name`] for an error number that has no name, and
/// its default answer for every number: `errno` and the number, such as
/// `errno 200`.
#[must_use]
pub fn unnamed_error(errno: i32) -> String {
    format!("errno {errno}")
}

/// How the calls that touched a path in one way ended.
///
/// Outcomes sort with [`Outcome::Ok`] first, then the errors in the byte order of
/// their names, then [`Outcome::Unknown`], so a set of them lists them in that
/// order, each once.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// The call succeeded.
    Ok,
    /// The call failed with the error of this name, such as `ENOENT`, or with an
    /// error without a name, written as [`unnamed_error`] writes it.
    Error(String),
    /// The tracer never saw the call's result, because its thread was killed during
    /// the call.
    Unknown,
}

impl fmt::Display for Outcome {
    /// `ok`, the error's name, or `unknown`.
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Error(name) => f.write_str(name),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// The outcomes of one access kind of one path, in the order of [`Outcome`].
pub type Outcomes = BTreeSet<Outcome>;

/// The access kinds of one observed path, each with its outcomes, in the order of
/// [`AccessKind::ALL`].
pub type KindOutcomes = BTreeMap<AccessKind, Outcomes>;

/// The paths that a program touched, each with its access kinds and each kind with
/// its outcomes, in sorted path order.
///
/// Recording and merging are set unions: a path keeps every kind recorded for it,
/// and a kind every outcome, each once, whatever the order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observations {
    /// The kinds and outcomes of each observed path.
    paths: BTreeMap<AbsolutePath, KindOutcomes>,
}

impl Observations {
    /// No observations.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The observations of every path of `accesses`, each of its kinds with
    /// `outcome` alone.
    #[must_use]
    pub fn from_accesses(
        accesses: &Accesses,
        outcome: &Outcome,
    ) -> Self {
        let mut observations = Self::new();
        for (path, kinds) in accesses {
            observations.record_kinds(path.clone(), *kinds, outcome);
        }
        observations
    }

    /// Record that a call with `outcome` touched `path` in the way `kind` names.
    pub fn record(
        &mut self,
        path: AbsolutePath,
        kind: AccessKind,
        outcome: Outcome,
    ) {
        self.paths
            .entry(path)
            .or_default()
            .entry(kind)
            .or_default()
            .insert(outcome);
    }

    /// Record that a call with `outcome` touched `path` in every way of `kinds`.
    /// The path is observed even when `kinds` is empty, as [`Accesses::record_kinds`]
    /// records it.
    pub fn record_kinds(
        &mut self,
        path: AbsolutePath,
        kinds: AccessKinds,
        outcome: &Outcome,
    ) {
        let entry = self.paths.entry(path).or_default();
        for kind in kinds.iter() {
            entry.entry(kind).or_default().insert(outcome.clone());
        }
    }

    /// Combine every observation of `other` into these.
    pub fn merge(
        &mut self,
        other: &Self,
    ) {
        for (path, kinds) in &other.paths {
            let entry = self.paths.entry(path.clone()).or_default();
            for (kind, outcomes) in kinds {
                entry
                    .entry(*kind)
                    .or_default()
                    .extend(outcomes.iter().cloned());
            }
        }
    }

    /// The kinds of `path`, each with its outcomes, if it was observed.
    #[must_use]
    pub fn get(
        &self,
        path: &AbsolutePath,
    ) -> Option<&KindOutcomes> {
        self.paths.get(path)
    }

    /// The combined access kinds of `path`, if it was observed.
    #[must_use]
    pub fn kinds(
        &self,
        path: &AbsolutePath,
    ) -> Option<AccessKinds> {
        self.paths
            .get(path)
            .map(|kinds| kinds.keys().copied().collect())
    }

    /// The number of observed paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether no path was observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The observed paths with their kinds and outcomes, in sorted path order.
    pub fn iter(&self) -> btree_map::Iter<'_, AbsolutePath, KindOutcomes> {
        self.paths.iter()
    }

    /// Every observed path with its combined access kinds, without the outcomes.
    #[must_use]
    pub fn accesses(&self) -> Accesses {
        let mut accesses = Accesses::new();
        for (path, kinds) in &self.paths {
            accesses.record_kinds(path.clone(), kinds.keys().copied().collect());
        }
        accesses
    }
}

impl<'a> IntoIterator for &'a Observations {
    type IntoIter = btree_map::Iter<'a, AbsolutePath, KindOutcomes>;
    type Item = (&'a AbsolutePath, &'a KindOutcomes);

    fn into_iter(self) -> Self::IntoIter {
        self.paths.iter()
    }
}

/// One program that a traced process ran, with everything the tracer attributes to
/// it.
///
/// A program is the path that an `exec` call named, resolved as its observation is,
/// with the arguments that the call passed; a process that a fork, a vfork or a
/// clone created first runs its creator's program, which it lists as inherited.
/// Everything the process does while it runs the program belongs to the program,
/// and so does a successful `exec` that starts it: its own path, its interpreters
/// and its ELF loader. A failed `exec` belongs to the program that made the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// The path of the program.
    pub path: AbsolutePath,
    /// The argument vector that the program received, starting with its
    /// `argv[0]`, or empty when the tracer could not read it.
    pub args: Vec<OsString>,
    /// Whether the process runs the program because its creator ran it, rather than
    /// through an `exec` of its own.
    pub inherited: bool,
    /// The paths that the program touched, with their kinds and outcomes.
    pub observations: Observations,
    /// The connections that the program made.
    pub connections: BTreeSet<Connection>,
    /// The native numbers of the system calls that the program made and that the
    /// tracer does not know.
    pub unknown_system_calls: BTreeSet<u32>,
    /// Every reason that the program made the trace incomplete, in the order the
    /// tracer found them.
    pub incomplete: Vec<IncompleteReason>,
}

impl Program {
    /// The program at `path` with `args`, inherited or not, to which nothing is
    /// attributed yet.
    #[must_use]
    pub const fn new(
        path: AbsolutePath,
        args: Vec<OsString>,
        inherited: bool,
    ) -> Self {
        Self {
            path,
            args,
            inherited,
            observations: Observations {
                paths: BTreeMap::new(),
            },
            connections: BTreeSet::new(),
            unknown_system_calls: BTreeSet::new(),
            incomplete: Vec::new(),
        }
    }

    /// Whether nothing is attributed to the program: no observation, connection,
    /// unknown system call or incomplete reason.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
            && self.connections.is_empty()
            && self.unknown_system_calls.is_empty()
            && self.incomplete.is_empty()
    }
}

/// One traced process: a thread group, whose threads fold into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// The number of the process, unique within the run: processes are numbered
    /// from 1 in the order the tracer saw them start, and process 1 is the command.
    pub number: u32,
    /// The process ID that the system gave it, or `None` when none is known, as
    /// for a fake tracer or a command whose process never started.
    pub pid: Option<u32>,
    /// The number of the process that created it, or `None` for the command and
    /// for a process whose creation the tracer never saw.
    pub parent: Option<u32>,
    /// The programs it ran, in order.
    pub programs: Vec<Program>,
    /// How it ended, or `None` when the tracer did not see it end, or the command
    /// could not start.
    pub status: Option<Termination>,
}

/// The processes of a run, in the order the tracer saw them start, each with its
/// programs and their observations.
///
/// A process ID that the system gives to a new process within the run starts a new
/// process, so two processes never merge. The kinds of each path, combined over
/// every program, are exactly its kinds in the run's combined accesses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessTree {
    /// The processes, numbered from 1, in order.
    pub processes: Vec<Process>,
}

impl ProcessTree {
    /// The tree of one process, the command of `request`, without a process ID,
    /// whose one program is the request's program with its arguments, which
    /// touched every path of `accesses` with its kinds, each with the outcome
    /// `ok`, and which ended as `status` says. Fake tracers build their trees
    /// with it.
    #[must_use]
    pub fn of_request(
        request: &RunRequest,
        accesses: &Accesses,
        status: Option<Termination>,
    ) -> Self {
        let mut program = Program::new(request.program.clone(), request.args.clone(), false);
        program.observations = Observations::from_accesses(accesses, &Outcome::Ok);
        Self {
            processes: vec![Process {
                number: 1,
                pid: None,
                parent: None,
                programs: vec![program],
                status,
            }],
        }
    }

    /// The tree of one process, the command of `request`, built from the run's
    /// whole `result`, as [`ProcessTree::of_request`] builds it from the result's
    /// accesses and termination, with the result's reason that the trace is
    /// incomplete, when it has one, its unknown system calls and its connections
    /// on the one program, where the Linux tracer puts them: on the program that
    /// made them. So the tree carries everything that the run reports, and a report
    /// built from it says what the run says. Fake tracers whose runs a test builds a
    /// report from build their trees with it; the tree of a failed start, which
    /// reports none of these, keeps the shape that `of_request` gives it.
    #[must_use]
    pub fn of_run(
        request: &RunRequest,
        result: &RunResult,
    ) -> Self {
        let mut tree = Self::of_request(request, &result.accesses, Some(result.termination));
        for process in &mut tree.processes {
            for program in &mut process.programs {
                program.incomplete.extend(result.incomplete.iter().cloned());
                program
                    .unknown_system_calls
                    .extend(result.unknown_system_calls.iter().copied());
                program
                    .connections
                    .extend(result.connections.iter().cloned());
            }
        }
        tree
    }

    /// The process numbered `number`, if there is one.
    #[must_use]
    pub fn process(
        &self,
        number: u32,
    ) -> Option<&Process> {
        self.processes
            .iter()
            .find(|process| process.number == number)
    }

    /// Every program of every process, with its process, in order.
    pub fn programs(&self) -> impl Iterator<Item = (&Process, &Program)> {
        self.processes.iter().flat_map(|process| {
            process
                .programs
                .iter()
                .map(move |program| (process, program))
        })
    }

    /// The observations of every program, combined.
    #[must_use]
    pub fn observations(&self) -> Observations {
        let mut combined = Observations::new();
        for (_, program) in self.programs() {
            combined.merge(&program.observations);
        }
        combined
    }

    /// The access kinds of every path of every program, combined, without the
    /// outcomes.
    #[must_use]
    pub fn accesses(&self) -> Accesses {
        self.observations().accesses()
    }

    /// The connections of every program, combined.
    #[must_use]
    pub fn connections(&self) -> BTreeSet<Connection> {
        self.programs()
            .flat_map(|(_, program)| program.connections.iter().cloned())
            .collect()
    }

    /// The unknown system calls of every program, combined.
    #[must_use]
    pub fn unknown_system_calls(&self) -> BTreeSet<u32> {
        self.programs()
            .flat_map(|(_, program)| program.unknown_system_calls.iter().copied())
            .collect()
    }
}

/// The result of a request that started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// How the command ended.
    pub termination: Termination,
    /// The raw captured output, in the shape of the request's capture mode.
    pub output: CapturedOutput,
    /// Every path the process tree touched, absolute and normalized, with its
    /// combined access kinds.
    pub accesses: Accesses,
    /// Why the trace is incomplete, or `None` when every access was observed.
    pub incomplete: Option<IncompleteReason>,
    /// The first-read metadata of the regular files the process tree read or ran
    /// before it wrote them, as [`FirstReads`] describes.
    pub first_reads: FirstReads,
    /// The native numbers of the system calls that the tracer does not know, as
    /// the filter saw them, which a process of the tree made, sorted and without
    /// repeats. They do not make the trace incomplete: the engine decides whether a
    /// run that made any is cached.
    pub unknown_system_calls: BTreeSet<u32>,
    /// The connections that a process of the tree made, sorted and without
    /// repeats, as [`Connection`] describes them. They do not make the trace
    /// incomplete: the engine records them and warns about them.
    pub connections: BTreeSet<Connection>,
    /// The process tree of the run when the request asked for it with
    /// [`RunRequest::tree`], and `None` otherwise.
    pub tree: Option<ProcessTree>,
}

impl RunResult {
    /// Whether the tracer observed every access of the run.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.incomplete.is_none()
    }
}

/// A request that did not produce a [`RunResult`].
///
/// The variants keep three cases apart, because the engine treats them
/// differently: a missing or non-executable program fails its task and is recorded
/// like any other failure; another failure to start is not cached; and a failure
/// of the tracer itself is neither the command's fault nor cached.
#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    /// The program does not exist.
    #[error("program not found: {program}")]
    ProgramNotFound {
        /// The program of the request.
        program: AbsolutePath,
        /// The paths touched while trying to start the program.
        accesses: Accesses,
        /// The tree of the one process that tried to start the program, when the
        /// request asked for it with [`RunRequest::tree`].
        tree: Option<ProcessTree>,
    },
    /// The program exists but cannot be executed.
    #[error("program is not executable: {program}: {source}")]
    ProgramNotExecutable {
        /// The program of the request.
        program: AbsolutePath,
        /// The error that starting the program reported.
        source: io::Error,
        /// The paths touched while trying to start the program.
        accesses: Accesses,
        /// The tree of the one process that tried to start the program, when the
        /// request asked for it with [`RunRequest::tree`].
        tree: Option<ProcessTree>,
    },
    /// The command could not start for a reason other than a missing or
    /// non-executable program.
    #[error("could not start {program}: {source}")]
    StartFailed {
        /// The program of the request.
        program: AbsolutePath,
        /// The error that starting the program reported.
        source: io::Error,
        /// The paths touched while trying to start the program.
        accesses: Accesses,
        /// The tree of the one process that tried to start the program, when the
        /// request asked for it with [`RunRequest::tree`].
        tree: Option<ProcessTree>,
    },
    /// The tracer itself failed, independently of the command.
    #[error("tracer failure: {message}")]
    Tracer {
        /// What the tracer was doing when it failed.
        message: String,
        /// The underlying error, if there is one.
        source: Option<io::Error>,
    },
}

impl TraceError {
    /// Whether the program is missing or not executable, the one failure to start
    /// that is recorded like any other task failure.
    #[must_use]
    pub const fn is_missing_program(&self) -> bool {
        matches!(
            self,
            Self::ProgramNotFound { .. } | Self::ProgramNotExecutable { .. }
        )
    }

    /// Whether the tracer itself failed, rather than the command.
    #[must_use]
    pub const fn is_tracer_failure(&self) -> bool {
        matches!(self, Self::Tracer { .. })
    }

    /// The paths touched while trying to start the program, if the error is a
    /// failure to start.
    #[must_use]
    pub const fn accesses(&self) -> Option<&Accesses> {
        match self {
            Self::ProgramNotFound { accesses, .. }
            | Self::ProgramNotExecutable { accesses, .. }
            | Self::StartFailed { accesses, .. } => Some(accesses),
            Self::Tracer { .. } => None,
        }
    }

    /// The tree of the one process that tried to start the program, if the error is
    /// a failure to start and the request asked for the tree.
    #[must_use]
    pub const fn tree(&self) -> Option<&ProcessTree> {
        match self {
            Self::ProgramNotFound { tree, .. }
            | Self::ProgramNotExecutable { tree, .. }
            | Self::StartFailed { tree, .. } => tree.as_ref(),
            Self::Tracer { .. } => None,
        }
    }
}

/// The kernel that runs a command, as `uname` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Kernel {
    /// The kernel release, such as `6.12.0`.
    pub release: String,
    /// The kernel version, such as `#1 SMP PREEMPT_DYNAMIC`.
    pub version: String,
}

/// One resource limit of a process, soft and hard.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceLimit {
    /// The resource, as `prlimit` names it, such as `nofile`.
    pub resource: String,
    /// The soft limit, or `None` when it is unlimited.
    pub soft: Option<u64>,
    /// The hard limit, or `None` when it is unlimited.
    pub hard: Option<u64>,
}

/// The memory of the machine, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Memory {
    /// The total memory.
    pub total: u64,
    /// The memory available to start new work without swapping.
    pub available: u64,
}

/// The file system that holds a working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileSystem {
    /// The type of the file system, as the platform numbers it.
    pub kind: u64,
    /// The mount flags, as the platform numbers them.
    pub flags: u64,
}

/// The size and free space of the file system that holds a working directory, in
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Disk {
    /// The size of the file system.
    pub size: u64,
    /// The free space that a process without privileges may use.
    pub free: u64,
}

/// The machine and process state that a command inherits when it starts, in six
/// parts. A part that could not be read is `None`.
///
/// Kizz records it with every step that runs, and compares only the parts that a
/// step names with `run(..., machine = [...])`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct MachineState {
    /// `kernel`: the kernel release and version.
    pub kernel: Option<Kernel>,
    /// `limits`: every resource limit, soft and hard, in the platform's order.
    pub limits: Option<Vec<ResourceLimit>>,
    /// `affinity`: the CPUs the command may run on, by number, sorted.
    pub affinity: Option<Vec<u32>>,
    /// `memory`: the total and the available memory.
    pub memory: Option<Memory>,
    /// `filesystem`: the type and mount flags of the file system that holds the
    /// working directory.
    pub filesystem: Option<FileSystem>,
    /// `disk`: the size and free space of that file system.
    pub disk: Option<Disk>,
}

/// The extended attributes of one path: each name with its value, as bytes, sorted
/// by name.
pub type ExtendedAttributes = BTreeMap<Vec<u8>, Vec<u8>>;

/// Runs and traces commands, and answers the platform queries of the core.
///
/// The Linux tracer implements it with `ptrace` and seccomp, and tests implement it
/// with a fake that performs scripted file operations and reports them, and that
/// answers the queries from values the test sets.
pub trait Tracer: Send + Sync {
    /// Run one command, wait for its whole process tree to finish, and report what
    /// it did.
    ///
    /// When [`RunRequest::echo`] is set, output also goes to `echo` live. When
    /// `cancel` is set while the command runs, the tracer interrupts the whole
    /// process tree, kills whatever remains of it after a grace period, and returns
    /// the result once the tree has exited; the command may then have ended by a
    /// signal or with an exit code. A signal that the caller forwards through
    /// `cancel` with [`Cancellation::forward`] while the command runs goes once to
    /// the command's first process while it lives, and afterwards to each live
    /// process of the tree; a fake tracer may ignore it. When
    /// [`RunRequest::terminal`] is set, the command keeps the terminal and the
    /// result's output is empty.
    ///
    /// # Errors
    ///
    /// Returns a [`TraceError`] when the command could not start or the tracer
    /// failed.
    fn run(
        &self,
        request: &RunRequest,
        echo: &dyn EchoSink,
        cancel: &Cancellation,
    ) -> Result<RunResult, TraceError>;

    /// Run a command without tracing its dependencies. The engine never reuses its
    /// result. Implementations must preserve the request's environment, streams and
    /// cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error when untraced execution is unsupported or the command cannot
    /// start.
    fn run_untraced(
        &self,
        _request: &RunRequest,
        _echo: &dyn EchoSink,
        _cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        Err(TraceError::Tracer {
            message: "this runner does not support untraced execution".to_owned(),
            source: None,
        })
    }

    /// The extended attributes of `path`, each name with its value, sorted by name,
    /// following a final symbolic link when `follow` is set, or not when it is not.
    /// A file system that keeps no extended attributes has none.
    ///
    /// The core reads extended attributes only through this query, because the
    /// standard library cannot read them.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the attributes, such as
    /// [`io::ErrorKind::NotFound`] when nothing is at the path.
    fn extended_attributes(
        &self,
        path: &Path,
        follow: bool,
    ) -> io::Result<ExtendedAttributes>;

    /// The machine and process state that a command started in `cwd` inherits: the
    /// kernel, the resource limits, the CPUs it may run on, the memory, and the file
    /// system that holds `cwd`, with its size and free space. A part that cannot be
    /// read is `None`.
    ///
    /// The core reads machine state only through this query, because the standard
    /// library cannot read it.
    fn machine_state(
        &self,
        cwd: &Path,
    ) -> MachineState;

    /// The name of the error number `errno` on the platform, such as `ENOENT`, which
    /// the core uses to report a failed check of its own as the tracer reports a
    /// failed call.
    ///
    /// The default answer is [`unnamed_error`], `errno` and the number, so a tracer
    /// that knows no names, such as a fake, needs no implementation.
    fn error_name(
        &self,
        errno: i32,
    ) -> String {
        unnamed_error(errno)
    }
}

/// Unit tests for the logic of the tracer interface.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::OsString,
        fs, io,
        path::Path,
    };

    use super::{
        AbsolutePath, AccessKind, AccessKinds, Accesses, Cancellation, CaptureMode, CapturedOutput,
        Connection, EchoSink, ExtendedAttributes, FileMetadata, FirstReads, IncompleteReason,
        MachineState, NotAbsoluteError, Observations, Outcome, Process, ProcessTree, Program,
        RunRequest, RunResult, Signal, Termination, TraceError, Tracer, unnamed_error,
    };

    /// Build an absolute path for a test.
    fn path(text: &str) -> AbsolutePath {
        AbsolutePath::new(text).unwrap()
    }

    /// Combining kinds is a set union that ignores order and repetition.
    #[test]
    fn access_kinds_combine_as_a_set() {
        let mut kinds = AccessKinds::EMPTY;
        assert!(kinds.is_empty() && kinds.iter().next().is_none());
        kinds.insert(AccessKind::Write);
        kinds |= AccessKind::Read;
        kinds |= AccessKind::Write;
        assert!(kinds.contains(AccessKind::Read));
        assert!(kinds.contains(AccessKind::Write));
        assert!(!kinds.contains(AccessKind::Probe));
        assert_eq!(kinds.iter().collect::<Vec<_>>(), vec![
            AccessKind::Read,
            AccessKind::Write
        ]);

        let reversed: AccessKinds = [AccessKind::Write, AccessKind::Read, AccessKind::Read]
            .into_iter()
            .collect();
        assert_eq!(kinds, reversed);
        assert_eq!(
            AccessKinds::only(AccessKind::Read) | AccessKinds::only(AccessKind::Write),
            kinds
        );
        assert_eq!(kinds.union(AccessKinds::EMPTY), kinds);
        assert_eq!(
            AccessKinds::from(AccessKind::Exec),
            AccessKinds::only(AccessKind::Exec)
        );
    }

    /// Every kind has its own bit, so the full set holds all six.
    #[test]
    fn every_access_kind_is_distinct() {
        let all: AccessKinds = AccessKind::ALL.into_iter().collect();
        assert_eq!(all.iter().collect::<Vec<_>>(), AccessKind::ALL.to_vec());
        for kind in AccessKind::ALL {
            assert_eq!(AccessKinds::only(kind).iter().collect::<Vec<_>>(), vec![
                kind
            ]);
        }
    }

    /// Kinds display by name, joined with `+`.
    #[test]
    fn access_kinds_display() {
        assert_eq!(AccessKinds::EMPTY.to_string(), "none");
        assert_eq!(AccessKind::List.to_string(), "list");
        let kinds = AccessKinds::only(AccessKind::Exec)
            .with(AccessKind::Probe)
            .with(AccessKind::Read);
        assert_eq!(kinds.to_string(), "read+probe+exec");
    }

    /// The extended-attribute kind is named `xattr`, comes after `exec`, and has its
    /// own bit.
    #[test]
    fn the_xattr_kind_follows_exec() {
        assert_eq!(AccessKind::Xattr.to_string(), "xattr");
        assert_eq!(AccessKind::ALL.last(), Some(&AccessKind::Xattr));
        let kinds = AccessKinds::only(AccessKind::Xattr).with(AccessKind::Exec);
        assert_eq!(kinds.to_string(), "exec+xattr");
        assert!(!AccessKinds::only(AccessKind::Exec).contains(AccessKind::Xattr));
    }

    /// Accesses to the same path combine into one entry, in sorted path order.
    #[test]
    fn accesses_combine_per_path() {
        let mut accesses = Accesses::new();
        assert!(accesses.is_empty() && accesses.iter().next().is_none());
        accesses.record(path("/b"), AccessKind::Read);
        accesses.record(path("/a"), AccessKind::Probe);
        accesses.record(path("/b/"), AccessKind::Write);
        accesses.record(path("/a"), AccessKind::Probe);
        assert_eq!(accesses.len(), 2);
        assert_eq!(
            accesses.get(&path("/b")),
            Some(AccessKinds::only(AccessKind::Read).with(AccessKind::Write))
        );
        assert_eq!(accesses.get(&path("/c")), None);
        let order: Vec<String> = accesses.iter().map(|(path, _)| path.to_string()).collect();
        assert_eq!(order, vec!["/a", "/b"]);

        let mut other = Accesses::new();
        other.record(path("/a"), AccessKind::List);
        other.record(path("/c"), AccessKind::Exec);
        accesses.merge(other);
        assert_eq!(accesses.len(), 3);
        assert_eq!(
            accesses.get(&path("/a")),
            Some(AccessKinds::only(AccessKind::Probe).with(AccessKind::List))
        );

        let collected: Accesses = [
            (path("/x"), AccessKind::Read),
            (path("/x"), AccessKind::Exec),
        ]
        .into_iter()
        .collect();
        let owned: Vec<_> = collected.into_iter().collect();
        assert_eq!(owned, vec![(
            path("/x"),
            AccessKinds::only(AccessKind::Read).with(AccessKind::Exec)
        )]);
    }

    /// Normalizing removes `.` and repeated separators and keeps `..`.
    #[test]
    fn absolute_paths_normalize() {
        assert_eq!(path("/a/./b//c/").as_path(), Path::new("/a/b/c"));
        assert_eq!(path("/a/../b").as_path(), Path::new("/a/../b"));
        assert_eq!(path("//a").to_string(), "/a");
        assert_eq!(path("/").as_path(), Path::new("/"));
        assert_eq!(
            AbsolutePath::new("a/b"),
            Err(NotAbsoluteError { path: "a/b".into() })
        );
        assert_eq!(
            AbsolutePath::try_from(std::path::PathBuf::from("/x/."))
                .map(AbsolutePath::into_path_buf),
            Ok("/x".into())
        );
    }

    /// Relative paths resolve against a base, and absolute ones replace it.
    #[test]
    fn absolute_paths_join() {
        let base = path("/work/dir");
        assert_eq!(
            base.join("./src//main.rs").as_path(),
            Path::new("/work/dir/src/main.rs")
        );
        assert_eq!(base.join("../up").as_path(), Path::new("/work/dir/../up"));
        assert_eq!(
            base.join("/etc/./passwd").as_path(),
            Path::new("/etc/passwd")
        );
        assert_eq!(base.join("").as_path(), Path::new("/work/dir"));
    }

    /// Clones of a cancellation flag share it.
    #[test]
    fn cancellation_is_shared() {
        let cancel = Cancellation::new();
        let clone = cancel.clone();
        assert!(!clone.is_cancelled());
        cancel.cancel();
        assert!(clone.is_cancelled());
    }

    /// A forwarded signal is taken once, through any clone, and a signal forwarded
    /// twice before it is taken is taken once, in the order of `Signal::ALL`.
    #[test]
    fn forwarded_signals_are_taken_once() {
        let cancel = Cancellation::new();
        let tracer = cancel.clone();
        assert_eq!(tracer.take_forwarded(), Vec::new());
        cancel.forward(Signal::Terminate);
        assert_eq!(tracer.take_forwarded(), vec![Signal::Terminate]);
        assert_eq!(tracer.take_forwarded(), Vec::new());
        cancel.forward(Signal::Terminate);
        cancel.forward(Signal::Hangup);
        cancel.forward(Signal::Terminate);
        assert_eq!(tracer.take_forwarded(), vec![
            Signal::Hangup,
            Signal::Terminate
        ]);
        assert_eq!(cancel.take_forwarded(), Vec::new());
    }

    /// Forwarding a signal does not cancel, and cancelling forwards no signal.
    #[test]
    fn forwarding_and_cancellation_are_apart() {
        let cancel = Cancellation::new();
        cancel.forward(Signal::Hangup);
        assert!(!cancel.is_cancelled());
        assert_eq!(cancel.take_forwarded(), vec![Signal::Hangup]);
        cancel.cancel();
        assert_eq!(cancel.take_forwarded(), Vec::new());
    }

    /// A forwarded signal is named as the operating system names it.
    #[test]
    fn signals_are_named() {
        assert_eq!(Signal::Hangup.to_string(), "SIGHUP");
        assert_eq!(Signal::Terminate.to_string(), "SIGTERM");
    }

    /// A termination reports either an exit code or a signal.
    #[test]
    fn termination_reports_code_or_signal() {
        assert_eq!(Termination::Exited(3).code(), Some(3));
        assert_eq!(Termination::Exited(3).signal(), None);
        assert_eq!(Termination::Signaled(9).code(), None);
        assert_eq!(Termination::Signaled(9).signal(), Some(9));
        assert_eq!(Termination::Exited(0).to_string(), "exit code 0");
        assert_eq!(Termination::Signaled(2).to_string(), "killed by signal 2");
    }

    /// Captured output keeps the shape of its capture mode.
    #[test]
    fn captured_output_matches_mode() {
        for mode in [CaptureMode::Split, CaptureMode::Merged] {
            assert_eq!(CapturedOutput::empty(mode).mode(), mode);
        }
        assert_eq!(CaptureMode::default(), CaptureMode::Split);
    }

    /// The metadata of a regular file holds what `stat` reports, and nothing else
    /// has metadata.
    #[test]
    fn file_metadata_describes_only_regular_files() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let metadata = FileMetadata::at(&manifest).unwrap();
        let stat = fs::metadata(&manifest).unwrap();
        assert_eq!(FileMetadata::of(&stat), Some(metadata));
        assert_eq!(metadata.size, stat.len());
        assert!(metadata.mtime_nanos > 0 && metadata.ctime_nanos > 0);
        assert_eq!(metadata.mode & 0o170_000, 0o100_000);
        assert_eq!(
            FileMetadata::at(Path::new(env!("CARGO_MANIFEST_DIR"))),
            None
        );
        assert_eq!(
            FileMetadata::at(&Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-file")),
            None
        );
    }

    /// A first read is noted once, and never after a write of the same path;
    /// a write of another path does not matter.
    #[test]
    fn first_reads_are_noted_before_any_write() {
        let metadata = |inode| {
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
        };
        let mut accesses = Accesses::new();
        let mut reads = FirstReads::new();
        assert!(reads.is_empty());
        accesses.record(path("/other"), AccessKind::Write);
        assert!(reads.note(&path("/in"), metadata(1), &accesses));
        accesses.record(path("/in"), AccessKind::Read);
        // A later read keeps the first note, even after a write.
        accesses.record(path("/in"), AccessKind::Write);
        assert!(!reads.note(&path("/in"), metadata(2), &accesses));
        assert_eq!(reads.get(&path("/in")), Some(&metadata(1)));
        // A path written before its first read is never noted.
        accesses.record(path("/out"), AccessKind::Write);
        assert!(!reads.is_first(&path("/out"), &accesses));
        assert!(!reads.note(&path("/out"), metadata(3), &accesses));
        assert!(!reads.note_at(&path("/out"), &accesses));
        assert_eq!(reads.len(), 1);
        let noted: Vec<String> = reads.iter().map(|(path, _)| path.to_string()).collect();
        assert_eq!(noted, ["/in"]);
        assert_eq!((&reads).into_iter().count(), 1);
    }

    /// Noting a path reads its metadata now, following links, and notes nothing for
    /// a path that names no regular file.
    #[test]
    fn first_reads_read_the_metadata_of_regular_files() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let file = AbsolutePath::new(&manifest).unwrap();
        let directory = AbsolutePath::new(env!("CARGO_MANIFEST_DIR")).unwrap();
        let missing = directory.join("no-such-file");
        let accesses = Accesses::new();
        let mut reads = FirstReads::new();
        assert!(reads.note_at(&file, &accesses));
        assert!(!reads.note_at(&directory, &accesses));
        assert!(!reads.note_at(&missing, &accesses));
        assert_eq!(reads.get(&file).copied(), FileMetadata::at(&manifest));
        assert_eq!(reads.len(), 1);
    }

    /// A result is complete only without an incomplete-trace report.
    #[test]
    fn incomplete_results_are_reported() {
        let mut result = RunResult {
            termination: Termination::Exited(0),
            output: CapturedOutput::empty(CaptureMode::Merged),
            accesses: Accesses::new(),
            incomplete: None,
            first_reads: FirstReads::new(),
            unknown_system_calls: BTreeSet::new(),
            connections: BTreeSet::new(),
            tree: None,
        };
        assert!(result.is_complete());
        result.incomplete = Some(IncompleteReason::NonNativeSystemCall {
            detail: "int 0x80 open".to_owned(),
        });
        assert!(!result.is_complete());
        assert_eq!(
            result.incomplete.as_ref().map(ToString::to_string),
            Some("a system call used a non-native interface: int 0x80 open".to_owned())
        );
        assert_eq!(
            IncompleteReason::Other {
                detail: "lost a process".to_owned()
            }
            .to_string(),
            "lost a process"
        );
        assert_eq!(
            IncompleteReason::UntraceableSystemCall {
                detail: "process 7 made `chroot`".to_owned()
            }
            .to_string(),
            "a system call that kizz cannot trace: process 7 made `chroot`"
        );
        // Unknown system calls alone leave the trace complete.
        result.incomplete = None;
        result.unknown_system_calls = BTreeSet::from([400, 1_000_000]);
        assert!(result.is_complete());
        // Connections alone leave the trace complete too.
        result.connections = BTreeSet::from([Connection::UnixAbstract(b"kizz".to_vec())]);
        assert!(result.is_complete());
    }

    /// Connections read as an address and port, or as a Unix socket by path or
    /// abstract name with its unprintable bytes escaped, and sort by kind, then by
    /// address, path or name, without repeats.
    #[test]
    fn connections_display_and_sort() {
        let v4 = Connection::Ip("127.0.0.1:80".parse().unwrap());
        let v6 = Connection::Ip(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1".parse().unwrap(),
            443,
            0,
            2,
        )));
        let unix = Connection::UnixPath(path("/run/kizz.sock"));
        let abstract_name = Connection::UnixAbstract(b"kizz\0x".to_vec());
        assert_eq!(v4.to_string(), "127.0.0.1:80");
        assert_eq!(v6.to_string(), "[fe80::1%2]:443");
        assert_eq!(unix.to_string(), "Unix socket /run/kizz.sock");
        assert_eq!(
            abstract_name.to_string(),
            "abstract Unix socket @kizz\\x00x"
        );
        let set = BTreeSet::from([
            abstract_name.clone(),
            unix.clone(),
            v6.clone(),
            v4.clone(),
            v4.clone(),
        ]);
        assert_eq!(set.into_iter().collect::<Vec<_>>(), [
            v4,
            v6,
            unix,
            abstract_name
        ]);
    }

    /// Errors tell a missing program, another failure to start, and a tracer
    /// failure apart.
    #[test]
    fn trace_errors_are_classified() {
        let program = path("/bin/missing");
        let mut accesses = Accesses::new();
        accesses.record(program.clone(), AccessKind::Exec);

        let not_found = TraceError::ProgramNotFound {
            program: program.clone(),
            accesses: accesses.clone(),
            tree: None,
        };
        assert!(not_found.is_missing_program());
        assert!(!not_found.is_tracer_failure());
        assert_eq!(not_found.accesses(), Some(&accesses));
        assert_eq!(not_found.to_string(), "program not found: /bin/missing");

        let not_executable = TraceError::ProgramNotExecutable {
            program: program.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
            accesses: accesses.clone(),
            tree: None,
        };
        assert!(not_executable.is_missing_program());
        assert!(!not_executable.is_tracer_failure());

        let start_failed = TraceError::StartFailed {
            program,
            source: io::Error::from(io::ErrorKind::OutOfMemory),
            accesses,
            tree: None,
        };
        assert!(!start_failed.is_missing_program());
        assert!(!start_failed.is_tracer_failure());
        assert!(start_failed.accesses().is_some());

        let tracer = TraceError::Tracer {
            message: "seccomp filter rejected".to_owned(),
            source: None,
        };
        assert!(tracer.is_tracer_failure());
        assert!(!tracer.is_missing_program());
        assert_eq!(tracer.accesses(), None);
        assert_eq!(
            tracer.to_string(),
            "tracer failure: seccomp filter rejected"
        );
    }

    /// A request for `/bin/tool` with two arguments.
    fn tool_request() -> RunRequest {
        RunRequest {
            program: path("/bin/tool"),
            args: vec![OsString::from("tool"), OsString::from("--flag")],
            env: BTreeMap::new(),
            cwd: path("/work"),
            stdin: None,
            capture: CaptureMode::Split,
            echo: false,
            tree: true,
            terminal: false,
        }
    }

    /// Outcomes sort with `ok` first, then the error names in byte order, then
    /// `unknown`, and a set keeps each once; they display by name.
    #[test]
    fn outcomes_sort_and_merge_as_sets() {
        let error = |name: &str| Outcome::Error(name.to_owned());
        let outcomes: BTreeSet<Outcome> = [
            Outcome::Unknown,
            error("errno 200"),
            error("ERESTART_RESTARTBLOCK"),
            error("ENOENT"),
            Outcome::Ok,
            error("ERESTARTSYS"),
            error("EACCES"),
            error("ENOENT"),
            Outcome::Ok,
        ]
        .into_iter()
        .collect();
        let names: Vec<String> = outcomes.iter().map(ToString::to_string).collect();
        assert_eq!(names, [
            "ok",
            "EACCES",
            "ENOENT",
            "ERESTARTSYS",
            "ERESTART_RESTARTBLOCK",
            "errno 200",
            "unknown"
        ]);

        let mut observations = Observations::new();
        assert!(observations.is_empty());
        observations.record(path("/a"), AccessKind::Probe, error("ENOENT"));
        observations.record(path("/a"), AccessKind::Probe, Outcome::Ok);
        observations.record(path("/a"), AccessKind::Probe, error("ENOENT"));
        observations.record_kinds(
            path("/a"),
            AccessKinds::only(AccessKind::Read).with(AccessKind::Probe),
            &Outcome::Unknown,
        );
        let mut other = Observations::new();
        other.record(path("/a"), AccessKind::Read, Outcome::Ok);
        other.record_kinds(path("/b"), AccessKinds::EMPTY, &Outcome::Ok);
        observations.merge(&other);
        let a = observations.get(&path("/a")).unwrap();
        assert_eq!(
            a.get(&AccessKind::Probe).unwrap(),
            &BTreeSet::from([Outcome::Ok, error("ENOENT"), Outcome::Unknown])
        );
        assert_eq!(
            a.get(&AccessKind::Read).unwrap(),
            &BTreeSet::from([Outcome::Ok, Outcome::Unknown])
        );
        assert_eq!(
            observations.kinds(&path("/a")),
            Some(AccessKinds::only(AccessKind::Read).with(AccessKind::Probe))
        );
        // A path recorded without kinds is observed, as the combined accesses
        // record it.
        assert_eq!(observations.kinds(&path("/b")), Some(AccessKinds::EMPTY));
        assert_eq!(observations.len(), 2);
        assert_eq!(
            observations.accesses().get(&path("/b")),
            Some(AccessKinds::EMPTY)
        );
        let order: Vec<String> = (&observations)
            .into_iter()
            .map(|(path, _)| path.to_string())
            .collect();
        assert_eq!(order, ["/a", "/b"]);
    }

    /// The observations of two programs combine into the union of their kinds and,
    /// for each kind, of their outcomes; so do their connections and unknown system
    /// calls.
    #[test]
    fn the_observations_of_two_programs_combine() {
        let mut shell = Program::new(path("/bin/sh"), vec![OsString::from("sh")], false);
        shell
            .observations
            .record(path("/in"), AccessKind::Read, Outcome::Ok);
        shell.observations.record(
            path("/missing"),
            AccessKind::Probe,
            Outcome::Error("ENOENT".to_owned()),
        );
        shell.unknown_system_calls.insert(400);
        let mut inherited = Program::new(path("/bin/sh"), vec![OsString::from("sh")], true);
        assert!(inherited.is_empty());
        inherited
            .observations
            .record(path("/in"), AccessKind::Write, Outcome::Ok);
        inherited
            .observations
            .record(path("/missing"), AccessKind::Probe, Outcome::Unknown);
        inherited
            .connections
            .insert(Connection::UnixAbstract(b"kizz".to_vec()));
        assert!(!inherited.is_empty());
        let mut cat = Program::new(path("/bin/cat"), vec![OsString::from("cat")], false);
        cat.observations
            .record(path("/other"), AccessKind::Read, Outcome::Ok);
        cat.incomplete.push(IncompleteReason::Other {
            detail: "lost".to_owned(),
        });
        let tree = ProcessTree {
            processes: vec![
                Process {
                    number: 1,
                    pid: Some(10),
                    parent: None,
                    programs: vec![shell],
                    status: Some(Termination::Exited(0)),
                },
                Process {
                    number: 2,
                    pid: Some(11),
                    parent: Some(1),
                    programs: vec![inherited, cat],
                    status: Some(Termination::Signaled(9)),
                },
            ],
        };
        let combined = tree.observations();
        assert_eq!(
            combined.kinds(&path("/in")),
            Some(AccessKinds::only(AccessKind::Read).with(AccessKind::Write))
        );
        assert_eq!(
            combined
                .get(&path("/missing"))
                .and_then(|kinds| kinds.get(&AccessKind::Probe)),
            Some(&BTreeSet::from([
                Outcome::Error("ENOENT".to_owned()),
                Outcome::Unknown
            ]))
        );
        assert_eq!(combined.len(), 3);
        let mut expected = Accesses::new();
        expected.record(path("/in"), AccessKind::Read);
        expected.record(path("/in"), AccessKind::Write);
        expected.record(path("/missing"), AccessKind::Probe);
        expected.record(path("/other"), AccessKind::Read);
        assert_eq!(tree.accesses(), expected);
        assert_eq!(tree.unknown_system_calls(), BTreeSet::from([400]));
        assert_eq!(
            tree.connections(),
            BTreeSet::from([Connection::UnixAbstract(b"kizz".to_vec())])
        );
        assert_eq!(tree.programs().count(), 3);
        assert_eq!(tree.process(2).map(|process| process.parent), Some(Some(1)));
        assert!(tree.process(3).is_none());
    }

    /// The helper's tree of one process combines to exactly the accesses it was
    /// given, each with `ok`, under the request's program and arguments.
    #[test]
    fn the_helper_tree_combines_to_its_accesses() {
        let request = tool_request();
        let mut accesses = Accesses::new();
        accesses.record(path("/bin/tool"), AccessKind::Exec);
        accesses.record(path("/work/in"), AccessKind::Read);
        accesses.record(path("/work/in"), AccessKind::Probe);
        accesses.record(path("/work/out"), AccessKind::Write);
        let tree = ProcessTree::of_request(&request, &accesses, Some(Termination::Exited(3)));
        assert_eq!(tree.accesses(), accesses);
        let [process] = tree.processes.as_slice() else {
            panic!("expected one process, got {tree:?}");
        };
        assert_eq!(
            (process.number, process.pid, process.parent, process.status),
            (1, None, None, Some(Termination::Exited(3)))
        );
        let [program] = process.programs.as_slice() else {
            panic!("expected one program, got {process:?}");
        };
        assert_eq!(program.path, request.program);
        assert_eq!(program.args, request.args);
        assert!(!program.inherited);
        for (_, kinds) in &program.observations {
            for outcomes in kinds.values() {
                assert_eq!(outcomes, &BTreeSet::from([Outcome::Ok]));
            }
        }
        let empty = ProcessTree::of_request(&request, &Accesses::new(), None);
        assert!(empty.accesses().is_empty());
        assert_eq!(empty.process(1).map(|process| process.status), Some(None));
    }

    /// The tree that the helper builds from a whole run holds the run's reason,
    /// unknown system calls and connections on its one program, so its unknown
    /// system calls and connections equal the run's, and its observations combine
    /// to the run's accesses, each with `ok`; a run without them gives the tree of
    /// `of_request`.
    #[test]
    fn the_helper_tree_of_a_run_holds_what_the_run_reports() {
        let request = tool_request();
        let mut accesses = Accesses::new();
        accesses.record(path("/bin/tool"), AccessKind::Exec);
        accesses.record(path("/work/in"), AccessKind::Read);
        accesses.record(path("/work/out"), AccessKind::Write);
        let reason = IncompleteReason::Other {
            detail: "the tracer lost a process".to_owned(),
        };
        let connections = BTreeSet::from([
            Connection::Ip("127.0.0.1:80".parse().unwrap()),
            Connection::UnixAbstract(b"kizz".to_vec()),
        ]);
        let result = RunResult {
            termination: Termination::Exited(4),
            output: CapturedOutput::empty(CaptureMode::Split),
            accesses: accesses.clone(),
            incomplete: Some(reason.clone()),
            first_reads: FirstReads::new(),
            unknown_system_calls: BTreeSet::from([400, 1_000_000]),
            connections: connections.clone(),
            tree: None,
        };
        let tree = ProcessTree::of_run(&request, &result);
        assert_eq!(tree.accesses(), accesses);
        assert_eq!(tree.unknown_system_calls(), result.unknown_system_calls);
        assert_eq!(tree.connections(), connections);
        let [process] = tree.processes.as_slice() else {
            panic!("expected one process, got {tree:?}");
        };
        assert_eq!(
            (process.number, process.pid, process.parent, process.status),
            (1, None, None, Some(Termination::Exited(4)))
        );
        let [program] = process.programs.as_slice() else {
            panic!("expected one program, got {process:?}");
        };
        assert_eq!(program.incomplete, vec![reason]);
        for (_, kinds) in &program.observations {
            for outcomes in kinds.values() {
                assert_eq!(outcomes, &BTreeSet::from([Outcome::Ok]));
            }
        }
        let plain = RunResult {
            incomplete: None,
            unknown_system_calls: BTreeSet::new(),
            connections: BTreeSet::new(),
            ..result
        };
        assert_eq!(
            ProcessTree::of_run(&request, &plain),
            ProcessTree::of_request(&request, &accesses, Some(Termination::Exited(4)))
        );
    }

    /// A tracer that runs nothing and implements only the required methods, so the
    /// error-name query gives its default answer.
    struct Unnamed;

    impl Tracer for Unnamed {
        fn run(
            &self,
            _request: &RunRequest,
            _echo: &dyn EchoSink,
            _cancel: &Cancellation,
        ) -> Result<RunResult, TraceError> {
            Err(TraceError::Tracer {
                message: "the test tracer runs nothing".to_owned(),
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

        fn machine_state(
            &self,
            _cwd: &Path,
        ) -> MachineState {
            MachineState::default()
        }
    }

    /// The default error name is `errno` and the number.
    #[test]
    fn the_default_error_name_is_errno_and_the_number() {
        assert_eq!(Unnamed.error_name(2), "errno 2");
        assert_eq!(Unnamed.error_name(200), "errno 200");
        assert_eq!(Unnamed.error_name(-1), "errno -1");
        assert_eq!(unnamed_error(512), "errno 512");
    }

    /// A failed start carries the tree of its one process when it has one, and a
    /// failure of the tracer has none.
    #[test]
    fn failed_starts_carry_their_tree() {
        let request = tool_request();
        let mut accesses = Accesses::new();
        accesses.record(request.program.clone(), AccessKind::Probe);
        let tree = ProcessTree::of_request(&request, &accesses, None);
        let not_found = TraceError::ProgramNotFound {
            program: request.program.clone(),
            accesses: accesses.clone(),
            tree: Some(tree.clone()),
        };
        assert_eq!(not_found.tree(), Some(&tree));
        let not_executable = TraceError::ProgramNotExecutable {
            program: request.program.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
            accesses: accesses.clone(),
            tree: Some(tree.clone()),
        };
        assert_eq!(not_executable.tree(), Some(&tree));
        let start_failed = TraceError::StartFailed {
            program: request.program,
            source: io::Error::from(io::ErrorKind::OutOfMemory),
            accesses,
            tree: None,
        };
        assert_eq!(start_failed.tree(), None);
        let tracer = TraceError::Tracer {
            message: "failed".to_owned(),
            source: None,
        };
        assert_eq!(tracer.tree(), None);
    }
}
