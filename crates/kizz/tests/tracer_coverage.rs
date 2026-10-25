// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The tracer conformance test, phase 10 of `architecture.md`.
//!
//! The fixture project `tests/tracer-coverage/` holds `coverage.c`, a plain C program
//! that makes every tracked system call in several variants, the inputs it uses,
//! `expected.txt`, which lists every path that its run must record, and a `kizz.md`
//! with two tasks: `coverage-build` compiles the program with `cc`, and the cached
//! `coverage-test` runs it.
//!
//! The end-to-end test copies the fixture into a temporary directory and runs
//! `kizz build coverage-build` there. It resets the copy's cache entries with
//! `kizz cache reset`, runs `kizz build coverage-test`, and compares the observations of
//! the one step record with the entries of `expected.txt` for the platform it runs on:
//! each path's kinds, the type of its state and, for a path whose extended attributes
//! the program read, the `user.` names of the attributes recorded with it.
//! Every run of `kizz` gets a temporary cache and an explicit environment of only
//! `KIZZ_CACHE_DIR` and `PATH`. Paths outside the copy, such as those of the dynamic
//! loader and the C library, are not compared with the expectation file, but the
//! loader that the program's `PT_INTERP` header names must be recorded with the exec
//! kind.
//!
//! The other tests check `expected.txt` itself: that it is consistent, and that each
//! tracked system call that `architecture.md` lists, the same list as
//! [`TRACKED_SYSTEM_CALLS`], has an entry for its success and, for a call that takes a
//! path, one for its failure with `ENOENT`, on which a tracer that missed the call
//! would record something else. The same holds for the kernel's implicit loads of
//! [`IMPLICIT_LOADS`]. Unit tests cover the parser and the comparison.
//!
//! The expectation file also lists the connections that the run must record, on
//! lines of their own: the portable core and the Linux section connect and send to
//! Unix sockets under their output directories and, through UDP sockets, to
//! loopback addresses on fixed ports, one for each call. The end-to-end test
//! compares the record's connections with those for its platform exactly, and the
//! build must warn about them once and repeat the warning in its summary. Each
//! call that connects has a connection line of its own, made through `syscall()`.
//!
//! Another end-to-end test builds `coverage-changes` in its own copy, after
//! `coverage-build` and a reset: each of its steps, [`CHANGES`], runs the program in
//! a mode that reads or runs one committed file under `in/changes` through one way
//! in which a command first reads a file, and then rewrites it, so its result is not
//! cached and its reason names that file with the command as its writer; the
//! `control` step writes its file before it reads it, so its result is cached.
//!
//! The availability gate covers the tracked calls that a kernel may lack because a
//! newer Linux added them, [`GATED_CALLS`]: `file_getattr`, `file_setattr` and the
//! `xattrat` calls. The tracer tracks them on every kernel, but the program makes
//! each once with arguments that cannot succeed, reports whether it is available in
//! a line of its output, and makes a group's calls only where every call that the
//! group needs is available. A call that returns `ENOSYS` is unavailable: `ENOSYS`
//! means that the kernel lacks the call, or that a seccomp filter installed before
//! kizz's own refuses it. The entries of those groups carry a mark after their
//! platform that names the gated calls they need, and an entry is expected only
//! where each of them is available. So where a call is available, each of its
//! entries is recorded exactly; where it is unavailable, the exact comparison
//! requires that nothing is recorded at its entries' paths. The test checks each
//! gated call itself in its own process, which no tracer follows, and a report that
//! disagrees is a problem; for each unavailable call it writes a notice that starts
//! with `NOTICE:` to its standard error. The static checks do not depend on the
//! kernel: every tracked call keeps the entries that only it explains.
//!
//! Two more end-to-end tests build `coverage-incomplete` in copies of their own,
//! after `coverage-build` and a reset. Each of its steps runs the program in a mode
//! that makes one call of [`UNTRACEABLE_SYSTEM_CALLS`], the list that
//! `architecture.md` gives under [`UNTRACEABLE_HEADING`], so its result is not
//! cached and its reason names the call, or one of [`UNKNOWN_NUMBERS`], which the
//! tracer does not know, so its result is not cached and its reason names the
//! number, and the build warns about it and repeats the warning in its summary.
//! With `--cache-unknown-syscalls` the steps of the numbers are cached, with the
//! numbers in their records; a later build without the option finds them cached
//! and warns about neither, and `kizz why` lists their numbers. Of the calls,
//! `open_tree_attr` and `fchroot` are checked only where they are available, as
//! the test's own check and the program's report, which must agree, find; where one
//! is unavailable, its step need not be marked, and a notice names it.
//!
//! A seccomp filter installed before kizz's own can refuse a call before the tracer
//! sees it. A call or number that the tracer never saw, whose step is cached and
//! lists no unknown system call, fails these two tests with a message
//! that names it and [`SKIP_VARIABLE`], unless that variable is `1` in the test's
//! environment: then its checks are skipped, and a notice that starts with
//! `NOTICE:` names it. Either way its step must have exited with 0, and its mode
//! must report the call with an error. A call or number that the tracer saw is
//! checked in full.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::{CStr, OsStr},
    fmt::{self, Write as _},
    fs,
    io::{self, Write as _},
    os::unix::{ffi::OsStrExt as _, fs::symlink},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};

use kizz_store::{
    AccessBits, Attribute, Attributes, ChangedRead, Connection, NotCachedReason, PathState,
    RecordedOutput, SpecialKind, StepExit, StepKey, StepRecord, Store, TaskId,
};
use kizz_trace_linux::{TRACKED_SYSTEM_CALLS, UNTRACEABLE_SYSTEM_CALLS};
use libc::c_long;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The global options of every run: no color and no banner.
const PLAIN: [&str; 3] = ["--color", "never", "--no-banner"];

/// The `PATH` every run of `kizz` gets, which the `coverage-build` task inherits.
const PATH: &str = "/usr/bin:/bin";

/// The name of the fixture directory, under `tests/` at the repository root.
const FIXTURE: &str = "tracer-coverage";

/// The name of the expectation file in the fixture.
const EXPECTATIONS: &str = "expected.txt";

/// The line of `architecture.md` that introduces the tracked system calls.
const TRACKED_HEADING: &str = "Tracked system calls, by x86_64 name:";

/// The start of the line the program prints when every call returned what it
/// expected.
const SUCCESS_LINE: &str = "coverage: every call returned what it expected";

/// The index of the `coverage-test` block in the fixture's `kizz.md`.
const TEST_BLOCK: u32 = 1;

/// The index of the `coverage-changes` block in the fixture's `kizz.md`.
const CHANGES_BLOCK: u32 = 2;

/// The index of the `coverage-incomplete` block in the fixture's `kizz.md`.
const INCOMPLETE_BLOCK: u32 = 3;

/// The line of `architecture.md` that introduces the system calls that mark a run
/// as incompletely traced.
const UNTRACEABLE_HEADING: &str = "System calls that mark the run as incompletely traced, by \
                                   x86_64 name:";

/// The calls that mark a run and that a kernel may lack because a newer Linux added
/// them, in the order of [`UNTRACEABLE_SYSTEM_CALLS`].
const GATED_UNTRACEABLE: [&str; 2] = ["fchroot", "open_tree_attr"];

/// The system calls that the `coverage-incomplete` task makes and that the tracer
/// does not know.
const UNKNOWN_NUMBERS: [u32; 2] = [400, 1_000_000];

/// The variable that, set to `1` in the environment of the test process, makes the
/// tests of `coverage-incomplete` skip, with a notice, each call or number that the
/// tracer never saw, because a seccomp filter installed before kizz's own may have
/// refused it. Only test code reads it, and only [`skip_mode`].
const SKIP_VARIABLE: &str = "KIZZ_TEST_SKIP_BLOCKED_SYSCALLS";

/// Each mode of the `coverage-changes` task, after its `change-` prefix, with the
/// file under the task directory that it reads or runs and then rewrites, or
/// `None` for the control, which writes its file before it reads it.
const CHANGES: [(&str, Option<&str>); 9] = [
    ("open", Some("in/changes/open.txt")),
    ("openat", Some("in/changes/openat.txt")),
    ("openat2", Some("in/changes/openat2.txt")),
    ("fopen", Some("in/changes/fopen.txt")),
    ("execve", Some("in/changes/execve")),
    ("execveat", Some("in/changes/execveat")),
    ("execv", Some("in/changes/execv")),
    ("interpreter", Some("in/changes/interpreter")),
    ("control", None),
];

/// The tracked system calls that take no path, only a descriptor, so they have no
/// `ENOENT` entry.
const DESCRIPTOR_CALLS: [&str; 12] = [
    "fstatfs",
    "fgetxattr",
    "flistxattr",
    "fsetxattr",
    "fremovexattr",
    "getdents",
    "getdents64",
    "fchdir",
    "ftruncate",
    "fchmod",
    "fchown",
    "fallocate",
];

/// The errors with which a call that reads or lists extended attributes fails and
/// still adds the xattr kind, because each answers from the attributes.
const ATTRIBUTE_ANSWERS: [&str; 3] = ["ENODATA", "ERANGE", "EOPNOTSUPP"];

/// The namespace of the extended attributes that the expectation file names: the
/// one that the fixture sets. Names in other namespaces, such as `security.`,
/// depend on the machine and are not compared.
const USER_NAMESPACE: &str = "user.";

/// The implicit loads of the kernel that the coverage check requires, each as the
/// name of its `kernel` source and an outcome: the interpreter of a script that a
/// process runs, the interpreter of that interpreter, which is a script itself,
/// and a missing interpreter.
const IMPLICIT_LOADS: [(&str, &str); 3] = [
    ("interpreter", "ok"),
    ("interpreter_of_interpreter", "ok"),
    ("interpreter", "ENOENT"),
];

/// The tracked calls that a kernel may lack because a newer Linux added them, in
/// the order of [`TRACKED_SYSTEM_CALLS`]: Linux 6.17 added `file_getattr` and
/// `file_setattr`, and Linux 6.13 the `xattrat` calls.
const GATED_CALLS: [&str; 6] = [
    "file_getattr",
    "getxattrat",
    "listxattrat",
    "setxattrat",
    "removexattrat",
    "file_setattr",
];

/// The start of the line in which the program reports that a gated call is
/// available, before the call's name.
const AVAILABLE_LINE: &str = "coverage: available: ";

/// The start of the line in which the program reports that a gated call is
/// unavailable, because it returned `ENOSYS`, before the call's name.
const UNAVAILABLE_LINE: &str = "coverage: unavailable: ";

/// The path on which the availability check makes each gated call: the root,
/// outside every task directory.
const CHECKED_PATH: &CStr = c"/";

/// The attribute name that the availability check passes to the `xattrat` calls
/// that take one.
const CHECKED_ATTRIBUTE: &CStr = c"user.kizz";

/// The first field of a connection line of the expectation file.
const CONNECTION_FIELD: &str = "connection";

/// The calls that record a connection, which the connection lines name.
const CONNECTING_CALLS: [&str; 4] = ["connect", "sendto", "sendmsg", "sendmmsg"];

/// The number of system calls that the tracer tracks, as `architecture.md` lists
/// them.
const TRACKED_COUNT: usize = 76;

/// An access kind, as "What is recorded" names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// Content was read.
    Read,
    /// Existence or metadata was checked.
    Probe,
    /// A directory's entries were read.
    List,
    /// The path was created, changed, or removed.
    Write,
    /// The path was run as a program.
    Exec,
    /// The path's extended attributes were read or listed.
    Xattr,
}

/// Every kind with its name and its bit in the store, in order.
const KINDS: [(Kind, &str, u8); 6] = [
    (Kind::Read, "read", AccessBits::READ),
    (Kind::Probe, "probe", AccessBits::PROBE),
    (Kind::List, "list", AccessBits::LIST),
    (Kind::Write, "write", AccessBits::WRITE),
    (Kind::Exec, "exec", AccessBits::EXEC),
    (Kind::Xattr, "xattr", AccessBits::XATTR),
];

/// A set of access kinds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Kinds(
    /// The kinds, in order.
    BTreeSet<Kind>,
);

impl Kinds {
    /// The kinds of the store's access bits `bits`.
    fn from_bits(bits: AccessBits) -> Self {
        Self(
            KINDS
                .iter()
                .filter(|(_, _, bit)| bits.contains(*bit))
                .map(|(kind, ..)| *kind)
                .collect(),
        )
    }

    /// Whether every kind of `self` is in `other`.
    fn is_subset(
        &self,
        other: &Self,
    ) -> bool {
        self.0.is_subset(&other.0)
    }

    /// Add every kind of `other`.
    fn extend(
        &mut self,
        other: &Self,
    ) {
        self.0.extend(other.0.iter().copied());
    }
}

impl FromStr for Kinds {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut kinds = BTreeSet::new();
        for name in text.split('+') {
            let kind = KINDS
                .iter()
                .find(|(_, known, _)| *known == name)
                .ok_or_else(|| format!("unknown access kind `{name}`"))?;
            if !kinds.insert(kind.0) {
                return Err(format!("the access kind `{name}` is repeated"));
            }
        }
        Ok(Self(kinds))
    }
}

impl fmt::Display for Kinds {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let names: Vec<&str> = KINDS
            .iter()
            .filter(|(kind, ..)| self.0.contains(kind))
            .map(|(_, name, _)| *name)
            .collect();
        if names.is_empty() {
            f.write_str("(none)")
        } else {
            f.write_str(&names.join("+"))
        }
    }
}

/// The type of a path's state, as "Path states" names it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Nothing exists at the path.
    Absent,
    /// A regular file, with or without the hash of its content.
    File {
        /// Whether the content was hashed.
        hash: bool,
    },
    /// A directory, with or without its listing.
    Dir {
        /// Whether its entries were listed.
        listing: bool,
    },
    /// A FIFO.
    Fifo,
    /// A socket.
    Socket,
    /// A character device.
    CharDevice,
    /// A block device.
    BlockDevice,
    /// Another type.
    Other,
    /// A symbolic link, with its target and the state of what it points to.
    Symlink {
        /// The target, as stored in the link.
        target: String,
        /// The state of what the link points to.
        pointee: Box<State>,
    },
    /// A path under `/proc` or `/sys` that is not compared.
    Untracked,
    /// A path whose state could not be read, with the error number.
    Inaccessible(i32),
}

impl State {
    /// The state type of a recorded state.
    fn from_recorded(state: &PathState) -> Self {
        match state {
            PathState::Absent => Self::Absent,
            PathState::File { hash, .. } => {
                Self::File {
                    hash: hash.is_some(),
                }
            },
            PathState::Directory { listing, .. } => {
                Self::Dir {
                    listing: listing.is_some(),
                }
            },
            PathState::Symlink { target, pointee } => {
                Self::Symlink {
                    target: target.to_string_lossy().into_owned(),
                    pointee: Box::new(Self::from_recorded(pointee)),
                }
            },
            PathState::Special { kind } => {
                match kind {
                    SpecialKind::Fifo => Self::Fifo,
                    SpecialKind::Socket => Self::Socket,
                    SpecialKind::CharDevice => Self::CharDevice,
                    SpecialKind::BlockDevice => Self::BlockDevice,
                    SpecialKind::Other => Self::Other,
                }
            },
            PathState::Untracked => Self::Untracked,
            PathState::Inaccessible { errno } => Self::Inaccessible(*errno),
        }
    }
}

