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
//! for each path, and a report when the trace is incomplete.
//!
//! Version 1 has one tracer, for Linux, in `kizz_trace_linux`; tests of the engine
//! implement [`Tracer`] with a fake. Nothing in the engine changes when the tracer
//! does.

use std::{
    collections::{BTreeMap, btree_map},
    ffi::OsString,
    fmt,
    io::{self, Write},
    ops::{BitOr, BitOrAssign},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
}

impl AccessKind {
    /// Every access kind, in the order [`AccessKinds`] lists them.
    pub const ALL: [Self; 5] = [Self::Read, Self::Probe, Self::List, Self::Write, Self::Exec];

    /// The bit that represents this kind in [`AccessKinds`].
    const fn bit(self) -> u8 {
        match self {
            Self::Read => 0b0000_0001,
            Self::Probe => 0b0000_0010,
            Self::List => 0b0000_0100,
            Self::Write => 0b0000_1000,
            Self::Exec => 0b0001_0000,
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

/// How a command's standard output and standard error are captured.
///
/// Output is always captured; the mode only decides how.
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
    /// The content written to standard input, or `None` to connect `/dev/null`.
    pub stdin: Option<Vec<u8>>,
    /// How standard output and standard error are captured.
    pub capture: CaptureMode,
    /// Whether output is also echoed live, through the [`EchoSink`] given to
    /// [`Tracer::run`], while it is captured.
    pub echo: bool,
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

/// A shared flag that cancels a running request.
///
/// Clones share the flag, so the engine keeps one clone and hands another to the
/// tracer. When the flag is set, the tracer interrupts the request's process tree,
/// kills whatever remains of it after a grace period, and returns its result once
/// the whole tree has exited. The result reports how the command ended: by a
/// signal, or with an exit code that the command chose after it handled the
/// interrupt.
#[derive(Debug, Clone, Default)]
pub struct Cancellation {
    /// Whether cancellation was requested.
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    /// A flag that is not set.
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
            Self::Other { detail } => f.write_str(detail),
        }
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
}

/// Runs and traces commands.
///
/// The Linux tracer implements it with `ptrace` and seccomp, and tests implement it
/// with a fake that performs scripted file operations and reports them.
pub trait Tracer: Send + Sync {
    /// Run one command, wait for its whole process tree to finish, and report what
    /// it did.
    ///
    /// When [`RunRequest::echo`] is set, output also goes to `echo` live. When
    /// `cancel` is set while the command runs, the tracer interrupts the whole
    /// process tree, kills whatever remains of it after a grace period, and returns
    /// the result once the tree has exited; the command may then have ended by a
    /// signal or with an exit code.
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
}

/// Unit tests for the logic of the tracer interface.
#[cfg(test)]
mod tests {
    use std::{io, path::Path};

    use super::{
        AbsolutePath, AccessKind, AccessKinds, Accesses, Cancellation, CaptureMode, CapturedOutput,
        IncompleteReason, NotAbsoluteError, RunResult, Termination, TraceError,
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

    /// Every kind has its own bit, so the full set holds all five.
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

    /// A result is complete only without an incomplete-trace report.
    #[test]
    fn incomplete_results_are_reported() {
        let mut result = RunResult {
            termination: Termination::Exited(0),
            output: CapturedOutput::empty(CaptureMode::Merged),
            accesses: Accesses::new(),
            incomplete: None,
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
        };
        assert!(not_found.is_missing_program());
        assert!(!not_found.is_tracer_failure());
        assert_eq!(not_found.accesses(), Some(&accesses));
        assert_eq!(not_found.to_string(), "program not found: /bin/missing");

        let not_executable = TraceError::ProgramNotExecutable {
            program: program.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
            accesses: accesses.clone(),
        };
        assert!(not_executable.is_missing_program());
        assert!(!not_executable.is_tracer_failure());

        let start_failed = TraceError::StartFailed {
            program,
            source: io::Error::from(io::ErrorKind::OutOfMemory),
            accesses,
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
}
