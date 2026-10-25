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
//! the one step record with the entries of `expected.txt` for the platform it runs on.
//! Every run of `kizz` gets a temporary cache and an explicit environment of only
//! `KIZZ_CACHE_DIR` and `PATH`. Paths outside the copy, such as those of the dynamic
//! loader and the C library, are not compared.
//!
//! The other tests check `expected.txt` itself: that it is consistent, and that each
//! tracked system call that `architecture.md` lists, the same list as
//! [`TRACKED_SYSTEM_CALLS`], has an entry for its success and, for a call that takes a
//! path, one for its failure with `ENOENT`, on which a tracer that missed the call
//! would record something else. Unit tests cover the parser and the comparison.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Write as _},
    fs,
    os::unix::fs::symlink,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};

use kizz_store::{
    AccessBits, PathState, RecordedOutput, SpecialKind, StepExit, StepKey, StepRecord, Store,
    TaskId,
};
use kizz_trace_linux::TRACKED_SYSTEM_CALLS;

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

/// The tracked system calls that take no path, only a descriptor, so they have no
/// `ENOENT` entry.
const DESCRIPTOR_CALLS: [&str; 6] = [
    "getdents",
    "getdents64",
    "fchdir",
    "ftruncate",
    "fchmod",
    "fchown",
];

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
}

/// Every kind with its name and its bit in the store, in order.
const KINDS: [(Kind, &str, u8); 5] = [
    (Kind::Read, "read", AccessBits::READ),
    (Kind::Probe, "probe", AccessBits::PROBE),
    (Kind::List, "list", AccessBits::LIST),
    (Kind::Write, "write", AccessBits::WRITE),
    (Kind::Exec, "exec", AccessBits::EXEC),
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
    /// The type of the state that must be recorded.
    state: State,
    /// The platforms that expect the entry.
    platform: Platform,
    /// Every call that touches the path in the run.
    sources: Vec<Source>,
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
    let platform = PLATFORMS
        .iter()
        .find(|(_, name)| name == platform)
        .map(|(known, _)| *known)
        .ok_or_else(|| format!("line {line}: unknown platform `{platform}`"))?;
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
        sources,
    })
}