impl FromStr for State {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = text.strip_prefix("symlink:") {
            let (target, pointee) = rest
                .split_once(':')
                .ok_or_else(|| format!("the symbolic link state `{text}` has no pointee"))?;
            let pointee: Self = pointee.parse()?;
            if target.is_empty() || matches!(pointee, Self::Symlink { .. }) {
                return Err(format!("invalid symbolic link state `{text}`"));
            }
            return Ok(Self::Symlink {
                target: target.to_owned(),
                pointee: Box::new(pointee),
            });
        }
        Ok(match text {
            "absent" => Self::Absent,
            "file" => Self::File { hash: false },
            "file+hash" => Self::File { hash: true },
            "dir" => Self::Dir { listing: false },
            "dir+listing" => Self::Dir { listing: true },
            "fifo" => Self::Fifo,
            "socket" => Self::Socket,
            "char-device" => Self::CharDevice,
            "block-device" => Self::BlockDevice,
            _ => return Err(format!("unknown state `{text}`")),
        })
    }
}

impl fmt::Display for State {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("absent"),
            Self::File { hash: false } => f.write_str("file"),
            Self::File { hash: true } => f.write_str("file+hash"),
            Self::Dir { listing: false } => f.write_str("dir"),
            Self::Dir { listing: true } => f.write_str("dir+listing"),
            Self::Fifo => f.write_str("fifo"),
            Self::Socket => f.write_str("socket"),
            Self::CharDevice => f.write_str("char-device"),
            Self::BlockDevice => f.write_str("block-device"),
            Self::Other => f.write_str("other"),
            Self::Symlink { target, pointee } => write!(f, "symlink:{target}:{pointee}"),
            Self::Untracked => f.write_str("untracked"),
            Self::Inaccessible(errno) => write!(f, "inaccessible({errno})"),
        }
    }
}

/// The `user.` names of one side of a path's recorded extended attributes, sorted,
/// or `None` when they could not be read.
type Names = Option<Vec<String>>;

/// The extended attributes recorded with a path, by their `user.` names: those of
/// the path itself and, for a symbolic link, those of what it points to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttributeNames {
    /// The path's own attributes.
    own: Names,
    /// For a symbolic link, the attributes of what it points to, or `None` when
    /// they were not recorded.
    pointee: Option<Names>,
}

impl AttributeNames {
    /// The `user.` names of the recorded `attributes`.
    fn from_recorded(attributes: &Attributes) -> Self {
        let names = |list: &[Attribute]| -> Vec<String> {
            list.iter()
                .map(|attribute| String::from_utf8_lossy(&attribute.name).into_owned())
                .filter(|name| name.starts_with(USER_NAMESPACE))
                .collect()
        };
        Self {
            own: attributes.own.as_deref().map(names),
            pointee: attributes.pointee.as_deref().map(|list| Some(names(list))),
        }
    }
}

/// One side of [`AttributeNames`] as the expectation file writes it: `-` when the
/// attributes could not be read, or else the names joined with `,`, possibly none.
fn names_text(names: Option<&[String]>) -> String {
    names.map_or_else(|| "-".to_owned(), |names| names.join(","))
}

/// One side of [`AttributeNames`] from its text, as [`names_text`] writes it.
fn parse_names(text: &str) -> Result<Names, String> {
    if text == "-" {
        return Ok(None);
    }
    if text.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let names: Vec<String> = text.split(',').map(str::to_owned).collect();
    if names.iter().any(|name| !name.starts_with(USER_NAMESPACE)) {
        return Err(format!(
            "the attribute names `{text}` are not all in `user.`"
        ));
    }
    Ok(Some(names))
}

/// The recorded state of a path, with its extended attributes when they were
/// recorded. The expectation file writes it as the state, followed, when the
/// attributes were recorded, by `@` and their names, and for a symbolic link by `/`
/// and the names of what it points to, such as `file@user.kizz` or
/// `symlink:t:file@/user.kizz`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    /// The type of the state.
    state: State,
    /// The recorded extended attributes, or `None` when none were recorded.
    attributes: Option<AttributeNames>,
}

impl Observed {
    /// A state without recorded attributes.
    const fn plain(state: State) -> Self {
        Self {
            state,
            attributes: None,
        }
    }
}

impl FromStr for Observed {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let Some((state, attributes)) = text.split_once('@') else {
            return Ok(Self::plain(text.parse()?));
        };
        let (own, pointee) = match attributes.split_once('/') {
            Some((own, pointee)) => (own, Some(parse_names(pointee)?)),
            None => (attributes, None),
        };
        Ok(Self {
            state: state.parse()?,
            attributes: Some(AttributeNames {
                own: parse_names(own)?,
                pointee,
            }),
        })
    }
}

impl fmt::Display for Observed {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", self.state)?;
        if let Some(attributes) = &self.attributes {
            write!(f, "@{}", names_text(attributes.own.as_deref()))?;
            if let Some(pointee) = &attributes.pointee {
                write!(f, "/{}", names_text(pointee.as_deref()))?;
            }
        }
        Ok(())
    }
}

/// The platforms that an entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    /// Every platform: the portable core.
    All,
    /// Linux on every architecture.
    Linux,
    /// Linux on `x86_64`.
    LinuxX86,
    /// Linux on aarch64.
    LinuxArm,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
}

/// Every platform with its name in the expectation file.
const PLATFORMS: [(Platform, &str); 6] = [
    (Platform::All, "all"),
    (Platform::Linux, "linux"),
    (Platform::LinuxX86, "linux-x86_64"),
    (Platform::LinuxArm, "linux-aarch64"),
    (Platform::MacOs, "macos"),
    (Platform::Windows, "windows"),
];

impl Platform {
    /// Whether the test runs on this platform.
    const fn is_current(self) -> bool {
        match self {
            Self::All => true,
            Self::Linux => cfg!(target_os = "linux"),
            Self::LinuxX86 => cfg!(all(target_os = "linux", target_arch = "x86_64")),
            Self::LinuxArm => cfg!(all(target_os = "linux", target_arch = "aarch64")),
            Self::MacOs => cfg!(target_os = "macos"),
            Self::Windows => cfg!(target_os = "windows"),
        }
    }

    /// Whether some platform is both `self` and `other`.
    fn overlaps(
        self,
        other: Self,
    ) -> bool {
        let linux = |platform: Self| matches!(platform, Self::LinuxX86 | Self::LinuxArm);
        self == other
            || self == Self::All
            || other == Self::All
            || (self == Self::Linux && linux(other))
            || (other == Self::Linux && linux(self))
    }
}

/// Where a source of an entry comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A tracked system call that the program makes through `syscall()`.
    Sys,
    /// A function of the C library.
    Libc,
    /// Kizz's own start of the program, a traced `execve`.
    Kizz,
    /// A file that the kernel loads to run a program, without a system call of its
    /// own: an interpreter that a `#!` line names, or an ELF loader.
    Kernel,
}

/// One call that touches an entry's path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    /// Where the call comes from.
    origin: Origin,
    /// The name of the call.
    name: String,
    /// `ok`, or the name of the error the call failed with.
    outcome: String,
    /// The kinds the call adds.
    kinds: Kinds,
}

impl Source {
    /// Whether the call succeeded.
    fn succeeded(&self) -> bool {
        self.outcome == "ok"
    }

    /// Whether this is the tracked system call `name`.
    fn is_system_call(
        &self,
        name: &str,
    ) -> bool {
        self.origin == Origin::Sys && self.name == name
    }

    /// Whether this is the implicit load `name` of the kernel.
    fn is_implicit_load(
        &self,
        name: &str,
    ) -> bool {
        self.origin == Origin::Kernel && self.name == name
    }
}

impl FromStr for Source {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let fields: Vec<&str> = text.split(':').collect();
        let [origin, name, outcome, kinds] = fields.as_slice() else {
            return Err(format!(
                "the source `{text}` is not ORIGIN:NAME:OUTCOME:KINDS"
            ));
        };
        let origin = match *origin {
            "sys" => Origin::Sys,
            "libc" => Origin::Libc,
            "kizz" => Origin::Kizz,
            "kernel" => Origin::Kernel,
            _ => return Err(format!("unknown origin `{origin}` in `{text}`")),
        };
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        let valid_outcome = *outcome == "ok"
            || (outcome.len() > 1
                && outcome.starts_with('E')
                && outcome.bytes().all(|byte| byte.is_ascii_uppercase()));
        if !valid_name || !valid_outcome {
            return Err(format!("invalid name or outcome in `{text}`"));
        }
        Ok(Self {
            origin,
            name: (*name).to_owned(),
            outcome: (*outcome).to_owned(),
            kinds: kinds.parse()?,
        })
    }
}

/// One entry of the expectation file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// The line of the entry, counting from one.
    line: usize,
    /// The path, relative to the task directory.
    path: String,
    /// The access kinds that must be recorded.
    kinds: Kinds,
    /// The type of the state that must be recorded, with the names of its recorded
    /// extended attributes.
    state: Observed,
    /// The platforms that expect the entry.
    platform: Platform,
    /// The gated calls that the entry's mark names, which its group needs: the
    /// entry is expected only where each of them is available.
    needs: Vec<String>,
    /// Every call that touches the path in the run.
    sources: Vec<Source>,
}

/// Whether `name` is a well-formed call name: letters, digits and `_`.
fn is_call_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The platform and the names of the mark of a `PLATFORM[/CALL+...]` field.
fn parse_platform(text: &str) -> Result<(Platform, Vec<String>), String> {
    let (platform, mark) = match text.split_once('/') {
        Some((platform, mark)) => (platform, Some(mark)),
        None => (text, None),
    };
    let platform = PLATFORMS
        .iter()
        .find(|(_, name)| *name == platform)
        .map(|(known, _)| *known)
        .ok_or_else(|| format!("unknown platform `{platform}`"))?;
    let Some(mark) = mark else {
        return Ok((platform, Vec::new()));
    };
    let needs: Vec<String> = mark.split('+').map(str::to_owned).collect();
    if !needs.iter().all(|name| is_call_name(name)) {
        return Err(format!("the mark of `{text}` is not CALL+..."));
    }
    Ok((platform, needs))
}

/// Whether `path` is a relative path as the tracer reports it: no root, no empty
/// component, no `.` component, and no trailing separator.
fn is_normal_relative(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(|part| !part.is_empty() && part != ".")
}

/// Parse one entry line, the `line`-th of the file.
fn parse_entry(
    line: usize,
    text: &str,
) -> Result<Entry, String> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [path, kinds, state, platform, sources @ ..] = fields.as_slice() else {
        return Err(format!(
            "line {line}: an entry is PATH KINDS STATE PLATFORM SOURCE..."
        ));
    };
    if sources.is_empty() {
        return Err(format!("line {line}: the entry for {path} has no source"));
    }
    if !is_normal_relative(path) {
        return Err(format!(
            "line {line}: {path} is not a normalized relative path"
        ));
    }
    let (platform, needs) =
        parse_platform(platform).map_err(|error| format!("line {line}: {error}"))?;
    let sources = sources
        .iter()
        .map(|source| source.parse())
        .collect::<Result<Vec<Source>, String>>()
        .map_err(|error| format!("line {line}: {error}"))?;
    Ok(Entry {
        line,
        path: (*path).to_owned(),
        kinds: kinds
            .parse()
            .map_err(|error| format!("line {line}: {error}"))?,
        state: state
            .parse()
            .map_err(|error| format!("line {line}: {error}"))?,
        platform,
        needs,
        sources,
    })
}

/// Parse an expectation file: one entry per line, with blank lines, lines that
/// start with `#`, and connection lines ignored. Returns every entry, or every
/// error.
fn parse_expectations(text: &str) -> Result<Vec<Entry>, Vec<String>> {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for (line, content) in (1_usize..).zip(text.lines()) {
        let content = content.trim();
        if content.is_empty() || content.starts_with('#') || is_connection_line(content) {
            continue;
        }
        match parse_entry(line, content) {
            Ok(entry) => entries.push(entry),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(entries)
    } else {
        Err(errors)
    }
}

/// Whether `content`, a trimmed line of the expectation file, is a connection line:
/// its first field is [`CONNECTION_FIELD`].
fn is_connection_line(content: &str) -> bool {
    content.split_whitespace().next() == Some(CONNECTION_FIELD)
}

/// One connection line of the expectation file: a connection that the run must
/// record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedConnection {
    /// The line, counting from one.
    line: usize,
    /// The platforms that expect the connection.
    platform: Platform,
    /// The destination, as [`connection_text`] writes a recorded one.
    destination: String,
    /// Where the call that made the connection comes from.
    origin: Origin,
    /// The call that made the connection, one of [`CONNECTING_CALLS`].
    call: String,
}

/// Whether `text` is a destination as the expectation file writes it: an IPv4 or
/// IPv6 address and port in the form that the standard library writes it,
/// `unix:` and a normalized relative path, or `abstract:` and a name.
fn is_destination(text: &str) -> bool {
    if let Some(path) = text.strip_prefix("unix:") {
        return is_normal_relative(path);
    }
    if let Some(name) = text.strip_prefix("abstract:") {
        return !name.is_empty();
    }
    text.parse::<std::net::SocketAddr>()
        .is_ok_and(|address| address.to_string() == text)
}

/// Parse one connection line, the `line`-th of the file:
/// `connection PLATFORM DESTINATION ORIGIN:CALL`.
fn parse_connection(
    line: usize,
    text: &str,
) -> Result<ExpectedConnection, String> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [keyword, platform, destination, source] = fields.as_slice() else {
        return Err(format!(
            "line {line}: a connection is {CONNECTION_FIELD} PLATFORM DESTINATION ORIGIN:CALL"
        ));
    };
    if *keyword != CONNECTION_FIELD {
        return Err(format!("line {line}: `{text}` is not a connection line"));
    }
    let (platform, needs) =
        parse_platform(platform).map_err(|error| format!("line {line}: {error}"))?;
    if !needs.is_empty() {
        return Err(format!("line {line}: a connection carries no mark"));
    }
    if !is_destination(destination) {
        return Err(format!("line {line}: `{destination}` is not a destination"));
    }
    let origin = match source.split_once(':') {
        Some(("sys", call)) if CONNECTING_CALLS.contains(&call) => (Origin::Sys, call),
        Some(("libc", call)) if CONNECTING_CALLS.contains(&call) => (Origin::Libc, call),
        _ => {
            return Err(format!(
                "line {line}: `{source}` is not sys or libc and a call that connects"
            ));
        },
    };
    Ok(ExpectedConnection {
        line,
        platform,
        destination: (*destination).to_owned(),
        origin: origin.0,
        call: origin.1.to_owned(),
    })
}

/// Parse the connection lines of an expectation file. Returns every connection,
/// or every error.
fn parse_connections(text: &str) -> Result<Vec<ExpectedConnection>, Vec<String>> {
    let mut connections = Vec::new();
    let mut errors = Vec::new();
    for (line, content) in (1_usize..).zip(text.lines()) {
        let content = content.trim();
        if !is_connection_line(content) {
            continue;
        }
        match parse_connection(line, content) {
            Ok(connection) => connections.push(connection),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(connections)
    } else {
        Err(errors)
    }
}

/// The problems of `connections` as a whole: a destination listed twice for a
/// platform, and a call of [`CONNECTING_CALLS`] without a connection that it makes
/// through `syscall()` on Linux.
fn connection_problems(connections: &[ExpectedConnection]) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, connection) in connections.iter().enumerate() {
        for other in connections.iter().skip(index.saturating_add(1)) {
            if connection.destination == other.destination
                && connection.platform.overlaps(other.platform)
            {
                problems.push(format!(
                    "lines {} and {}: {} is listed twice for a platform",
                    connection.line, other.line, connection.destination
                ));
            }
        }
    }
    for call in CONNECTING_CALLS {
        let made = connections.iter().any(|connection| {
            connection.origin == Origin::Sys
                && connection.call == call
                && connection.platform.overlaps(Platform::Linux)
        });
        if !made {
            problems.push(format!(
                "{call} has no connection that it makes through syscall() on Linux"
            ));
        }
    }
    problems
}

/// The connection `connection` of a record, as the expectation file writes it, with
/// a Unix socket path relative to `dir`, or absolute when it is outside.
fn connection_text(
    connection: &Connection,
    dir: &Path,
) -> String {
    match connection {
        Connection::UnixPath { path } => {
            format!("unix:{}", path.strip_prefix(dir).unwrap_or(path).display())
        },
        Connection::UnixAbstract { name } => format!("abstract:{}", name.escape_ascii()),
        Connection::Ipv4 { .. } | Connection::Ipv6 { .. } => connection.to_string(),
    }
}

/// The connections of `record`, as the expectation file writes them, relative to
/// `dir`.
fn recorded_connections(
    record: &StepRecord,
    dir: &Path,
) -> BTreeSet<String> {
    record
        .connections
        .iter()
        .map(|connection| connection_text(connection, dir))
        .collect()
}

/// The destinations of `connections` for the platform the test runs on.
fn expected_connections_here(connections: &[ExpectedConnection]) -> BTreeSet<String> {
    connections
        .iter()
        .filter(|connection| connection.platform.is_current())
        .map(|connection| connection.destination.clone())
        .collect()
}

/// Every difference between the `expected` and the `recorded` connections: each
/// expected one that was not recorded, and each recorded one that was not
/// expected.
fn connection_differences(
    expected: &BTreeSet<String>,
    recorded: &BTreeSet<String>,
) -> Vec<String> {
    let missing = expected
        .difference(recorded)
        .map(|destination| format!("missing connection: {destination}"));
    let unexpected = recorded
        .difference(expected)
        .map(|destination| format!("unexpected connection: {destination}"));
    missing.chain(unexpected).collect()
}

/// The problems of `entries` as a whole, given the names of the tracked system
/// calls: an entry's kinds that are not the union of its sources' kinds, a system
/// call that is not tracked, an implicit load that is not known, a failed call
/// that adds more than a probe, unless it adds only the xattr kind and failed with
/// one of [`ATTRIBUTE_ANSWERS`], a start by kizz that is not a successful `execve`,
/// the problems of a mark that [`mark_problems`] finds, and a path listed twice for
/// a platform.
fn consistency_problems(
    entries: &[Entry],
    tracked: &[&str],
) -> Vec<String> {
    let mut problems = Vec::new();
    for entry in entries {
        let mut union = Kinds::default();
        for source in &entry.sources {
            union.extend(&source.kinds);
            if source.origin == Origin::Sys && !tracked.contains(&source.name.as_str()) {
                problems.push(format!(
                    "line {}: {} is not a tracked system call",
                    entry.line, source.name
                ));
            }
            if source.origin == Origin::Kernel
                && !IMPLICIT_LOADS.iter().any(|(name, _)| *name == source.name)
            {
                problems.push(format!(
                    "line {}: {} is not a known implicit load",
                    entry.line, source.name
                ));
            }
            let answered = source.kinds == Kinds(BTreeSet::from([Kind::Xattr]))
                && ATTRIBUTE_ANSWERS.contains(&source.outcome.as_str());
            let probed = source.kinds == Kinds(BTreeSet::from([Kind::Probe]));
            if !source.succeeded() && !probed && !answered {
                problems.push(format!(
                    "line {}: the failed call {} adds {}, not a probe",
                    entry.line, source.name, source.kinds
                ));
            }
            if source.origin == Origin::Kizz && (source.name != "execve" || !source.succeeded()) {
                problems.push(format!(
                    "line {}: kizz starts a program only with a successful execve",
                    entry.line
                ));
            }
        }
        if union != entry.kinds {
            problems.push(format!(
                "line {}: {} has the kinds {}, but its sources add {union}",
                entry.line, entry.path, entry.kinds
            ));
        }
        problems.extend(mark_problems(entry));
    }
    for (index, entry) in entries.iter().enumerate() {
        for other in entries.iter().skip(index.saturating_add(1)) {
            if entry.path == other.path && entry.platform.overlaps(other.platform) {
                problems.push(format!(
                    "lines {} and {}: {} is listed twice for a platform",
                    entry.line, other.line, entry.path
                ));
            }
        }
    }
    problems
}

/// The problems of the mark of `entry`: a name that is not one of [`GATED_CALLS`],
/// a name given twice, and a source of a gated call that the mark does not name.
fn mark_problems(entry: &Entry) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, name) in entry.needs.iter().enumerate() {
        if !GATED_CALLS.contains(&name.as_str()) {
            problems.push(format!(
                "line {}: the mark names {name}, which is not a gated call",
                entry.line
            ));
        }
        if entry
            .needs
            .iter()
            .take(index)
            .any(|earlier| earlier == name)
        {
            problems.push(format!("line {}: the mark names {name} twice", entry.line));
        }
    }
    for source in &entry.sources {
        let gated = source.origin == Origin::Sys && GATED_CALLS.contains(&source.name.as_str());
        if gated && !entry.needs.contains(&source.name) {
            problems.push(format!(
                "line {}: the gated call {} is a source, but the mark does not name it",
                entry.line, source.name
            ));
        }
    }
    problems
}

/// Whether a tracer that missed the sources that `missed` selects would record
/// `entry` differently: they are its only sources, or add a kind that no other
/// source adds.
fn detects(
    entry: &Entry,
    missed: impl Fn(&Source) -> bool,
) -> bool {
    let mut own = Kinds::default();
    let mut others = Kinds::default();
    let mut has_others = false;
    for source in &entry.sources {
        if missed(source) {
            own.extend(&source.kinds);
        } else {
            others.extend(&source.kinds);
            has_others = true;
        }
    }
    !has_others || !own.is_subset(&others)
}

/// The problems of the coverage of `tracked` by `entries`, which are the entries for
/// one platform: a call without an entry on which it succeeded, or for a call that
/// takes a path, failed with `ENOENT`, and that would be recorded differently
/// without it; and an implicit load of [`IMPLICIT_LOADS`] without an entry with
/// its outcome that would be recorded differently without it.
fn coverage_problems(
    entries: &[&Entry],
    tracked: &[&str],
) -> Vec<String> {
    let mut problems = Vec::new();
    for name in tracked {
        let mut outcomes = vec!["ok"];
        if !DESCRIPTOR_CALLS.contains(name) {
            outcomes.push("ENOENT");
        }
        for outcome in outcomes {
            let detected = entries.iter().any(|entry| {
                entry
                    .sources
                    .iter()
                    .any(|source| source.is_system_call(name) && source.outcome == outcome)
                    && detects(entry, |source| source.is_system_call(name))
            });
            if !detected {
                problems.push(format!(
                    "{name} has no entry with outcome {outcome} that only it explains"
                ));
            }
        }
    }
    for (name, outcome) in IMPLICIT_LOADS {
        let selects = |source: &Source| source.is_implicit_load(name) && source.outcome == outcome;
        let detected = entries
            .iter()
            .any(|entry| entry.sources.iter().any(selects) && detects(entry, selects));
        if !detected {
            problems.push(format!(
                "the implicit load {name} has no entry with outcome {outcome} that only it explains"
            ));
        }
    }
    problems
}

/// The tracked system calls that `architecture.md`'s text lists: every name in
/// backticks in the bullets under [`TRACKED_HEADING`], in order.
fn spec_tracked_calls(text: &str) -> Result<Vec<String>, String> {
    spec_calls(text, TRACKED_HEADING)
}

/// The system calls that `architecture.md`'s text lists under `heading`: every name
/// in backticks in the bullets after that line, with their continuation lines, in
/// order.
fn spec_calls(
    text: &str,
    heading: &str,
) -> Result<Vec<String>, String> {
    let mut lines = text.lines().skip_while(|line| *line != heading).skip(1);
    if text.lines().all(|line| line != heading) {
        return Err(format!("architecture.md has no line `{heading}`"));
    }
    let mut names = Vec::new();
    let mut started = false;
    for line in lines.by_ref() {
        if line.starts_with("* ") || (started && line.starts_with("  ")) {
            started = true;
            names.extend(line.split('`').skip(1).step_by(2).map(str::to_owned));
        } else if started || !line.is_empty() {
            break;
        }
    }
    if names.is_empty() {
        return Err(format!(
            "architecture.md lists no system call under `{heading}`"
        ));
    }
    Ok(names)
}

/// What a path's record holds: its kinds and its state, with its attributes.
type Recorded = BTreeMap<String, (Kinds, Observed)>;

/// One difference between the expected and the recorded paths.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Difference {
    /// An expected path that was not recorded.
    Missing {
        /// The path.
        path: String,
        /// The expected kinds and state.
        expected: (Kinds, Observed),
    },
    /// A recorded path that was not expected.
    Unexpected {
        /// The path.
        path: String,
        /// The recorded kinds and state.
        recorded: (Kinds, Observed),
    },
    /// A path recorded with other kinds or another state than expected.
    Mismatched {
        /// The path.
        path: String,
        /// The expected kinds and state.
        expected: (Kinds, Observed),
        /// The recorded kinds and state.
        recorded: (Kinds, Observed),
    },
}

impl fmt::Display for Difference {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Missing { path, expected } => {
                write!(f, "missing: {path}, expected {} {}", expected.0, expected.1)
            },
            Self::Unexpected { path, recorded } => {
                write!(
                    f,
                    "unexpected: {path}, recorded {} {}",
                    recorded.0, recorded.1
                )
            },
            Self::Mismatched {
                path,
                expected,
                recorded,
            } => {
                write!(
                    f,
                    "mismatched: {path}, expected {} {}, recorded {} {}",
                    expected.0, expected.1, recorded.0, recorded.1
                )
            },
        }
    }
}

/// Every difference between `expected` and `recorded`, in path order.
fn compare(
    expected: &Recorded,
    recorded: &Recorded,
) -> Vec<Difference> {
    let paths: BTreeSet<&String> = expected.keys().chain(recorded.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            match (expected.get(path), recorded.get(path)) {
                (Some(want), None) => {
                    Some(Difference::Missing {
                        path: path.clone(),
                        expected: want.clone(),
                    })
                },
                (None, Some(got)) => {
                    Some(Difference::Unexpected {
                        path: path.clone(),
                        recorded: got.clone(),
                    })
                },
                (Some(want), Some(got)) if want != got => {
                    Some(Difference::Mismatched {
                        path: path.clone(),
                        expected: want.clone(),
                        recorded: got.clone(),
                    })
                },
                _ => None,
            }
        })
        .collect()
}

/// The paths of `record` under `dir`, relative to it, with their kinds and states.
/// `dir` itself is `.`.
fn recorded_under(
    record: &StepRecord,
    dir: &Path,
) -> Recorded {
    record
        .observations
        .iter()
        .filter_map(|observation| {
            let relative = observation.path.strip_prefix(dir).ok()?;
            let name = if relative.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                relative.to_string_lossy().into_owned()
            };
            Some((
                name,
                (Kinds::from_bits(observation.access), Observed {
                    state: State::from_recorded(&observation.state),
                    attributes: observation
                        .attributes
                        .as_ref()
                        .map(AttributeNames::from_recorded),
                }),
            ))
        })
        .collect()
}

/// The directory of the fixture in the repository.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests")
        .join(FIXTURE)
}

/// The entries of the fixture's expectation file.
fn fixture_entries() -> TestResult<Vec<Entry>> {
    let text = fs::read_to_string(fixture_dir().join(EXPECTATIONS))?;
    parse_expectations(&text).map_err(|errors| errors.join("\n").into())
}

/// The connections of the fixture's expectation file.
fn fixture_connections() -> TestResult<Vec<ExpectedConnection>> {
    let text = fs::read_to_string(fixture_dir().join(EXPECTATIONS))?;
    parse_connections(&text).map_err(|errors| errors.join("\n").into())
}

/// The names of [`TRACKED_SYSTEM_CALLS`].
fn tracked_names() -> Vec<&'static str> {
    TRACKED_SYSTEM_CALLS.iter().map(|call| call.name).collect()
}

/// Copy the directory `from` to `to`, which must not exist, recreating symbolic
/// links as links.
fn copy_tree(
    from: &Path,
    to: &Path,
) -> TestResult {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            symlink(fs::read_link(&source)?, &destination)?;
        } else if file_type.is_dir() {
            copy_tree(&source, &destination)?;
        } else {
            fs::copy(&source, &destination)?;
        }
    }
    Ok(())
}

/// What one run of `kizz` produced.
#[derive(Debug)]
struct Run {
    /// The arguments after the global options.
    args: Vec<String>,
    /// The exit code, or `None` when a signal killed the process.
    status: Option<i32>,
    /// Standard output, with invalid UTF-8 replaced.
    stdout: String,
    /// Standard error, with invalid UTF-8 replaced.
    stderr: String,
}

impl fmt::Display for Run {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        writeln!(
            f,
            "kizz {} exited with {:?}",
            self.args.join(" "),
            self.status
        )?;
        writeln!(f, "standard output:\n{}", self.stdout)?;
        writeln!(f, "standard error:\n{}", self.stderr)
    }
}