/// Parse an expectation file: one entry per line, with blank lines and lines that
/// start with `#` ignored. Returns every entry, or every error.
fn parse_expectations(text: &str) -> Result<Vec<Entry>, Vec<String>> {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for (line, content) in (1_usize..).zip(text.lines()) {
        let content = content.trim();
        if content.is_empty() || content.starts_with('#') {
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

/// The problems of `entries` as a whole, given the names of the tracked system
/// calls: an entry's kinds that are not the union of its sources' kinds, a system
/// call that is not tracked, a failed call that adds more than a probe, a start by
/// kizz that is not a successful `execve`, and a path listed twice for a platform.
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
            if !source.succeeded() && source.kinds != Kinds(BTreeSet::from([Kind::Probe])) {
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

/// Whether a tracer that missed the system call `name` would record `entry`
/// differently: the call is its only source, or adds a kind that no other source
/// adds.
fn detects(
    entry: &Entry,
    name: &str,
) -> bool {
    let mut own = Kinds::default();
    let mut others = Kinds::default();
    let mut has_others = false;
    for source in &entry.sources {
        if source.is_system_call(name) {
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
/// without it.
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
                    && detects(entry, name)
            });
            if !detected {
                problems.push(format!(
                    "{name} has no entry with outcome {outcome} that only it explains"
                ));
            }
        }
    }
    problems
}

/// The tracked system calls that `architecture.md`'s text lists: every name in
/// backticks in the bullets under [`TRACKED_HEADING`], in order.
fn spec_tracked_calls(text: &str) -> Result<Vec<String>, String> {
    let mut lines = text
        .lines()
        .skip_while(|line| *line != TRACKED_HEADING)
        .skip(1);
    if text.lines().all(|line| line != TRACKED_HEADING) {
        return Err(format!("architecture.md has no line `{TRACKED_HEADING}`"));
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
        return Err("architecture.md lists no tracked system call".to_owned());
    }
    Ok(names)
}

/// What a path's record holds: its kinds and its state.
type Recorded = BTreeMap<String, (Kinds, State)>;

/// One difference between the expected and the recorded paths.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Difference {
    /// An expected path that was not recorded.
    Missing {
        /// The path.
        path: String,
        /// The expected kinds and state.
        expected: (Kinds, State),
    },
    /// A recorded path that was not expected.
    Unexpected {
        /// The path.
        path: String,
        /// The recorded kinds and state.
        recorded: (Kinds, State),
    },
    /// A path recorded with other kinds or another state than expected.
    Mismatched {
        /// The path.
        path: String,
        /// The expected kinds and state.
        expected: (Kinds, State),
        /// The recorded kinds and state.
        recorded: (Kinds, State),
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
                (
                    Kinds::from_bits(observation.access),
                    State::from_recorded(&observation.state),
                ),
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
/// recorded in `steps`, and its captured output.
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

/// The expected kinds and states of `entries` for the platform the test runs on.
fn expected_here(entries: &[Entry]) -> Recorded {
    entries
        .iter()
        .filter(|entry| entry.platform.is_current())
        .map(|entry| {
            (
                entry.path.clone(),
                (entry.kinds.clone(), entry.state.clone()),
            )
        })
        .collect()
}

/// Run the fixture's two builds and the reset in a copy of it, and compare the
/// step that `coverage-test` recorded with the expectation file. Every problem is
/// reported at once, with the output of `kizz` and of the program.
#[test]
fn the_tracer_records_every_access_of_the_coverage_program() -> TestResult {
    let entries = fixture_entries()?;
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
    runs.push(run);

    let task = TaskId {
        file: copy.join("kizz.md"),
        index: TEST_BLOCK,
    };
    let (found, record) = record_problems(&step_records(cache.path())?, &task);
    problems.extend(found);
    let mut report = String::new();
    if let Some(record) = &record {
        let differences = compare(&expected_here(&entries), &recorded_under(record, &copy));
        problems.extend(differences.iter().map(ToString::to_string));
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
    assert_eq!(tracked.len(), 46);
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
/// and no path is listed twice for a platform.
#[test]
fn the_expectation_file_is_consistent() -> TestResult {
    let entries = fixture_entries()?;
    let problems = consistency_problems(&entries, &tracked_names());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(())
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
    assert_eq!(first.state, State::File { hash: true });
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
    assert_eq!(entries[1].state, State::Symlink {
        target: "../t".to_owned(),
        pointee: Box::new(State::Dir { listing: false }),
    });
    assert_eq!(entries[1].state.to_string(), "symlink:../t:dir");
    assert_eq!(entries[1].platform, Platform::All);
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
/// untracked system calls, failed calls that add more than a probe, and a path
/// listed twice for overlapping platforms, but not for disjoint ones.
#[test]
fn the_consistency_check_finds_each_problem() {
    let text = "a read+write file all sys:openat:ok:read\n\
                b read file linux sys:read:ok:read\n\
                c probe absent linux sys:openat:ENOENT:read\n\
                d probe file linux sys:stat:ok:probe\n\
                d probe file linux-x86_64 sys:stat:ok:probe\n\
                e probe file linux-x86_64 sys:stat:ok:probe\n\
                e probe file linux-aarch64 sys:statx:ok:probe\n\
                f exec file+hash all kizz:execveat:ok:exec\n";
    let entries = parse_expectations(text).unwrap();
    let problems = consistency_problems(&entries, &["openat", "stat", "statx"]);
    assert_eq!(problems, vec![
        "line 1: a has the kinds read+write, but its sources add read",
        "line 2: read is not a tracked system call",
        "line 3: the failed call openat adds read, not a probe",
        "line 3: c has the kinds probe, but its sources add read",
        "line 8: kizz starts a program only with a successful execve",
        "lines 4 and 5: d is listed twice for a platform",
    ]);
}

/// The coverage check requires, for each call, an entry on which it succeeded and,
/// for a call with a path, one on which it failed with `ENOENT`, each of which it
/// alone explains: as the only source, or with a kind that no other source adds.
#[test]
fn the_coverage_check_requires_entries_that_only_the_call_explains() {
    let text = "a probe+write dir linux sys:mkdirat:ok:write sys:mkdirat:EEXIST:probe \
                sys:openat:ok:probe\n\
                b probe absent linux sys:mkdirat:ENOENT:probe\n\
                c probe file linux sys:openat:ok:probe sys:stat:ok:probe\n\
                d probe absent linux sys:openat:ENOENT:probe sys:stat:ENOENT:probe\n\
                e list dir linux sys:getdents64:ok:list\n";
    let entries = parse_expectations(text).unwrap();
    let all: Vec<&Entry> = entries.iter().collect();
    let problems = coverage_problems(&all, &["mkdirat", "openat", "getdents64", "fchmod"]);
    assert_eq!(problems, vec![
        "openat has no entry with outcome ok that only it explains",
        "openat has no entry with outcome ENOENT that only it explains",
        "fchmod has no entry with outcome ok that only it explains",
    ]);
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
        }
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
    };
    let recorded: Vec<String> = recorded_under(&record, Path::new("/t"))
        .into_iter()
        .map(|(path, (kinds, state))| format!("{path} {kinds} {state}"))
        .collect();
    assert_eq!(recorded, vec![
        ". probe dir",
        "a/../b read+probe file",
        "l write symlink:x:absent",
    ]);
}