/// Run `kizz` with `args` in `dir`, with the cache `cache`, `PATH`, and no other
/// environment, and with no standard input.
fn run_kizz(
    dir: &Path,
    cache: &Path,
    args: &[&str],
) -> TestResult<Run> {
    let output = Command::new(env!("CARGO_BIN_EXE_kizz"))
        .args(PLAIN)
        .args(args)
        .env_clear()
        .env("KIZZ_CACHE_DIR", cache)
        .env("PATH", PATH)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()?;
    Ok(Run {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Every step record of the cache `cache`, with the database closed again.
fn step_records(cache: &Path) -> TestResult<Vec<(StepKey, StepRecord)>> {
    let store = Store::open(cache)?;
    let steps = {
        let txn = store.read()?;
        txn.scan(&[])?
    };
    store.close()?;
    Ok(steps)
}

/// The captured output of `record`, both streams.
fn output_of(record: &StepRecord) -> String {
    match &record.output {
        RecordedOutput::Merged { output } => String::from_utf8_lossy(output).into_owned(),
        RecordedOutput::Split { stdout, stderr } => {
            format!(
                "{}{}",
                String::from_utf8_lossy(stdout),
                String::from_utf8_lossy(stderr)
            )
        },
    }
}

/// The problems of the one step record that `coverage-test`, the task `task`,
/// recorded in `steps`, and its captured output: it must be cached, so complete,
/// and list no system call that the tracer does not know.
fn record_problems(
    steps: &[(StepKey, StepRecord)],
    task: &TaskId,
) -> (Vec<String>, Option<StepRecord>) {
    let mut problems = Vec::new();
    if steps.len() != 1 {
        problems.push(format!(
            "the cache holds {} step records after building coverage-test, not one",
            steps.len()
        ));
    }
    let Some((_, record)) = steps
        .iter()
        .find(|(_, record)| record.owners.contains(task))
    else {
        problems.push("no step record is owned by the coverage-test task".to_owned());
        return (problems, None);
    };
    if record.owners != BTreeSet::from([task.clone()]) {
        problems.push(format!(
            "the step record is owned by {:?}, not by the coverage-test task alone",
            record.owners
        ));
    }
    if record.exit != (StepExit::Exited { code: 0 }) {
        problems.push(format!("the program ended with {:?}", record.exit));
    }
    if let Some(reason) = &record.not_cached {
        problems.push(format!("the result is not cached: {reason}"));
    }
    if !record.unknown_system_calls.is_empty() {
        problems.push(format!(
            "the step record lists unknown system calls: {:?}",
            record.unknown_system_calls
        ));
    }
    if record.ignored != 0 {
        problems.push(format!("{} accesses were ignored", record.ignored));
    }
    if !output_of(record)
        .lines()
        .any(|line| line.starts_with(SUCCESS_LINE))
    {
        problems.push("the program did not print its summary line".to_owned());
    }
    (problems, Some(record.clone()))
}

/// Whether each gated call is available, by name: unavailable when its check
/// returned `ENOSYS`.
type Availability = BTreeMap<String, bool>;

/// The expected kinds and states of `entries` for the platform the test runs on,
/// where every gated call that an entry's mark names is available in `available`.
fn expected_here(
    entries: &[Entry],
    available: &Availability,
) -> Recorded {
    entries
        .iter()
        .filter(|entry| {
            entry.platform.is_current()
                && entry
                    .needs
                    .iter()
                    .all(|name| available.get(name) == Some(&true))
        })
        .map(|entry| {
            (
                entry.path.clone(),
                (entry.kinds.clone(), entry.state.clone()),
            )
        })
        .collect()
}

/// The address of `pointer`, as a system-call argument.
fn address<T>(pointer: *const T) -> c_long {
    c_long::try_from(pointer.expose_provenance()).unwrap_or_default()
}

/// Make the gated call `name` once, in this process, which no tracer follows, on
/// [`CHECKED_PATH`], with arguments that the kernel rejects with `EINVAL` before it
/// looks the path up: a structure size of 0 for `getxattrat`, `setxattrat`,
/// `file_getattr` and `file_setattr`, and a flag that the call does not accept for
/// `listxattrat` and `removexattrat`. Returns the error number of the failure, or
/// `None` if the call succeeded.
fn check_call(name: &str) -> TestResult<Option<i32>> {
    let number = TRACKED_SYSTEM_CALLS
        .iter()
        .find(|call| call.name == name)
        .ok_or_else(|| format!("{name} is not tracked"))?
        .number;
    let mut buffer = [0_u8; 64];
    let place = address(buffer.as_mut_ptr().cast_const());
    let size = c_long::try_from(buffer.len())?;
    let path = address(CHECKED_PATH.as_ptr());
    let attribute = address(CHECKED_ATTRIBUTE.as_ptr());
    let cwd = c_long::from(libc::AT_FDCWD);
    let unaccepted = c_long::from(libc::AT_REMOVEDIR);
    let arguments: [c_long; 6] = match name {
        "file_getattr" | "file_setattr" => [cwd, path, place, 0, 0, 0],
        "getxattrat" | "setxattrat" => [cwd, path, 0, attribute, place, 0],
        "listxattrat" => [cwd, path, unaccepted, place, size, 0],
        "removexattrat" => [cwd, path, unaccepted, attribute, 0, 0],
        _ => return Err(format!("{name} is not a gated call").into()),
    };
    let [a0, a1, a2, a3, a4, a5] = arguments;
    // SAFETY: the pointers name live memory: two NUL-terminated strings and
    // `buffer`, which is writable for its whole length; the kernel rejects the
    // arguments before it uses them, or the call is unavailable.
    let result = unsafe { libc::syscall(number, a0, a1, a2, a3, a4, a5) };
    if result >= 0 {
        return Ok(None);
    }
    Ok(io::Error::last_os_error().raw_os_error())
}

/// The word for whether a call is `available`.
const fn availability_word(available: bool) -> &'static str {
    if available {
        "available"
    } else {
        "unavailable"
    }
}

/// Whether each of [`GATED_CALLS`] is available, checked in this process with
/// [`check_call`]: `ENOSYS` means that the kernel lacks the call, or that a seccomp
/// filter installed before kizz's own refuses it, so the call is unavailable, and
/// `EINVAL` that it is available; any other result is a problem, and the call then
/// counts as available.
fn own_availability() -> TestResult<(Availability, Vec<String>)> {
    let mut available = Availability::new();
    let mut problems = Vec::new();
    for name in GATED_CALLS {
        let error = check_call(name)?;
        let has = error != Some(libc::ENOSYS);
        if has && error != Some(libc::EINVAL) {
            let result = error.map_or_else(
                || "success".to_owned(),
                |error| io::Error::from_raw_os_error(error).to_string(),
            );
            problems.push(format!(
                "the test's check of {name} returned {result}, not EINVAL or ENOSYS"
            ));
        }
        available.insert(name.to_owned(), has);
    }
    Ok((available, problems))
}

/// The availability of each gated call that the program reported in `output`, in
/// its lines that start with [`AVAILABLE_LINE`] or [`UNAVAILABLE_LINE`], with the
/// problems of the report: a gated call without a line, a call with more than one,
/// and a name that is not a gated call.
fn reported_availability(output: &str) -> (Availability, Vec<String>) {
    let mut reported = Availability::new();
    let mut problems = Vec::new();
    for line in output.lines() {
        let parsed = line
            .strip_prefix(AVAILABLE_LINE)
            .map(|name| (name, true))
            .or_else(|| {
                line.strip_prefix(UNAVAILABLE_LINE)
                    .map(|name| (name, false))
            });
        let Some((name, has)) = parsed else {
            continue;
        };
        if !GATED_CALLS.contains(&name) {
            problems.push(format!(
                "the program reported {name}, which is not a gated call"
            ));
        } else if reported.insert(name.to_owned(), has).is_some() {
            problems.push(format!("the program reported {name} more than once"));
        }
    }
    for name in GATED_CALLS {
        if !reported.contains_key(name) {
            problems.push(format!(
                "the program did not report whether {name} is available"
            ));
        }
    }
    (reported, problems)
}

/// The gated calls whose availability the program's report, `reported`, gives
/// otherwise than the test's own check, `own`, each as a problem.
fn disagreements(
    reported: &Availability,
    own: &Availability,
) -> Vec<String> {
    GATED_CALLS
        .iter()
        .filter_map(|name| {
            let (by_program, by_test) = (*reported.get(*name)?, *own.get(*name)?);
            (by_program != by_test).then(|| {
                format!(
                    "the program reported {name} {}, but the test's own check found it {}",
                    availability_word(by_program),
                    availability_word(by_test)
                )
            })
        })
        .collect()
}

/// The notice that names `test` and each gated call that `available` marks
/// unavailable, or `None` when there is none.
fn unavailable_notice(
    test: &str,
    available: &Availability,
) -> Option<String> {
    let missing: Vec<&str> = GATED_CALLS
        .iter()
        .copied()
        .filter(|name| available.get(*name) == Some(&false))
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "NOTICE: {test}: {} returned ENOSYS (the kernel lacks the call, or a seccomp filter \
             installed before kizz's own refuses it), so the test required that nothing was \
             recorded at their paths instead of checking their accesses",
            missing.join(", ")
        )
    })
}

/// Write the notice of [`unavailable_notice`], if any, to the standard error of
/// this process through [`io::stderr`], which the test harness does not capture,
/// so the output of a passing run shows it.
fn write_notice(
    test: &str,
    available: &Availability,
) -> io::Result<()> {
    unavailable_notice(test, available).map_or(Ok(()), |notice| writeln!(io::stderr(), "{notice}"))
}

/// Run the fixture's two builds and the reset in a copy of it, and compare the
/// step that `coverage-test` recorded with the expectation file, leaving out the
/// entries that name an unavailable gated call, as the test's own check and the
/// program's report, which must agree, find. The step's connections must be
/// exactly those of the expectation file for the platform, and the build must warn
/// about them once, naming them, and repeat the warning in its summary. Every
/// problem is reported at once, with the output of `kizz` and of the program, and
/// a notice names each unavailable gated call.
#[test]
fn the_tracer_records_every_access_of_the_coverage_program() -> TestResult {
    let (available, mut problems) = own_availability()?;
    write_notice(
        "the_tracer_records_every_access_of_the_coverage_program",
        &available,
    )?;
    let entries = fixture_entries()?;
    let connections = fixture_connections()?;
    let dir = tempfile::tempdir()?;
    let copy = dir.path().canonicalize()?.join(FIXTURE);
    copy_tree(&fixture_dir(), &copy)?;
    let cache = tempfile::tempdir()?;
    let copy_text = copy.to_str().ok_or("the copy's path is not UTF-8")?;

    let mut runs = Vec::new();
    for args in [
        &["build", "coverage-build"][..],
        &["cache", "reset", copy_text, "--yes"][..],
    ] {
        let run = run_kizz(&copy, cache.path(), args)?;
        if run.status != Some(0) {
            problems.push(format!("kizz {} failed", args.join(" ")));
        }
        runs.push(run);
    }
    let after_reset = step_records(cache.path())?;
    if !after_reset.is_empty() {
        problems.push(format!("the reset left {} step records", after_reset.len()));
    }
    let run = run_kizz(&copy, cache.path(), &["build", "coverage-test"])?;
    if run.status != Some(0) {
        problems.push("kizz build coverage-test failed".to_owned());
    }
    problems.extend(warning_problems(&run, &[
        "`./coverage`",
        "connected to",
        "127.0.0.1:47011",
        "127.0.0.1:47012",
        "Unix socket `out/core/stream.sock`",
        "kizz cannot see what came back, and its result is still cached",
        "`always = True` runs a command on every build",
    ]));
    runs.push(run);

    let task = TaskId {
        file: copy.join("kizz.md"),
        index: TEST_BLOCK,
    };
    let (found, record) = record_problems(&step_records(cache.path())?, &task);
    problems.extend(found);
    let mut report = String::new();
    if let Some(record) = &record {
        let (reported, report_problems) = reported_availability(&output_of(record));
        problems.extend(report_problems);
        problems.extend(disagreements(&reported, &available));
        let expected = expected_here(&entries, &available);
        let differences = compare(&expected, &recorded_under(record, &copy));
        problems.extend(differences.iter().map(ToString::to_string));
        problems.extend(loader_problems(record, &copy));
        problems.extend(connection_differences(
            &expected_connections_here(&connections),
            &recorded_connections(record, &copy),
        ));
        writeln!(report, "program output:\n{}", output_of(record))?;
    }
    for run in &runs {
        writeln!(report, "{run}")?;
    }
    assert!(
        problems.is_empty(),
        "{} problems:\n{}\n\n{report}",
        problems.len(),
        problems.join("\n")
    );
    Ok(())
}

/// The problems of the step record of the `coverage-changes` step in `mode`, the
/// task `task`, in `steps`: it must have ended with exit code 0 after the program
/// printed its summary line, and, when `rewritten` names a file under `copy`, not
/// be cached, for the one reason that the command wrote that file after it first
/// read it; the control must be cached.
fn change_problems(
    steps: &[(StepKey, StepRecord)],
    task: &TaskId,
    copy: &Path,
    mode: &str,
    rewritten: Option<&str>,
) -> Vec<String> {
    let argv = ["./coverage".to_owned(), format!("change-{mode}")];
    let Some((_, record)) = steps
        .iter()
        .find(|(_, record)| record.spec.argv == argv && record.owners.contains(task))
    else {
        return vec![format!("no step record of the {mode} step")];
    };
    let mut problems = Vec::new();
    if record.exit != (StepExit::Exited { code: 0 }) {
        problems.push(format!("the {mode} step ended with {:?}", record.exit));
    }
    if !output_of(record)
        .lines()
        .any(|line| line.starts_with(SUCCESS_LINE))
    {
        problems.push(format!(
            "the {mode} step did not print its summary line: {}",
            output_of(record)
        ));
    }
    let expected = rewritten.map(|file| {
        NotCachedReason::ChangedAfterRead {
            files: vec![ChangedRead {
                path: copy.join(file),
                written: true,
            }],
        }
    });
    if record.not_cached != expected {
        problems.push(format!(
            "the {mode} step is marked {:?}, not {expected:?}",
            record.not_cached
        ));
    }
    problems
}

/// Build `coverage-changes` in a copy of the fixture, after `coverage-build` and a
/// reset: each step that rewrites the file it first read or ran is not cached, with
/// the reason that names that file and the command as its writer, without a
/// warning, and the control, which writes its file before it reads it, is cached.
#[test]
fn each_way_of_first_reading_a_file_is_checked_for_a_rewrite() -> TestResult {
    let dir = tempfile::tempdir()?;
    let copy = dir.path().canonicalize()?.join(FIXTURE);
    copy_tree(&fixture_dir(), &copy)?;
    let cache = tempfile::tempdir()?;
    let copy_text = copy.to_str().ok_or("the copy's path is not UTF-8")?;

    let mut problems = Vec::new();
    let mut runs = Vec::new();
    for args in [
        &["build", "coverage-build"][..],
        &["cache", "reset", copy_text, "--yes"][..],
        &["build", "coverage-changes"][..],
    ] {
        let run = run_kizz(&copy, cache.path(), args)?;
        if run.status != Some(0) {
            problems.push(format!("kizz {} failed", args.join(" ")));
        }
        runs.push(run);
    }
    if runs
        .iter()
        .any(|run| run.stderr.contains("something else changed"))
    {
        problems.push("a step that rewrote its own input was warned about".to_owned());
    }
    let steps = step_records(cache.path())?;
    if steps.len() != CHANGES.len() {
        problems.push(format!(
            "the cache holds {} step records after building coverage-changes, not {}",
            steps.len(),
            CHANGES.len()
        ));
    }
    let task = TaskId {
        file: copy.join("kizz.md"),
        index: CHANGES_BLOCK,
    };
    for (mode, rewritten) in CHANGES {
        problems.extend(change_problems(&steps, &task, &copy, mode, rewritten));
    }
    let mut report = String::new();
    for run in &runs {
        writeln!(report, "{run}")?;
    }
    assert!(
        problems.is_empty(),
        "{} problems:\n{}\n\n{report}",
        problems.len(),
        problems.join("\n")
    );
    Ok(())
}

/// The `x86_64` number of `name`, a call that marks a run.
fn untraceable_number(name: &str) -> TestResult<c_long> {
    Ok(UNTRACEABLE_SYSTEM_CALLS
        .iter()
        .find(|call| call.name == name)
        .ok_or_else(|| format!("{name} does not mark a run"))?
        .number)
}

/// Whether each of [`GATED_UNTRACEABLE`] is available, checked in this process,
/// which no tracer follows, with arguments that cannot succeed: `fchroot` on an
/// invalid descriptor, and `open_tree_attr` on `/` with a flag that it does not
/// accept. `ENOSYS` means that the kernel lacks the call, or that a seccomp filter
/// installed before kizz's own refuses it, so the call is unavailable; any other
/// result means that it is available.
fn own_untraceable_availability() -> TestResult<Availability> {
    let mut available = Availability::new();
    for name in GATED_UNTRACEABLE {
        let number = untraceable_number(name)?;
        let arguments: [c_long; 3] = match name {
            "fchroot" => [-1, 0, 0],
            _ => {
                [
                    c_long::from(libc::AT_FDCWD),
                    address(CHECKED_PATH.as_ptr()),
                    c_long::from(libc::AT_REMOVEDIR),
                ]
            },
        };
        let [a0, a1, a2] = arguments;
        // SAFETY: the one pointer names a NUL-terminated string; the kernel rejects
        // the arguments before it acts, or the call is unavailable.
        let result = unsafe { libc::syscall(number, a0, a1, a2, 0, 0, 0) };
        let error = (result < 0)
            .then(|| io::Error::last_os_error().raw_os_error())
            .flatten();
        available.insert(name.to_owned(), error != Some(libc::ENOSYS));
    }
    Ok(available)
}

/// Write to the standard error of this process, through [`io::stderr`], which the
/// test harness does not capture, a notice that names `test` and each of
/// [`GATED_UNTRACEABLE`] that `available` marks unavailable, if any.
fn write_untraceable_notice(
    test: &str,
    available: &Availability,
) -> io::Result<()> {
    let missing: Vec<&str> = GATED_UNTRACEABLE
        .iter()
        .copied()
        .filter(|name| available.get(*name) == Some(&false))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    writeln!(
        io::stderr(),
        "NOTICE: {test}: {} returned ENOSYS (the kernel lacks the call, or a seccomp filter \
         installed before kizz's own refuses it), so the test did not require that their \
         steps mark the run",
        missing.join(", ")
    )
}

/// The step record of the `coverage-incomplete` step that runs the program in
/// `mode`, the task `task`, in `steps`.
fn incomplete_record<'a>(
    steps: &'a [(StepKey, StepRecord)],
    task: &TaskId,
    mode: &str,
) -> Option<&'a StepRecord> {
    let argv = ["./coverage".to_owned(), mode.to_owned()];
    steps
        .iter()
        .find(|(_, record)| record.spec.argv == argv && record.owners.contains(task))
        .map(|(_, record)| record)
}

/// Whether `value`, the value of [`SKIP_VARIABLE`], or `None` when it is unset,
/// turns skipping on: only `1` does, and unset, empty or any other value leaves the
/// tests strict.
fn skips_blocked(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

/// Whether this test process skips the calls and numbers that the tracer never
/// saw: the one place that reads [`SKIP_VARIABLE`], from the environment that the
/// test process was started with.
fn skip_mode() -> bool {
    skips_blocked(std::env::var_os(SKIP_VARIABLE).as_deref())
}

/// A call or number that a step of `coverage-incomplete` makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Made {
    /// A call that marks a run, by name.
    Call(&'static str),
    /// A system call that the tracer does not know, by number.
    Number(u32),
}

impl Made {
    /// The mode of the program that makes it.
    fn mode(self) -> String {
        match self {
            Self::Call(name) => format!("incomplete-{name}"),
            Self::Number(number) => format!("unknown-{number}"),
        }
    }

    /// The start of the line in which that mode reports what the call returned,
    /// before the result.
    fn report(self) -> String {
        match self {
            Self::Call(name) => format!("coverage: made {name}: "),
            Self::Number(number) => format!("coverage: made system call {number}: "),
        }
    }

    /// The call in a message: a name in backticks, or a number after "system call".
    fn quoted(self) -> String {
        match self {
            Self::Call(name) => format!("`{name}`"),
            Self::Number(number) => format!("system call {number}"),
        }
    }
}

impl fmt::Display for Made {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Call(name) => f.write_str(name),
            Self::Number(number) => write!(f, "system call {number}"),
        }
    }
}

/// Whether the tracer saw the call or number that the `coverage-incomplete` step of
/// `record` made, judged in the build that ran it: only a step that is cached and
/// lists no system call that the tracer does not know made a call that the tracer
/// never saw. A step that is not cached, for any reason, or that lists an unknown
/// system call, made one that it saw, and gets the full check.
fn tracer_saw(record: &StepRecord) -> bool {
    record.not_cached.is_some() || !record.unknown_system_calls.is_empty()
}

/// The problems of the step of `record`, which made `made` and whose call the
/// tracer never saw, that hold whether or not the tests skip it: it must have
/// exited with 0, and its mode must have reported the call once, with an error,
/// because a call that a filter refuses fails.
fn unseen_problems(
    record: &StepRecord,
    made: Made,
) -> Vec<String> {
    let mut problems = Vec::new();
    if record.exit != (StepExit::Exited { code: 0 }) {
        problems.push(format!("the step of {made} ended with {:?}", record.exit));
    }
    let output = output_of(record);
    let report = made.report();
    let results: Vec<&str> = output
        .lines()
        .filter_map(|line| line.strip_prefix(report.as_str()))
        .collect();
    if !matches!(results.as_slice(), [result] if *result != "success") {
        problems.push(format!(
            "the tracer never saw {}, but its step did not report it once with an error: \
             {output}",
            made.quoted()
        ));
    }
    problems
}

/// The calls and numbers that a test skipped because the tracer never saw them, and
/// whether it may skip them.
#[derive(Debug)]
struct Skips {
    /// Whether [`SKIP_VARIABLE`] is `1`, as [`skips_blocked`] reads it.
    skip: bool,
    /// Each call and number skipped so far, in order.
    skipped: Vec<Made>,
}

impl Skips {
    /// No call skipped yet, in the mode `skip`.
    const fn new(skip: bool) -> Self {
        Self {
            skip,
            skipped: Vec::new(),
        }
    }

    /// The problems of `made`, which the tracer never saw, with `ran`, the problems
    /// of its step that hold in both modes: with skipping off, also a problem that
    /// names it and [`SKIP_VARIABLE`]; with skipping on, it is skipped instead, unless
    /// `ran` holds a problem, which fails the test anyway.
    fn never_saw(
        &mut self,
        made: Made,
        ran: Vec<String>,
    ) -> Vec<String> {
        let mut problems = ran;
        if self.skip {
            if problems.is_empty() {
                self.skipped.push(made);
            }
        } else {
            problems.push(format!(
                "the tracer never saw {}; if a seccomp filter installed before kizz refuses it, \
                 set {SKIP_VARIABLE}=1",
                made.quoted()
            ));
        }
        problems
    }

    /// The notice that names `test`, [`SKIP_VARIABLE`] and each call and number
    /// skipped, or `None` when none was.
    fn notice(
        &self,
        test: &str,
    ) -> Option<String> {
        (!self.skipped.is_empty()).then(|| {
            let skipped: Vec<String> = self.skipped.iter().map(ToString::to_string).collect();
            format!(
                "NOTICE: {test}: the tracer never saw {}, which a seccomp filter installed \
                 before kizz's own may have refused, so {SKIP_VARIABLE}=1 skipped their checks",
                skipped.join(", ")
            )
        })
    }

    /// Write the notice of [`Skips::notice`], if any, to the standard error of this
    /// process through [`io::stderr`], which the test harness does not capture, so
    /// the output of a passing run shows it.
    fn write_notice(
        &self,
        test: &str,
    ) -> io::Result<()> {
        self.notice(test)
            .map_or(Ok(()), |notice| writeln!(io::stderr(), "{notice}"))
    }
}

/// The problems of the step of `coverage-incomplete`, the task `task`, that makes
/// the call `name`, in `steps`, as [`call_problems`] gives them.
fn untraceable_problems(
    steps: &[(StepKey, StepRecord)],
    task: &TaskId,
    name: &'static str,
    available: bool,
    skips: &mut Skips,
) -> Vec<String> {
    let Some(record) = incomplete_record(steps, task, &Made::Call(name).mode()) else {
        return vec![format!("no step record of the {name} step")];
    };
    call_problems(record, name, available, skips)
}

/// The problems of `record`, the step of `coverage-incomplete` that makes the call
/// `name`, which `available` gives as available or not. For a gated call, the
/// program's report must agree with `available`. Where the call is available and
/// the tracer never saw it, [`Skips::never_saw`] judges it. Otherwise the step must
/// have exited with 0 and list no unknown system call; where the call is available,
/// its result must be not cached, with an incomplete reason that names the call,
/// and where it is unavailable, it may also be cached.
fn call_problems(
    record: &StepRecord,
    name: &'static str,
    available: bool,
    skips: &mut Skips,
) -> Vec<String> {
    let mut problems = Vec::new();
    if GATED_UNTRACEABLE.contains(&name) {
        let output = output_of(record);
        let reported: Vec<bool> = output
            .lines()
            .filter_map(|line| {
                if line == format!("{AVAILABLE_LINE}{name}") {
                    Some(true)
                } else if line == format!("{UNAVAILABLE_LINE}{name}") {
                    Some(false)
                } else {
                    None
                }
            })
            .collect();
        if reported != [available] {
            problems.push(format!(
                "the {name} step reported {reported:?}, but the test's own check found it {}: \
                 {output}",
                availability_word(available)
            ));
        }
    }
    if available && !tracer_saw(record) {
        let made = Made::Call(name);
        problems.extend(skips.never_saw(made, unseen_problems(record, made)));
        return problems;
    }
    if record.exit != (StepExit::Exited { code: 0 }) {
        problems.push(format!("the {name} step ended with {:?}", record.exit));
    }
    if !record.unknown_system_calls.is_empty() {
        problems.push(format!(
            "the {name} step lists unknown system calls: {:?}",
            record.unknown_system_calls
        ));
    }
    let named = format!("`{name}`");
    let names_call = matches!(
        &record.not_cached,
        Some(NotCachedReason::Incomplete { detail }) if detail.contains(&named)
    );
    if (available || record.not_cached.is_some()) && !names_call {
        problems.push(format!(
            "the {name} step is marked {:?}, not incompletely traced for {named}",
            record.not_cached
        ));
    }
    problems
}

/// The problems of the step of `coverage-incomplete`, the task `task`, that makes
/// the system call `number`, in `steps`, as [`number_problems`] gives them.
fn unknown_problems(
    steps: &[(StepKey, StepRecord)],
    task: &TaskId,
    number: u32,
    cached: bool,
    skips: &mut Skips,
) -> Vec<String> {
    let Some(record) = incomplete_record(steps, task, &Made::Number(number).mode()) else {
        return vec![format!(
            "no step record of the step of system call {number}"
        )];
    };
    number_problems(record, number, cached, skips)
}

/// The problems of `record`, the step of `coverage-incomplete` that makes the system
/// call `number`. Where the tracer never saw it, [`Skips::never_saw`] judges it.
/// Otherwise the step must have exited with 0, list only that number, and be not
/// cached for it, unless `cached`, when it must be cached.
fn number_problems(
    record: &StepRecord,
    number: u32,
    cached: bool,
    skips: &mut Skips,
) -> Vec<String> {
    if !tracer_saw(record) {
        let made = Made::Number(number);
        return skips.never_saw(made, unseen_problems(record, made));
    }
    let mut problems = Vec::new();
    if record.exit != (StepExit::Exited { code: 0 }) {
        problems.push(format!(
            "the step of system call {number} ended with {:?}",
            record.exit
        ));
    }
    if record.unknown_system_calls != [number] {
        problems.push(format!(
            "the step of system call {number} lists {:?}",
            record.unknown_system_calls
        ));
    }
    let expected = (!cached).then(|| {
        NotCachedReason::UnknownSystemCalls {
            numbers: vec![number],
        }
    });
    if record.not_cached != expected {
        problems.push(format!(
            "the step of system call {number} is marked {:?}, not {expected:?}",
            record.not_cached
        ));
    }
    problems
}

/// The problems of the warnings of `run` that hold every one of `parts`: there must
/// be exactly one, which starts with `warning: `, and the summary must repeat it
/// once, indented by two spaces.
fn warning_problems(
    run: &Run,
    parts: &[&str],
) -> Vec<String> {
    let holds = |line: &str| parts.iter().all(|part| line.contains(part));
    let warned = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("warning: ") && holds(line))
        .count();
    let repeated = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("  ") && holds(line))
        .count();
    if (warned, repeated) == (1, 1) {
        Vec::new()
    } else {
        vec![format!(
            "kizz {} warned {warned} times and repeated it {repeated} times in its summary, not \
             once each, about {parts:?}",
            run.args.join(" ")
        )]
    }
}

/// A copy of the fixture with its cache after `coverage-build`, a reset, and a
/// build of `coverage-incomplete`.
struct IncompleteBuild {
    /// The temporary directory that holds the copy.
    _dir: tempfile::TempDir,
    /// The copy, the task directory.
    copy: PathBuf,
    /// The cache.
    cache: tempfile::TempDir,
    /// The runs of `kizz`, in order.
    runs: Vec<Run>,
    /// The problems found so far: each run that failed.
    problems: Vec<String>,
}

impl IncompleteBuild {
    /// Copy the fixture, build `coverage-build`, reset the copy's entries, and build
    /// `coverage-incomplete` with `options` before the target.
    fn new(options: &[&str]) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let copy = dir.path().canonicalize()?.join(FIXTURE);
        copy_tree(&fixture_dir(), &copy)?;
        let cache = tempfile::tempdir()?;
        let copy_text = copy.to_str().ok_or("the copy's path is not UTF-8")?;
        let mut incomplete: Vec<&str> = vec!["build"];
        incomplete.extend_from_slice(options);
        incomplete.push("coverage-incomplete");
        let mut built = Self {
            _dir: dir,
            copy: copy.clone(),
            cache,
            runs: Vec::new(),
            problems: Vec::new(),
        };
        for args in [
            &["build", "coverage-build"][..],
            &["cache", "reset", copy_text, "--yes"][..],
            &incomplete[..],
        ] {
            built.run(args)?;
        }
        Ok(built)
    }

    /// Run `kizz` with `args` in the copy and keep the run, with a problem when it
    /// fails.
    fn run(
        &mut self,
        args: &[&str],
    ) -> TestResult<&Run> {
        let run = run_kizz(&self.copy, self.cache.path(), args)?;
        if run.status != Some(0) {
            self.problems
                .push(format!("kizz {} failed", args.join(" ")));
        }
        self.runs.push(run);
        self.runs.last().ok_or_else(|| "no run".into())
    }

    /// The task `coverage-incomplete`.
    fn task(&self) -> TaskId {
        TaskId {
            file: self.copy.join("kizz.md"),
            index: INCOMPLETE_BLOCK,
        }
    }

    /// The problems of the steps of the calls that mark a run in `steps`, whose
    /// availability `available` gives, with the skips of `skips`, and of the
    /// warnings of `run` about those that are available and that the tracer saw.
    fn untraceable_problems(
        &self,
        steps: &[(StepKey, StepRecord)],
        available: &Availability,
        run: &Run,
        skips: &mut Skips,
    ) -> Vec<String> {
        let task = self.task();
        let mut problems = Vec::new();
        for call in UNTRACEABLE_SYSTEM_CALLS {
            let has = available.get(call.name).copied().unwrap_or(true);
            problems.extend(untraceable_problems(steps, &task, call.name, has, skips));
            let seen = incomplete_record(steps, &task, &Made::Call(call.name).mode())
                .is_some_and(tracer_saw);
            if has && seen {
                let command = format!("incomplete-{}`", call.name);
                let named = format!("`{}`", call.name);
                problems.extend(warning_problems(run, &[
                    command.as_str(),
                    "was traced incompletely",
                    named.as_str(),
                ]));
            }
        }
        problems
    }

    /// Fail with every problem, the runs of `kizz` and the output of every step.
    fn assert_no_problems(
        &self,
        problems: &[String],
    ) -> TestResult {
        let mut report = String::new();
        for (_, record) in step_records(self.cache.path())? {
            writeln!(
                report,
                "{}: {:?}\n{}",
                record.spec.argv.join(" "),
                record.not_cached,
                output_of(&record)
            )?;
        }
        for run in &self.runs {
            writeln!(report, "{run}")?;
        }
        assert!(
            problems.is_empty(),
            "{} problems:\n{}\n\n{report}",
            problems.len(),
            problems.join("\n")
        );
        Ok(())
    }
}

/// Build `coverage-incomplete` in a copy of the fixture, after `coverage-build` and
/// a reset. The list of `architecture.md` is [`UNTRACEABLE_SYSTEM_CALLS`]. The step
/// of each call is not cached, with an incomplete reason that names the call, and
/// the build warns about it; where `open_tree_attr` or `fchroot` is unavailable, as
/// the test's own check and the program's report find, the step may be cached, and
/// a notice names the call. The step of each unknown number is not cached, with
/// the reason that names the number, its record lists only that number, and the
/// build warns about it, names the option, and repeats the warning in its summary.
/// A call or number that the tracer never saw fails the test, unless
/// [`SKIP_VARIABLE`] is `1`, when a notice names it instead.
#[test]
fn each_call_that_marks_a_run_and_each_unknown_number_leaves_its_result_uncached() -> TestResult {
    let test = "each_call_that_marks_a_run_and_each_unknown_number_leaves_its_result_uncached";
    let available = own_untraceable_availability()?;
    write_untraceable_notice(test, &available)?;
    let mut skips = Skips::new(skip_mode());
    let spec =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../architecture.md"))?;
    let listed = spec_calls(&spec, UNTRACEABLE_HEADING)?;
    let names: Vec<&str> = UNTRACEABLE_SYSTEM_CALLS
        .iter()
        .map(|call| call.name)
        .collect();
    let built = IncompleteBuild::new(&[])?;
    let mut problems = built.problems.clone();
    if listed != names {
        problems.push(format!(
            "architecture.md lists {listed:?}, but the tracer marks the run for {names:?}"
        ));
    }
    let steps = step_records(built.cache.path())?;
    let expected_steps = UNTRACEABLE_SYSTEM_CALLS
        .len()
        .saturating_add(UNKNOWN_NUMBERS.len());
    if steps.len() != expected_steps {
        problems.push(format!(
            "the cache holds {} step records after building coverage-incomplete, not \
             {expected_steps}",
            steps.len()
        ));
    }
    let run = built.runs.last().ok_or("no build")?;
    let task = built.task();
    problems.extend(built.untraceable_problems(&steps, &available, run, &mut skips));
    for number in UNKNOWN_NUMBERS {
        let seen =
            incomplete_record(&steps, &task, &Made::Number(number).mode()).is_some_and(tracer_saw);
        problems.extend(unknown_problems(&steps, &task, number, false, &mut skips));
        if !seen {
            continue;
        }
        let made = format!(
            "made system call {number}, which the tracer does not know, so its result was not \
             cached and it runs again on the next build"
        );
        problems.extend(warning_problems(run, &[
            format!("unknown-{number}`").as_str(),
            made.as_str(),
            "`kizz build --cache-unknown-syscalls`",
        ]));
    }
    skips.write_notice(test)?;
    built.assert_no_problems(&problems)
}

/// Build `coverage-incomplete` with `--cache-unknown-syscalls` in a copy of the
/// fixture, after `coverage-build` and a reset: the steps of the unknown numbers are
/// cached and list their numbers, the build warns that the option cached them and
/// repeats the warning in its summary, and the steps of the calls that mark a run
/// are still not cached. A following build without the option finds the steps of
/// the numbers cached and warns about neither, and `kizz why` lists their numbers.
/// A call or number that the tracer never saw fails the test, unless
/// [`SKIP_VARIABLE`] is `1`, when a notice names it instead.
#[test]
fn with_the_option_the_steps_of_unknown_numbers_are_cached() -> TestResult {
    let test = "with_the_option_the_steps_of_unknown_numbers_are_cached";
    let available = own_untraceable_availability()?;
    write_untraceable_notice(test, &available)?;
    let mut skips = Skips::new(skip_mode());
    let mut built = IncompleteBuild::new(&["--cache-unknown-syscalls"])?;
    let mut problems = Vec::new();
    let steps = step_records(built.cache.path())?;
    let task = built.task();
    let run = built.runs.last().ok_or("no build")?;
    problems.extend(built.untraceable_problems(&steps, &available, run, &mut skips));
    let mut unseen = BTreeSet::new();
    for number in UNKNOWN_NUMBERS {
        let seen =
            incomplete_record(&steps, &task, &Made::Number(number).mode()).is_some_and(tracer_saw);
        problems.extend(unknown_problems(&steps, &task, number, true, &mut skips));
        if !seen {
            unseen.insert(number);
            continue;
        }
        let made = format!(
            "made system call {number}, which the tracer does not know; its result was cached \
             because `kizz build --cache-unknown-syscalls` was given"
        );
        let command = format!("unknown-{number}`");
        problems.extend(warning_problems(run, &[command.as_str(), made.as_str()]));
    }

    let again = built.run(&["build", "coverage-incomplete"])?;
    if again.stderr.contains("which the tracer does not know") {
        problems.push("a build that found the unknown numbers cached warned about them".to_owned());
    }
    let later = step_records(built.cache.path())?;
    for number in UNKNOWN_NUMBERS {
        let mode = Made::Number(number).mode();
        let ran_at = |records: &[(StepKey, StepRecord)]| {
            incomplete_record(records, &task, &mode).map(|record| record.ran_at_nanos)
        };
        if ran_at(&later).is_none() || ran_at(&later) != ran_at(&steps) {
            problems.push(format!(
                "the step of system call {number} ran again in a build without the option"
            ));
        }
        if !unseen.contains(&number) {
            problems.extend(unknown_problems(&later, &task, number, true, &mut skips));
        }
    }

    let why = built.run(&["why"])?;
    for number in UNKNOWN_NUMBERS {
        if unseen.contains(&number) {
            continue;
        }
        let line = format!("system calls that the tracer does not know: {number}");
        if !why.stdout.lines().any(|shown| shown.trim() == line) {
            problems.push(format!("kizz why does not show `{line}`"));
        }
    }
    problems.extend(built.problems.iter().cloned());
    skips.write_notice(test)?;
    built.assert_no_problems(&problems)
}

/// The little-endian field of `size` bytes, at most eight, at `offset` in `bytes`.
fn field(
    bytes: &[u8],
    offset: usize,
    size: usize,
) -> TestResult<usize> {
    let end = offset.checked_add(size).ok_or("the field overflows")?;
    let slice = bytes.get(offset..end).ok_or("the file is too short")?;
    let mut word = [0_u8; 8];
    word.get_mut(..size)
        .ok_or("the field is too wide")?
        .copy_from_slice(slice);
    Ok(usize::try_from(u64::from_le_bytes(word))?)
}

/// The loader that the 64-bit ELF executable at `path` names in its `PT_INTERP`
/// program header, or `None` when it has none.
fn elf_loader(path: &Path) -> TestResult<Option<PathBuf>> {
    let bytes = fs::read(path)?;
    if !bytes.starts_with(b"\x7fELF") || bytes.get(4) != Some(&2) {
        return Err(format!("{} is not a 64-bit ELF file", path.display()).into());
    }
    let start = field(&bytes, 32, 8)?;
    let size = field(&bytes, 54, 2)?;
    let count = field(&bytes, 56, 2)?;
    for index in 0..count {
        let header = index
            .checked_mul(size)
            .and_then(|offset| offset.checked_add(start))
            .ok_or("the program headers overflow")?;
        if field(&bytes, header, 4)? != 3 {
            continue;
        }
        let offset = field(&bytes, header.checked_add(8).ok_or("overflow")?, 8)?;
        let length = field(&bytes, header.checked_add(32).ok_or("overflow")?, 8)?;
        let end = offset
            .checked_add(length)
            .ok_or("the loader path overflows")?;
        let name = bytes.get(offset..end).ok_or("the loader path is cut")?;
        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
        return Ok(Some(PathBuf::from(OsStr::from_bytes(name))));
    }
    Ok(None)
}

/// The problems of the loader of the program in `copy`, the task directory: `cc`
/// links dynamically by default, so the program must have a `PT_INTERP` header,
/// and `record` must hold the loader it names, outside the copy, with the exec
/// kind.
fn loader_problems(
    record: &StepRecord,
    copy: &Path,
) -> Vec<String> {
    let loader = match elf_loader(&copy.join("coverage")) {
        Ok(Some(loader)) => loader,
        Ok(None) => return vec!["the program has no PT_INTERP program header".to_owned()],
        Err(error) => return vec![format!("could not read the program's loader: {error}")],
    };
    let mut problems = Vec::new();
    if loader.starts_with(copy) {
        problems.push(format!("the loader {} is in the copy", loader.display()));
    }
    let executed = record.observations.iter().any(|observation| {
        observation.path == loader && observation.access.contains(AccessBits::EXEC)
    });
    if !executed {
        problems.push(format!(
            "the loader {} is not recorded with the exec kind",
            loader.display()
        ));
    }
    problems
}

/// The list of tracked system calls in `architecture.md` has the names of
/// [`TRACKED_SYSTEM_CALLS`], in order, and each one has expected entries for this
/// platform that a tracer missing it would record differently: one on which it
/// succeeded and, for a call that takes a path, one on which it failed with
/// `ENOENT`.
#[test]
fn every_tracked_system_call_has_entries_that_only_it_explains() -> TestResult {
    let spec =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../architecture.md"))?;
    let listed = spec_tracked_calls(&spec)?;
    let tracked = tracked_names();
    assert_eq!(listed, tracked);
    assert_eq!(tracked.len(), TRACKED_COUNT);
    let entries = fixture_entries()?;
    let here: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.platform.is_current())
        .collect();
    let problems = coverage_problems(&here, &tracked);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(())
}

/// The expectation file is consistent: each entry's kinds are the union of its
/// sources' kinds, each system call is tracked, each failed call adds only a probe,
/// each mark names only gated calls, each once, and every gated call among the
/// entry's sources, and no path is listed twice for a platform.
#[test]
fn the_expectation_file_is_consistent() -> TestResult {
    let entries = fixture_entries()?;
    let problems = consistency_problems(&entries, &tracked_names());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(())
}

/// The connection lines of the expectation file are consistent: no destination is
/// listed twice for a platform, each call that connects has a connection that it
/// makes through `syscall()` on Linux, and each destination of a Unix socket path
/// is under `out`, which the program creates.
#[test]
fn the_expected_connections_are_consistent() -> TestResult {
    let connections = fixture_connections()?;
    let problems = connection_problems(&connections);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    for connection in &connections {
        if let Some(path) = connection.destination.strip_prefix("unix:") {
            assert!(path.starts_with("out/"), "line {}: {path}", connection.line);
        }
    }
    assert!(!expected_connections_here(&connections).is_empty());
    Ok(())
}

/// The parser reads every field of a connection line, which the parser of entries
/// skips, and rejects a malformed one with its line.
#[test]
fn the_parser_reads_connections() {
    let text = "# A comment.\nin/a.txt read file+hash all libc:open:ok:read\nconnection all \
                127.0.0.1:80 libc:connect\n  connection linux unix:out/s sys:sendmmsg\nconnection \
                linux-x86_64 [::1]:443 sys:sendto\nconnection all abstract:kizz libc:sendmsg\n";
    let connections = parse_connections(text).unwrap();
    assert_eq!(connections, vec![
        ExpectedConnection {
            line: 3,
            platform: Platform::All,
            destination: "127.0.0.1:80".to_owned(),
            origin: Origin::Libc,
            call: "connect".to_owned(),
        },
        ExpectedConnection {
            line: 4,
            platform: Platform::Linux,
            destination: "unix:out/s".to_owned(),
            origin: Origin::Sys,
            call: "sendmmsg".to_owned(),
        },
        ExpectedConnection {
            line: 5,
            platform: Platform::LinuxX86,
            destination: "[::1]:443".to_owned(),
            origin: Origin::Sys,
            call: "sendto".to_owned(),
        },
        ExpectedConnection {
            line: 6,
            platform: Platform::All,
            destination: "abstract:kizz".to_owned(),
            origin: Origin::Libc,
            call: "sendmsg".to_owned(),
        },
    ]);
    assert_eq!(parse_expectations(text).unwrap().len(), 1);
    for bad in [
        "connection all 127.0.0.1:80",
        "connection all 127.0.0.1 libc:connect",
        "connection all 127.000.0.1:80 libc:connect",
        "connection all unix:/abs sys:connect",
        "connection all unix:./x sys:connect",
        "connection all abstract: sys:connect",
        "connection all 127.0.0.1:80 sys:bind",
        "connection all 127.0.0.1:80 kizz:connect",
        "connection all 127.0.0.1:80 connect",
        "connection linux/getxattrat 127.0.0.1:80 sys:connect",
        "connection nowhere 127.0.0.1:80 sys:connect",
    ] {
        let errors = parse_connections(bad).unwrap_err();
        assert_eq!(errors.len(), 1, "{bad}");
        assert!(errors[0].starts_with("line 1: "), "{bad}: {errors:?}");
    }
}

/// The problems of connection lines name a destination listed twice for a platform
/// and each call that connects without a connection through `syscall()` on Linux.
#[test]
fn connection_problems_name_repeats_and_missing_calls() {
    let text = "connection all 127.0.0.1:1 sys:connect\nconnection linux 127.0.0.1:1 \
                sys:sendto\nconnection macos 127.0.0.1:2 sys:sendmsg\nconnection linux \
                127.0.0.1:3 libc:sendmmsg\n";
    let connections = parse_connections(text).unwrap();
    assert_eq!(connection_problems(&connections), vec![
        "lines 1 and 2: 127.0.0.1:1 is listed twice for a platform".to_owned(),
        "sendmsg has no connection that it makes through syscall() on Linux".to_owned(),
        "sendmmsg has no connection that it makes through syscall() on Linux".to_owned(),
    ]);
}

/// A record's connections read as the expectation file writes them, a Unix socket
/// path relative to the task directory, or absolute outside it; the comparison
/// names each missing and each unexpected connection.
#[test]
fn recorded_connections_are_compared_with_the_expected_ones() {
    let dir = Path::new("/t");
    let shown: Vec<String> = [
        Connection::Ipv4 {
            address: [127, 0, 0, 1],
            port: 80,
        },
        Connection::Ipv6 {
            address: std::net::Ipv6Addr::LOCALHOST.octets(),
            port: 443,
            scope: 0,
        },
        Connection::UnixPath {
            path: PathBuf::from("/t/out/s"),
        },
        Connection::UnixPath {
            path: PathBuf::from("/run/s"),
        },
        Connection::UnixAbstract {
            name: b"k\0".to_vec(),
        },
    ]
    .iter()
    .map(|connection| connection_text(connection, dir))
    .collect();
    assert_eq!(shown, [
        "127.0.0.1:80",
        "[::1]:443",
        "unix:out/s",
        "unix:/run/s",
        "abstract:k\\x00"
    ]);
    let set = |items: &[&str]| -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    };
    assert_eq!(
        connection_differences(&set(&["a", "b"]), &set(&["b", "c"])),
        ["missing connection: a", "unexpected connection: c"]
    );
    assert_eq!(
        connection_differences(&set(&["a"]), &set(&["a"])),
        Vec::<String>::new()
    );
}

/// Every path of the expectation file is in the fixture's committed inputs or under
/// `out`, which the program creates, so the program never touches a path that the
/// fixture holds outside its inputs.
#[test]
fn expected_paths_are_inputs_outputs_or_the_program() -> TestResult {
    for entry in fixture_entries()? {
        let first = Path::new(&entry.path).components().next();
        let allowed = matches!(first, Some(Component::Normal(name))
            if name == "in" || name == "out" || name == "coverage");
        assert!(allowed, "line {}: {}", entry.line, entry.path);
    }
    Ok(())
}

/// The parser reads every field of an entry and skips comments and blank lines.
#[test]
fn the_parser_reads_entries() {
    let text = "# A comment.\n\nin/a.txt read+probe file+hash linux \
                sys:openat:ok:read sys:stat:ENOENT:probe\nout/l write \
                symlink:../t:dir all libc:symlink:ok:write\n";
    let entries = parse_expectations(text).unwrap();
    assert_eq!(entries.len(), 2);
    let first = &entries[0];
    assert_eq!(first.line, 3);
    assert_eq!(first.path, "in/a.txt");
    assert_eq!(first.kinds, "probe+read".parse().unwrap());
    assert_eq!(first.state, Observed::plain(State::File { hash: true }));
    assert_eq!(first.platform, Platform::Linux);
    assert_eq!(first.sources, vec![
        Source {
            origin: Origin::Sys,
            name: "openat".to_owned(),
            outcome: "ok".to_owned(),
            kinds: "read".parse().unwrap(),
        },
        Source {
            origin: Origin::Sys,
            name: "stat".to_owned(),
            outcome: "ENOENT".to_owned(),
            kinds: "probe".parse().unwrap(),
        },
    ]);
    assert_eq!(
        entries[1].state,
        Observed::plain(State::Symlink {
            target: "../t".to_owned(),
            pointee: Box::new(State::Dir { listing: false }),
        })
    );
    assert_eq!(entries[1].state.to_string(), "symlink:../t:dir");
    assert_eq!(entries[1].platform, Platform::All);
}

/// A state with recorded attributes names their `user.` names after `@`, `-` for
/// attributes that could not be read, and for a symbolic link those of what it
/// points to after `/`; it is written back as it was read, and names outside
/// `user.` are rejected.
#[test]
fn states_with_recorded_attributes_are_read_and_written() {
    let names = |list: &[&str]| Some(list.iter().map(|name| (*name).to_owned()).collect());
    let cases = [
        (
            "file@user.a,user.b",
            State::File { hash: false },
            AttributeNames {
                own: names(&["user.a", "user.b"]),
                pointee: None,
            },
        ),
        ("file+hash@", State::File { hash: true }, AttributeNames {
            own: names(&[]),
            pointee: None,
        }),
        ("absent@-", State::Absent, AttributeNames {
            own: None,
            pointee: None,
        }),
        (
            "symlink:t:file@/user.kizz",
            State::Symlink {
                target: "t".to_owned(),
                pointee: Box::new(State::File { hash: false }),
            },
            AttributeNames {
                own: names(&[]),
                pointee: Some(names(&["user.kizz"])),
            },
        ),
    ];
    for (text, state, attributes) in cases {
        let observed: Observed = text.parse().unwrap();
        assert_eq!(observed, Observed {
            state,
            attributes: Some(attributes),
        });
        assert_eq!(observed.to_string(), text);
    }
    assert!("file@security.selinux".parse::<Observed>().is_err());
    assert!("folder@user.a".parse::<Observed>().is_err());
}

/// Recorded attributes keep only their `user.` names, for the path and for what a
/// link points to, and attributes that could not be read stay apart from none.
#[test]
fn recorded_attributes_keep_their_user_names() {
    use kizz_store::ContentHash;

    let attribute = |name: &str| {
        Attribute {
            name: name.as_bytes().to_vec(),
            hash: ContentHash([0; 32]),
        }
    };
    let recorded = Attributes {
        own: Some(vec![attribute("security.selinux"), attribute("user.a")]),
        pointee: Some(vec![attribute("trusted.x")]),
    };
    assert_eq!(AttributeNames::from_recorded(&recorded), AttributeNames {
        own: Some(vec!["user.a".to_owned()]),
        pointee: Some(Some(Vec::new())),
    });
    let unreadable = Attributes {
        own: None,
        pointee: None,
    };
    assert_eq!(AttributeNames::from_recorded(&unreadable), AttributeNames {
        own: None,
        pointee: None,
    });
}

/// The parser reports every malformed line, with its number.
#[test]
fn the_parser_reports_every_error() {
    let text = "a read file all\n\
                /abs read file all sys:open:ok:read\n\
                a/./b read file all sys:open:ok:read\n\
                a reed file all sys:open:ok:read\n\
                a read folder all sys:open:ok:read\n\
                a read file mars sys:open:ok:read\n\
                a read file all sys:open:read\n\
                a read file all os:open:ok:read\n\
                a read file all sys:open:fine:read\n\
                a read+read file all sys:open:ok:read\n\
                a read symlink:t all sys:open:ok:read\n";
    let errors = parse_expectations(text).unwrap_err();
    let lines: Vec<&str> = errors
        .iter()
        .map(|error| error.split(':').next().unwrap())
        .collect();
    assert_eq!(lines, vec![
        "line 1", "line 2", "line 3", "line 4", "line 5", "line 6", "line 7", "line 8", "line 9",
        "line 10", "line 11",
    ]);
}

/// The consistency check finds kinds that are not the union of the sources' kinds,
/// untracked system calls, unknown implicit loads, failed calls and loads that add
/// more than a probe, except a failed read of attributes that adds only the xattr
/// kind with an error that answers from the attributes, and a path listed twice for
/// overlapping platforms, but not for disjoint ones.
#[test]
fn the_consistency_check_finds_each_problem() {
    let text = "a read+write file all sys:openat:ok:read\n\
                b read file linux sys:read:ok:read\n\
                c probe absent linux sys:openat:ENOENT:read\n\
                d probe file linux sys:stat:ok:probe\n\
                d probe file linux-x86_64 sys:stat:ok:probe\n\
                e probe file linux-x86_64 sys:stat:ok:probe\n\
                e probe file linux-aarch64 sys:statx:ok:probe\n\
                f exec file+hash all kizz:execveat:ok:exec\n\
                g exec file+hash linux kernel:loader:ok:exec\n\
                h exec absent linux kernel:interpreter:ENOENT:exec\n\
                i exec file+hash linux kernel:interpreter_of_interpreter:ok:exec\n\
                j xattr file@ linux sys:getxattr:ENODATA:xattr\n\
                k xattr file@user.a linux sys:getxattr:ERANGE:xattr\n\
                l xattr file@ linux sys:getxattr:EACCES:xattr\n";
    let entries = parse_expectations(text).unwrap();
    let problems = consistency_problems(&entries, &["openat", "stat", "statx", "getxattr"]);
    assert_eq!(problems, vec![
        "line 1: a has the kinds read+write, but its sources add read",
        "line 2: read is not a tracked system call",
        "line 3: the failed call openat adds read, not a probe",
        "line 3: c has the kinds probe, but its sources add read",
        "line 8: kizz starts a program only with a successful execve",
        "line 9: loader is not a known implicit load",
        "line 10: the failed call interpreter adds exec, not a probe",
        "line 14: the failed call getxattr adds xattr, not a probe",
        "lines 4 and 5: d is listed twice for a platform",
    ]);
}

/// The availability of [`GATED_CALLS`] with only the calls of `unavailable`
/// missing.
fn availability_without(unavailable: &[&str]) -> Availability {
    GATED_CALLS
        .iter()
        .map(|name| ((*name).to_owned(), !unavailable.contains(name)))
        .collect()
}

/// The parser reads the mark after a platform, the gated calls joined with `+`, and
/// reports an empty, malformed or nested mark, and a mark after an unknown platform.
#[test]
fn the_parser_reads_marks_and_reports_malformed_ones() {
    let text = "a probe file linux/file_getattr sys:file_getattr:ok:probe\n\
                b write file linux/file_getattr+file_setattr sys:file_setattr:ok:write\n\
                c probe file linux sys:stat:ok:probe\n";
    let entries = parse_expectations(text).unwrap();
    let marks: Vec<(Platform, Vec<String>)> = entries
        .iter()
        .map(|entry| (entry.platform, entry.needs.clone()))
        .collect();
    assert_eq!(marks, vec![
        (Platform::Linux, vec!["file_getattr".to_owned()]),
        (Platform::Linux, vec![
            "file_getattr".to_owned(),
            "file_setattr".to_owned()
        ]),
        (Platform::Linux, Vec::new()),
    ]);
    let text = "a probe file linux/ sys:stat:ok:probe\n\
                a probe file linux/a++b sys:stat:ok:probe\n\
                a probe file linux/a/b sys:stat:ok:probe\n\
                a probe file mars/a sys:stat:ok:probe\n";
    let errors = parse_expectations(text).unwrap_err();
    let lines: Vec<&str> = errors
        .iter()
        .map(|error| error.split(':').next().unwrap())
        .collect();
    assert_eq!(lines, vec!["line 1", "line 2", "line 3", "line 4"]);
}

/// The consistency check finds a mark that names a call that is not gated, a mark
/// that names a call twice, and a source of a gated call that the mark does not
/// name, but accepts a marked entry whose sources are other calls.
#[test]
fn the_consistency_check_finds_each_problem_of_marks() {
    let text = "a xattr file linux/getxattrat sys:getxattrat:ok:xattr\n\
                b probe file linux/stat sys:stat:ok:probe\n\
                c probe absent linux/getxattrat+getxattrat sys:getxattrat:ENOENT:probe\n\
                d probe file linux sys:file_getattr:ok:probe\n\
                e probe+write file linux/file_setattr sys:file_getattr:ok:probe \
                sys:file_setattr:ok:write\n\
                f write file linux/file_getattr+file_setattr sys:openat:ok:write\n";
    let entries = parse_expectations(text).unwrap();
    let tracked = [
        "getxattrat",
        "stat",
        "file_getattr",
        "file_setattr",
        "openat",
    ];
    assert_eq!(consistency_problems(&entries, &tracked), vec![
        "line 2: the mark names stat, which is not a gated call",
        "line 3: the mark names getxattrat twice",
        "line 4: the gated call file_getattr is a source, but the mark does not name it",
        "line 5: the gated call file_getattr is a source, but the mark does not name it",
    ]);
}

/// The program's availability lines are read in any place of its output; a gated
/// call without a line, one with two, and a name that is not gated are problems,
/// and so is a report that disagrees with the test's own check.
#[test]
fn availability_lines_are_read_and_their_problems_reported() {
    let output = "coverage: available: file_getattr\n\
                  coverage: unavailable: getxattrat\n\
                  mismatch: openat x: expected success, got ENOENT\n\
                  coverage: available: listxattrat\n\
                  coverage: available: setxattrat\n\
                  coverage: unavailable: removexattrat\n\
                  coverage: available: file_setattr\n\
                  coverage: every call returned what it expected, 1 checks\n";
    let (reported, problems) = reported_availability(output);
    assert_eq!(
        reported,
        availability_without(&["getxattrat", "removexattrat"])
    );
    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(
        disagreements(&reported, &availability_without(&["getxattrat"])),
        vec![
            "the program reported removexattrat unavailable, but the test's own check found \
             it available"
        ]
    );
    let output = "coverage: available: file_getattr\n\
                  coverage: unavailable: file_getattr\n\
                  coverage: available: stat\n\
                  coverage: available: getxattrat\n\
                  coverage: available: listxattrat\n\
                  coverage: available: setxattrat\n\
                  coverage: available: removexattrat\n";
    let (_, problems) = reported_availability(output);
    assert_eq!(problems, vec![
        "the program reported file_getattr more than once",
        "the program reported stat, which is not a gated call",
        "the program did not report whether file_setattr is available",
    ]);
}

/// The expected paths leave out exactly the entries whose mark names an
/// unavailable call, and keep every entry where every gated call is available.
#[test]
fn expected_paths_leave_out_the_entries_of_unavailable_calls() {
    let text = "a read file+hash linux sys:openat:ok:read\n\
                b probe file linux/file_getattr sys:file_getattr:ok:probe\n\
                c write file linux/file_getattr+file_setattr sys:file_setattr:ok:write\n\
                d xattr file@user.kizz linux/getxattrat sys:getxattrat:ok:xattr\n\
                e write file linux/file_getattr+file_setattr sys:openat:ok:write\n";
    let entries = parse_expectations(text).unwrap();
    let paths = |available: &Availability| -> Vec<String> {
        expected_here(&entries, available).into_keys().collect()
    };
    assert_eq!(paths(&availability_without(&[])), vec![
        "a", "b", "c", "d", "e"
    ]);
    assert_eq!(paths(&availability_without(&["file_setattr"])), vec![
        "a", "b", "d"
    ]);
    assert_eq!(
        paths(&availability_without(&["file_getattr", "getxattrat"])),
        vec!["a"]
    );
}

/// The notice names the test and each unavailable gated call, in order, and there
/// is none when every gated call is available.
#[test]
fn the_notice_names_the_test_and_each_unavailable_call() {
    assert_eq!(unavailable_notice("t", &availability_without(&[])), None);
    assert_eq!(
        unavailable_notice("t", &availability_without(&["file_setattr", "getxattrat"])),
        Some(
            "NOTICE: t: getxattrat, file_setattr returned ENOSYS (the kernel lacks the call, or \
             a seccomp filter installed before kizz's own refuses it), so the test required \
             that nothing was recorded at their paths instead of checking their accesses"
                .to_owned()
        )
    );
}

/// A step record of `coverage-incomplete` with `not_cached`, the unknown system
/// calls `unknown`, the exit code `code` and the captured output `output`.
fn incomplete_step(
    not_cached: Option<NotCachedReason>,
    unknown: &[u32],
    code: i32,
    output: &str,
) -> StepRecord {
    use kizz_store::{Capture, ContentHash, RunReason, StepSpecRecord};

    StepRecord {
        spec: StepSpecRecord {
            program_hash: ContentHash([0; 32]),
            cwd: PathBuf::from("/t"),
            argv: vec!["./coverage".to_owned()],
            env: BTreeMap::new(),
            stdin: None,
            capture: Capture::Merged,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            expect: None,
            occurrence: 0,
        },
        program: None,
        observations: Vec::new(),
        ignored: 0,
        exit: StepExit::Exited { code },
        output: RecordedOutput::Merged {
            output: output.as_bytes().to_vec(),
        },
        duration_nanos: 0,
        ran_at_nanos: 0,
        reason: RunReason::NeverRun,
        recorded_at_nanos: 0,
        failure: None,
        not_cached,
        owners: BTreeSet::new(),
        unknown_system_calls: unknown.to_vec(),
        connections: Vec::new(),
    }
}

/// The incomplete reason of a run in which a process made `name`.
fn marked(name: &str) -> NotCachedReason {
    NotCachedReason::Incomplete {
        detail: format!("a system call that kizz cannot trace: process 7 made `{name}`"),
    }
}

/// Only the value `1` of the variable turns skipping on: unset, empty, `0` and
/// `true` leave the tests strict.
#[test]
fn only_the_value_1_turns_skipping_on() {
    assert!(skips_blocked(Some(OsStr::new("1"))));
    for value in [None, Some(""), Some("0"), Some("true")] {
        assert!(!skips_blocked(value.map(OsStr::new)), "{value:?}");
    }
}

/// A step that is not cached, whatever its reason, or that lists an unknown system
/// call, cached or not, made a call that the tracer saw; only a cached step that
/// lists none made one that it never saw.
#[test]
fn step_records_are_judged_as_seen_or_never_seen() {
    let seen = [
        incomplete_step(Some(marked("acct")), &[], 0, ""),
        incomplete_step(
            Some(NotCachedReason::UnknownSystemCalls { numbers: vec![400] }),
            &[400],
            0,
            "",
        ),
        incomplete_step(None, &[400], 0, ""),
        incomplete_step(
            Some(NotCachedReason::ChangedAfterRead { files: Vec::new() }),
            &[],
            0,
            "",
        ),
    ];
    for record in &seen {
        assert!(tracer_saw(record), "{:?}", record.not_cached);
    }
    let unseen = incomplete_step(None, &[], 0, "");
    assert!(!tracer_saw(&unseen), "{:?}", unseen.not_cached);
}

/// In both modes, a step that exited with 0 and reported its call or number with an
/// error, but is not cached for a reason other than the call's, was seen: it gets
/// the full check, which fails on its reason and numbers, and it is never skipped.
#[test]
fn a_step_not_cached_for_another_reason_is_checked_in_full_in_both_modes() {
    let changed = || Some(NotCachedReason::ChangedAfterRead { files: Vec::new() });
    let call = incomplete_step(changed(), &[], 0, "coverage: made acct: EPERM\n");
    let number = incomplete_step(
        changed(),
        &[],
        0,
        "coverage: made system call 400: ENOSYS\n",
    );
    for on in [false, true] {
        let mut skips = Skips::new(on);
        assert_eq!(call_problems(&call, "acct", true, &mut skips), vec![
            "the acct step is marked Some(ChangedAfterRead { files: [] }), not incompletely \
             traced for `acct`"
        ]);
        assert_eq!(number_problems(&number, 400, false, &mut skips), vec![
            "the step of system call 400 lists []".to_owned(),
            "the step of system call 400 is marked Some(ChangedAfterRead { files: [] }), not \
             Some(UnknownSystemCalls { numbers: [400] })"
                .to_owned(),
        ]);
        assert_eq!(number_problems(&number, 400, true, &mut skips), vec![
            "the step of system call 400 lists []".to_owned(),
            "the step of system call 400 is marked Some(ChangedAfterRead { files: [] }), not None"
                .to_owned(),
        ]);
        assert_eq!(skips.skipped, Vec::new(), "skipping {on}");
        assert_eq!(skips.notice("t"), None, "skipping {on}");
    }
}

/// A call or number that the tracer never saw, whose step exited with 0 and
/// reported it with an error, fails with a message that names it and the variable
/// when skipping is off, and is skipped, with no problem, when it is on.
#[test]
fn a_call_that_the_tracer_never_saw_fails_unless_skipping_is_on() {
    let call = incomplete_step(None, &[], 0, "coverage: made acct: EPERM\n");
    let number = incomplete_step(None, &[], 0, "coverage: made system call 400: ENOSYS\n");

    let mut strict = Skips::new(false);
    assert_eq!(call_problems(&call, "acct", true, &mut strict), vec![
        "the tracer never saw `acct`; if a seccomp filter installed before kizz refuses it, set \
         KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1"
    ]);
    assert_eq!(number_problems(&number, 400, false, &mut strict), vec![
        "the tracer never saw system call 400; if a seccomp filter installed before kizz refuses \
         it, set KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1"
    ]);
    assert_eq!(strict.skipped, Vec::new());

    let mut skipping = Skips::new(true);
    assert_eq!(
        call_problems(&call, "acct", true, &mut skipping),
        Vec::<String>::new()
    );
    assert_eq!(
        number_problems(&number, 400, true, &mut skipping),
        Vec::<String>::new()
    );
    assert_eq!(skipping.skipped, vec![
        Made::Call("acct"),
        Made::Number(400)
    ]);
}

/// In both modes, a call or number that the tracer saw and mishandled fails, and so
/// does one that it never saw whose step did not exit with 0 or did not report the
/// call with an error; none of them is skipped. A gated call that is unavailable
/// follows decision 8, not the variable.
#[test]
fn mishandled_or_failed_calls_fail_in_both_modes() {
    let ok_call = "coverage: made acct: EPERM\n";
    let ok_number = "coverage: made system call 400: ENOSYS\n";
    for on in [false, true] {
        let mut skips = Skips::new(on);
        // Seen, with a reason that names another call.
        let wrong = incomplete_step(Some(marked("chroot")), &[], 0, ok_call);
        assert_eq!(call_problems(&wrong, "acct", true, &mut skips).len(), 1);
        // Seen, with an extra number.
        let extra = incomplete_step(
            Some(NotCachedReason::UnknownSystemCalls {
                numbers: vec![400, 401],
            }),
            &[400, 401],
            0,
            ok_number,
        );
        assert_eq!(number_problems(&extra, 400, false, &mut skips).len(), 2);
        // Seen, and cached without the option.
        let cached = incomplete_step(None, &[400], 0, ok_number);
        assert_eq!(number_problems(&cached, 400, false, &mut skips).len(), 1);
        // Never seen, but the step failed.
        let failed = incomplete_step(None, &[], 1, ok_call);
        assert!(
            call_problems(&failed, "acct", true, &mut skips)
                .contains(&"the step of acct ended with Exited { code: 1 }".to_owned())
        );
        // Never seen, but the call succeeded or was not reported.
        for output in ["coverage: made acct: success\n", ""] {
            let unreported = incomplete_step(None, &[], 0, output);
            let problems = call_problems(&unreported, "acct", true, &mut skips);
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.contains("did not report it once with an error")),
                "{problems:?}"
            );
        }
        let unreported = incomplete_step(None, &[], 0, "coverage: made system call 401: ENOSYS\n");
        let problems = number_problems(&unreported, 400, false, &mut skips);
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("did not report it once with an error")),
            "{problems:?}"
        );
        // Unavailable under decision 8: cached and reported as unavailable.
        let unavailable = incomplete_step(
            None,
            &[],
            0,
            "coverage: unavailable: fchroot\ncoverage: made fchroot: ENOSYS\n",
        );
        assert_eq!(
            call_problems(&unavailable, "fchroot", false, &mut skips),
            Vec::<String>::new()
        );
        assert_eq!(skips.skipped, Vec::new(), "skipping {on}");
    }
}

/// The notice of the skipped calls and numbers names the test, the variable and
/// each of them, in order, and there is none when nothing was skipped.
#[test]
fn the_skip_notice_names_the_test_the_variable_and_each_skipped_call() {
    let mut skips = Skips::new(true);
    assert_eq!(skips.notice("t"), None);
    assert_eq!(
        skips.never_saw(Made::Call("acct"), Vec::new()),
        Vec::<String>::new()
    );
    assert_eq!(
        skips.never_saw(Made::Number(400), Vec::new()),
        Vec::<String>::new()
    );
    assert_eq!(
        skips.notice("t"),
        Some(
            "NOTICE: t: the tracer never saw acct, system call 400, which a seccomp filter \
             installed before kizz's own may have refused, so KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1 \
             skipped their checks"
                .to_owned()
        )
    );
    let mut strict = Skips::new(false);
    assert_eq!(strict.never_saw(Made::Call("acct"), Vec::new()).len(), 1);
    assert_eq!(strict.notice("t"), None);
}

/// The coverage check requires, for each call, an entry on which it succeeded and,
/// for a call with a path, one on which it failed with `ENOENT`, each of which it
/// alone explains: as the only source, or with a kind that no other source adds.
/// It requires the same of each implicit load of the kernel, with its outcome.
#[test]
fn the_coverage_check_requires_entries_that_only_the_call_explains() {
    let text = "a probe+write dir linux sys:mkdirat:ok:write sys:mkdirat:EEXIST:probe \
                sys:openat:ok:probe\n\
                b probe absent linux sys:mkdirat:ENOENT:probe\n\
                c probe file linux sys:openat:ok:probe sys:stat:ok:probe\n\
                d probe absent linux sys:openat:ENOENT:probe sys:stat:ENOENT:probe\n\
                e list dir linux sys:getdents64:ok:list\n\
                f exec file+hash linux kernel:interpreter:ok:exec\n\
                g exec file+hash linux kizz:execve:ok:exec \
                kernel:interpreter_of_interpreter:ok:exec\n\
                h probe absent linux kernel:interpreter:ENOENT:probe\n";
    let entries = parse_expectations(text).unwrap();
    let all: Vec<&Entry> = entries.iter().collect();
    let problems = coverage_problems(&all, &["mkdirat", "openat", "getdents64", "fchmod"]);
    assert_eq!(problems, vec![
        "openat has no entry with outcome ok that only it explains",
        "openat has no entry with outcome ENOENT that only it explains",
        "fchmod has no entry with outcome ok that only it explains",
        "the implicit load interpreter_of_interpreter has no entry with outcome ok that only it \
         explains",
    ]);
    // An interpreter of an interpreter that only that load explains.
    let text = format!("{text}i exec file+hash linux kernel:interpreter_of_interpreter:ok:exec\n");
    let entries = parse_expectations(&text).unwrap();
    let all: Vec<&Entry> = entries.iter().collect();
    let problems = coverage_problems(&all, &["mkdirat", "getdents64"]);
    assert_eq!(problems, Vec::<String>::new());
}

/// The list of tracked calls is read from the bullets under its heading, with the
/// continuation lines of a bullet, and nothing after them.
#[test]
fn the_spec_list_is_read_from_its_bullets() {
    let text = "intro `x`\n\nTracked system calls, by x86_64 name:\n\n* a: `open` and `creat`\n\
                * b: `stat`,\n  `lstat` and `access`\n\n* later: `nope`\n";
    assert_eq!(spec_tracked_calls(text).unwrap(), vec![
        "open", "creat", "stat", "lstat", "access"
    ]);
    assert!(spec_tracked_calls("nothing here").is_err());
    let both = format!("{text}\n{UNTRACEABLE_HEADING}\n\n* c: `mount`,\n  `chroot`\n\nafter `x`\n");
    assert_eq!(spec_calls(&both, UNTRACEABLE_HEADING).unwrap(), vec![
        "mount", "chroot"
    ]);
    assert_eq!(spec_tracked_calls(&both).unwrap().len(), 5);
}

/// The comparison reports a missing, an unexpected, and a mismatched entry
/// together, in path order, and nothing for equal entries.
#[test]
fn the_comparison_reports_every_difference_together() {
    let entry = |kinds: &str, state: &str| (kinds.parse().unwrap(), state.parse().unwrap());
    let expected = Recorded::from([
        ("a".to_owned(), entry("read", "file+hash")),
        ("b".to_owned(), entry("probe", "absent")),
        ("c".to_owned(), entry("write", "file")),
    ]);
    let recorded = Recorded::from([
        ("a".to_owned(), entry("read", "file+hash")),
        ("c".to_owned(), entry("write+probe", "file")),
        ("d".to_owned(), entry("list", "dir+listing")),
    ]);
    let differences: Vec<String> = compare(&expected, &recorded)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(differences, vec![
        "missing: b, expected probe absent",
        "mismatched: c, expected write file, recorded probe+write file",
        "unexpected: d, recorded list dir+listing",
    ]);
}

/// Recorded observations are taken relative to the task directory, only under it,
/// with their kinds and the types of their states.
#[test]
fn recorded_observations_are_taken_relative_to_the_task_directory() {
    use kizz_store::{
        Capture, ContentHash, FileMeta, Observation, Owner, RunReason, StepSpecRecord,
    };

    let owner = Owner { uid: 1, gid: 1 };
    let meta = FileMeta {
        size: 1,
        mtime_nanos: 0,
        ctime_nanos: 0,
        inode: 1,
        device: 1,
        mode: 0o100_644,
        owner,
    };
    let observation = |path: &str, access: u8, state: PathState| {
        Observation {
            path: PathBuf::from(path),
            access: AccessBits(access),
            tracked: false,
            state,
            attributes: None,
        }
    };
    let read_attributes = Observation {
        path: PathBuf::from("/t/x"),
        access: AccessBits(AccessBits::XATTR),
        tracked: false,
        state: PathState::File { meta, hash: None },
        attributes: Some(Attributes {
            own: Some(vec![Attribute {
                name: b"user.a".to_vec(),
                hash: ContentHash([2; 32]),
            }]),
            pointee: None,
        }),
    };
    let record = StepRecord {
        spec: StepSpecRecord {
            program_hash: ContentHash([0; 32]),
            cwd: PathBuf::from("/t"),
            argv: Vec::new(),
            env: BTreeMap::new(),
            stdin: None,
            capture: Capture::Merged,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            expect: None,
            occurrence: 0,
        },
        program: None,
        observations: vec![
            observation("/lib/libc.so", AccessBits::READ, PathState::File {
                meta,
                hash: Some(ContentHash([1; 32])),
            }),
            observation("/t", AccessBits::PROBE, PathState::Directory {
                mode: 0o040_755,
                owner,
                listing: None,
                times: meta.times(),
            }),
            observation(
                "/t/a/../b",
                AccessBits::READ | AccessBits::PROBE,
                PathState::File { meta, hash: None },
            ),
            observation("/t/l", AccessBits::WRITE, PathState::Symlink {
                target: PathBuf::from("x"),
                pointee: Box::new(PathState::Absent),
            }),
            observation("/tt/c", AccessBits::LIST, PathState::Absent),
            read_attributes,
        ],
        ignored: 0,
        exit: StepExit::Exited { code: 0 },
        output: RecordedOutput::Merged { output: Vec::new() },
        duration_nanos: 0,
        ran_at_nanos: 0,
        reason: RunReason::NeverRun,
        recorded_at_nanos: 0,
        failure: None,
        not_cached: None,
        owners: BTreeSet::new(),
        unknown_system_calls: Vec::new(),
        connections: Vec::new(),
    };
    let recorded: Vec<String> = recorded_under(&record, Path::new("/t"))
        .into_iter()
        .map(|(path, (kinds, state))| format!("{path} {kinds} {state}"))
        .collect();
    assert_eq!(recorded, vec![
        ". probe dir",
        "a/../b read+probe file",
        "l write symlink:x:absent",
        "x xattr file@user.a",
    ]);
}
