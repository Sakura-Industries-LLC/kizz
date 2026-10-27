// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The file operations of task programs, behind the `fs` functions and the dotenv
//! reads of `env.load_file` and `env.read`.
//!
//! Every operation resolves its path against the task directory, where an
//! absolute path stands for itself and paths outside the tree are allowed, and it
//! records each access it makes in the task's [`Accesses`]:
//!
//! * a read of a file's content is a `read`, and a path found missing is a `probe`;
//! * an existence check is a `probe`;
//! * reading a directory's entries is a `list`, and a missing directory is a `probe`;
//! * a write is a `write` of the file and of every parent directory it created.
//!
//! Writes happen only when the content differs, so an unchanged file keeps its
//! timestamps; a write is recorded either way. A write that changes the file is
//! also a change, which [`FileChanges`] keeps with when it happened, so a pass in
//! which a task program changed a file does work and the task is the file's last
//! writer. Text is UTF-8: bytes that are not valid UTF-8 are replaced with U+FFFD, as
//! with command output. JSON is written in a stable form: object keys sorted by their
//! UTF-8 bytes, `indent` spaces per level, and a trailing newline.
//!
//! For the observation report of `kizz run --output`, the operations can also note
//! how each access ended ([`Files::noting`]): into [`AccessOutcomes`], `ok` for an
//! access whose operation succeeded, and otherwise the number of the system's
//! error, which [`AccessOutcomes::observations`] names through the tracer's
//! error-name query, such as `ENOENT` for a missing file's probe. Without it, as in
//! every build, nothing more than the accesses is kept.
//!
//! No operation can panic ("Kizz never panics"): a glob pattern is split at its
//! first glob component with `slice::get`, never `split_at`, and the indentation of
//! JSON is built in space reserved with `String::try_reserve_exact`, never with
//! `str::repeat`, which panics when the length overflows, so an indent too large
//! for memory is an error of the write.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    time::SystemTime,
};

use globset::{GlobBuilder, GlobMatcher};
use kizz_trace::{AbsolutePath, AccessKind, Accesses, Observations, Outcome};
use serde::ser::Error as _;

/// A file operation that failed.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// The file system reported an error for a path.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was being done, such as `cannot read`.
        action: &'static str,
        /// The path.
        path: AbsolutePath,
        /// The error.
        source: io::Error,
    },
    /// A file did not hold valid JSON.
    #[error("{path} does not hold valid JSON: {source}")]
    InvalidJson {
        /// The file.
        path: AbsolutePath,
        /// What is wrong with it.
        source: serde_json::Error,
    },
    /// A value could not be written as JSON.
    #[error("cannot write {path} as JSON: {message}")]
    NotJson {
        /// The file.
        path: AbsolutePath,
        /// Why the value has no JSON form.
        message: String,
    },
    /// A glob pattern is not valid.
    #[error("invalid glob pattern `{pattern}`: {message}")]
    Pattern {
        /// The pattern.
        pattern: String,
        /// What is wrong with it.
        message: String,
    },
}

impl FileError {
    /// An I/O error of `action` on `path`.
    const fn io(
        action: &'static str,
        path: AbsolutePath,
        source: io::Error,
    ) -> Self {
        Self::Io {
            action,
            path,
            source,
        }
    }
}

/// Whether an error says that a path, or one of its parent directories, does not
/// exist.
fn is_missing(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// A file that a task program changed, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// The absolute path of the file.
    pub path: AbsolutePath,
    /// When the program last changed it, in nanoseconds since the Unix epoch.
    pub at_nanos: u64,
}

/// The files that a task program changed through `fs.write` and `fs.write_json`, in
/// the order it first changed each one, without repeats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileChanges {
    /// The changed files.
    files: Vec<ChangedFile>,
}

impl FileChanges {
    /// No changes.
    #[must_use]
    pub const fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Note that the file at `path` changed at `at_nanos`; a file already noted
    /// keeps its place and takes the later time.
    pub fn record(
        &mut self,
        path: AbsolutePath,
        at_nanos: u64,
    ) {
        if let Some(file) = self.files.iter_mut().find(|file| file.path == path) {
            file.at_nanos = file.at_nanos.max(at_nanos);
        } else {
            self.files.push(ChangedFile { path, at_nanos });
        }
    }

    /// Add the changes of `other`, which came after these.
    pub fn merge(
        &mut self,
        other: Self,
    ) {
        for file in other.files {
            self.record(file.path, file.at_nanos);
        }
    }

    /// Whether no file changed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The changed files, in the order they first changed.
    #[must_use]
    pub fn files(&self) -> &[ChangedFile] {
        &self.files
    }

    /// The paths of the changed files, in the order they first changed.
    #[must_use]
    pub fn paths(&self) -> Vec<AbsolutePath> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }
}

/// Now, in nanoseconds since the Unix epoch.
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// How one file operation of a task program ended, before its error is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Noted {
    /// The operation succeeded.
    Ok,
    /// The operation failed with the system's error of this number.
    Errno(i32),
    /// The operation failed with an error that has no number.
    Unnumbered,
}

impl Noted {
    /// How an operation ended that failed with `error`, or succeeded when it is
    /// `None`.
    fn of(error: Option<&io::Error>) -> Self {
        match error {
            None => Self::Ok,
            Some(error) => error.raw_os_error().map_or(Self::Unnumbered, Self::Errno),
        }
    }

    /// The outcome as a report shows it: `ok`, the error's name, which `error_name`
    /// gives its number, or `unknown` for an error without a number.
    fn outcome(
        self,
        error_name: &impl Fn(i32) -> String,
    ) -> Outcome {
        match self {
            Self::Ok => Outcome::Ok,
            Self::Errno(errno) => Outcome::Error(error_name(errno)),
            Self::Unnumbered => Outcome::Unknown,
        }
    }
}

/// How the file accesses of a task program ended, for the observation report of
/// `kizz run --output`: each path with its access kinds, and each kind with how
/// the operations that made it ended, before the errors are named.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessOutcomes {
    /// How the accesses of each kind to each path ended.
    noted: BTreeMap<AbsolutePath, BTreeMap<AccessKind, BTreeSet<Noted>>>,
}

impl AccessOutcomes {
    /// No accesses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that an operation touched `path` in the way `kind` names, and failed
    /// with `error`, or succeeded when it is `None`.
    pub fn note(
        &mut self,
        path: AbsolutePath,
        kind: AccessKind,
        error: Option<&io::Error>,
    ) {
        self.noted
            .entry(path)
            .or_default()
            .entry(kind)
            .or_default()
            .insert(Noted::of(error));
    }

    /// Add every access of `other`.
    pub fn merge(
        &mut self,
        other: Self,
    ) {
        for (path, kinds) in other.noted {
            let known = self.noted.entry(path).or_default();
            for (kind, outcomes) in kinds {
                known.entry(kind).or_default().extend(outcomes);
            }
        }
    }

    /// Whether no access was noted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.noted.is_empty()
    }

    /// The accesses as observations: `ok`, the name that `error_name` gives the
    /// number of an error, such as the tracer's error-name query, or `unknown` for
    /// an error without a number.
    #[must_use]
    pub fn observations(
        &self,
        error_name: impl Fn(i32) -> String,
    ) -> Observations {
        let mut observations = Observations::new();
        for (path, kinds) in &self.noted {
            for (kind, outcomes) in kinds {
                for noted in outcomes {
                    observations.record(path.clone(), *kind, noted.outcome(&error_name));
                }
            }
        }
        observations
    }
}

/// The file operations of one task: paths resolve against the task directory, and
/// every access is recorded, with every file a write changed.
pub struct Files<'a> {
    /// The task directory.
    dir: &'a AbsolutePath,
    /// The task's accesses.
    accesses: &'a mut Accesses,
    /// The files the task's writes changed.
    changes: &'a mut FileChanges,
    /// How each access ended, when the run notes it for a report.
    outcomes: Option<&'a mut AccessOutcomes>,
}

impl<'a> Files<'a> {
    /// The file operations of the task in `dir`, recording into `accesses` and,
    /// for writes that change a file, into `changes`.
    pub const fn new(
        dir: &'a AbsolutePath,
        accesses: &'a mut Accesses,
        changes: &'a mut FileChanges,
    ) -> Self {
        Self {
            dir,
            accesses,
            changes,
            outcomes: None,
        }
    }

    /// The file operations, which also note how each access ended into `outcomes`
    /// when it is `Some`, for the observation report of `kizz run --output`.
    #[must_use]
    pub fn noting(
        mut self,
        outcomes: Option<&'a mut AccessOutcomes>,
    ) -> Self {
        self.outcomes = outcomes;
        self
    }

    /// Record that an operation touched `path` in the way `kind` names, and note
    /// that it failed with `error`, or succeeded when it is `None`, when the
    /// outcomes are noted.
    fn record(
        &mut self,
        path: AbsolutePath,
        kind: AccessKind,
        error: Option<&io::Error>,
    ) {
        if let Some(outcomes) = &mut self.outcomes {
            outcomes.note(path.clone(), kind, error);
        }
        self.accesses.record(path, kind);
    }

    /// The absolute path of `path`, relative to the task directory unless absolute.
    #[must_use]
    pub fn resolve(
        &self,
        path: &str,
    ) -> AbsolutePath {
        self.dir.join(path)
    }

    /// Read the bytes of the file at `path`, or `None` when it is missing; the
    /// file is recorded as read, or probed when missing.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when the file exists but cannot be read.
    pub fn read_bytes_if_present(
        &mut self,
        path: &AbsolutePath,
    ) -> Result<Option<Vec<u8>>, FileError> {
        match fs::read(path) {
            Ok(bytes) => {
                self.record(path.clone(), AccessKind::Read, None);
                Ok(Some(bytes))
            },
            Err(error) if is_missing(&error) => {
                self.record(path.clone(), AccessKind::Probe, Some(&error));
                Ok(None)
            },
            Err(error) => {
                self.record(path.clone(), AccessKind::Read, Some(&error));
                Err(FileError::io("cannot read", path.clone(), error))
            },
        }
    }

    /// Read the bytes of the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when the file is missing or cannot be read.
    pub fn read_bytes(
        &mut self,
        path: &AbsolutePath,
    ) -> Result<Vec<u8>, FileError> {
        self.read_bytes_if_present(path)?.ok_or_else(|| {
            FileError::io(
                "cannot read",
                path.clone(),
                io::Error::from(ErrorKind::NotFound),
            )
        })
    }

    /// Read the file at `path` as text, with bytes that are not valid UTF-8
    /// replaced.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when the file is missing or cannot be read.
    pub fn read_text(
        &mut self,
        path: &str,
    ) -> Result<String, FileError> {
        let path = self.resolve(path);
        let bytes = self.read_bytes(&path)?;
        Ok(match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        })
    }

    /// Write `content` to the file at `path`, creating parent directories, unless
    /// the file already holds exactly `content`. Returns whether the file was
    /// written; a written file is noted as changed. A write that leaves the file as
    /// it was succeeds; each parent directory it created has the outcome of
    /// creating them.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when a directory cannot be created or the file cannot
    /// be written.
    pub fn write_bytes(
        &mut self,
        path: &str,
        content: &[u8],
    ) -> Result<bool, FileError> {
        let path = self.resolve(path);
        if fs::read(&path).is_ok_and(|existing| existing == content) {
            self.record(path, AccessKind::Write, None);
            return Ok(false);
        }
        if let Some(parent) = path.as_path().parent() {
            let missing: Vec<AbsolutePath> = parent
                .ancestors()
                .take_while(|dir| fs::symlink_metadata(dir).is_err())
                .filter_map(|dir| AbsolutePath::new(dir).ok())
                .collect();
            if !missing.is_empty() {
                let created = fs::create_dir_all(parent);
                for dir in missing {
                    self.record(dir, AccessKind::Write, created.as_ref().err());
                }
                if let Err(error) = created {
                    self.record(path.clone(), AccessKind::Write, Some(&error));
                    return Err(FileError::io(
                        "cannot create the parent directories of",
                        path.clone(),
                        error,
                    ));
                }
            }
        }
        let written = fs::write(&path, content);
        self.record(path.clone(), AccessKind::Write, written.as_ref().err());
        written.map_err(|error| FileError::io("cannot write", path.clone(), error))?;
        self.changes.record(path, now_nanos());
        Ok(true)
    }

    /// Read the file at `path` as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when the file is missing, cannot be read, or is not
    /// valid JSON.
    pub fn read_json(
        &mut self,
        path: &str,
    ) -> Result<serde_json::Value, FileError> {
        let path = self.resolve(path);
        let bytes = self.read_bytes(&path)?;
        serde_json::from_slice(&bytes).map_err(|source| FileError::InvalidJson { path, source })
    }

    /// Write `value` to the file at `path` as stable JSON with `indent` spaces per
    /// level and a trailing newline, unless the file already holds exactly that
    /// text. Returns whether the file was written.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when the file cannot be written.
    pub fn write_json(
        &mut self,
        path: &str,
        value: &serde_json::Value,
        indent: usize,
    ) -> Result<bool, FileError> {
        let text = stable_json(value, indent).map_err(|error| {
            FileError::NotJson {
                path: self.resolve(path),
                message: error.to_string(),
            }
        })?;
        self.write_bytes(path, text.as_bytes())
    }

    /// Whether `path` exists, following symbolic links; the path is recorded as
    /// probed.
    pub fn exists(
        &mut self,
        path: &str,
    ) -> bool {
        let path = self.resolve(path);
        let found = fs::metadata(&path);
        self.record(path, AccessKind::Probe, found.as_ref().err());
        found.is_ok()
    }

    /// The names of the entries of the directory at `path`, sorted, with bytes
    /// that are not valid UTF-8 replaced.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when the directory is missing or cannot be read.
    pub fn list(
        &mut self,
        path: &str,
    ) -> Result<Vec<String>, FileError> {
        let path = self.resolve(path);
        self.list_dir(&path)?
            .ok_or_else(|| FileError::io("cannot list", path, io::Error::from(ErrorKind::NotFound)))
            .map(|entries| entries.into_iter().map(|(name, _)| name).collect())
    }

    /// The sorted entries of the directory at `path`, each with whether it is a
    /// directory, following symbolic links; `None` when the directory is missing.
    fn list_dir(
        &mut self,
        path: &AbsolutePath,
    ) -> Result<Option<Vec<(String, bool)>>, FileError> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if is_missing(&error) => {
                self.record(path.clone(), AccessKind::Probe, Some(&error));
                return Ok(None);
            },
            Err(error) => {
                self.record(path.clone(), AccessKind::List, Some(&error));
                return Err(FileError::io("cannot list", path.clone(), error));
            },
        };
        self.record(path.clone(), AccessKind::List, None);
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| FileError::io("cannot list", path.clone(), error))?;
            let is_dir = fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir());
            names.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
        }
        names.sort();
        Ok(Some(names))
    }

    /// The paths that match the glob `pattern`, sorted.
    ///
    /// The pattern is split at its first component with a glob character: the
    /// components before it name the directory the search starts in, relative to
    /// the task directory unless absolute, and the rest is matched against paths
    /// below it. `*`, `?`, `[...]`, and `{a,b}` stay within one component, and `**`
    /// matches any number of components. Names that start with `.` match like any
    /// other. Each match is the starting directory as written, joined with the
    /// matching path. Every directory read is recorded as listed, and a missing one
    /// as probed; a pattern without glob characters probes its one path.
    ///
    /// # Errors
    ///
    /// Returns [`FileError`] when the pattern is not valid or a directory cannot be
    /// read.
    pub fn glob(
        &mut self,
        pattern: &str,
    ) -> Result<Vec<String>, FileError> {
        let invalid = |message: String| {
            FileError::Pattern {
                pattern: pattern.to_owned(),
                message,
            }
        };
        if pattern.is_empty() {
            return Err(invalid("the pattern is empty".to_owned()));
        }
        let components: Vec<&str> = pattern.split('/').collect();
        let split = components
            .iter()
            .position(|component| component.contains(['*', '?', '[', ']', '{', '}', '\\']))
            .unwrap_or(components.len());
        // `split` is at most the number of components, and `get` turns an index past
        // the end into empty parts, where `split_at` would panic.
        let literal = components.get(..split).unwrap_or_default();
        let globbed = components.get(split..).unwrap_or_default();
        let joined = literal.join("/");
        let start = if joined.is_empty() && pattern.starts_with('/') {
            "/".to_owned()
        } else {
            joined
        };
        if globbed.is_empty() {
            return Ok(if self.exists(pattern) {
                vec![pattern.to_owned()]
            } else {
                Vec::new()
            });
        }
        let rest = globbed.join("/");
        let matcher = GlobBuilder::new(&rest)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| invalid(error.kind().to_string()))?
            .compile_matcher();
        let depth = if rest.contains("**") {
            None
        } else {
            Some(globbed.len())
        };
        let mut search = GlobSearch {
            matcher,
            depth,
            found: Vec::new(),
            visiting: BTreeSet::new(),
        };
        let base = self.resolve(if start.is_empty() { "." } else { &start });
        self.glob_dir(&mut search, &base, Path::new(""), 1)?;
        let start = Path::new(&start);
        let mut found: Vec<String> = search
            .found
            .iter()
            .map(|relative| start.join(relative).to_string_lossy().into_owned())
            .collect();
        found.sort();
        Ok(found)
    }

    /// Search the directory `dir`, at `relative` below the starting directory and
    /// `level` components deep, for matches of `search`.
    fn glob_dir(
        &mut self,
        search: &mut GlobSearch,
        dir: &AbsolutePath,
        relative: &Path,
        level: usize,
    ) -> Result<(), FileError> {
        let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.as_path().to_path_buf());
        if !search.visiting.insert(canonical.clone()) {
            return Ok(());
        }
        let Some(entries) = self.list_dir(dir)? else {
            return Ok(());
        };
        for (name, is_dir) in entries {
            let path = relative.join(&name);
            if search.matcher.is_match(&path) {
                search.found.push(path.clone());
            }
            if is_dir && search.depth.is_none_or(|depth| level < depth) {
                let next = level.saturating_add(1);
                self.glob_dir(search, &dir.join(&name), &path, next)?;
            }
        }
        search.visiting.remove(&canonical);
        Ok(())
    }
}

/// The state of one `fs.glob` search.
struct GlobSearch {
    /// Matches paths below the starting directory.
    matcher: GlobMatcher,
    /// How many components deep a match can be, or `None` for any depth.
    depth: Option<usize>,
    /// The matching paths, relative to the starting directory.
    found: Vec<PathBuf>,
    /// The canonical paths of the directories being searched, outermost first, so
    /// a symbolic link back to one of them is not followed again.
    visiting: BTreeSet<PathBuf>,
}

/// `value` as stable JSON: object keys sorted by their UTF-8 bytes, each element
/// and member on its own line indented by `indent` spaces per level, `": "`
/// between a key and its value, empty arrays and objects as `[]` and `{}`, and a
/// trailing newline.
///
/// # Errors
///
/// Returns [`serde_json::Error`] when a value cannot be written, or when the
/// indentation of a line cannot be allocated, such as for an indent too large for
/// memory, where `str::repeat` would panic.
pub fn stable_json(
    value: &serde_json::Value,
    indent: usize,
) -> Result<String, serde_json::Error> {
    let mut text = String::new();
    write_json_value(&mut text, value, &repeated(" ", indent)?, 0)?;
    text.push('\n');
    Ok(text)
}

/// `unit` repeated `count` times, or an error when its length cannot be
/// allocated: the length is reserved first with `String::try_reserve_exact`, which
/// returns an error where `str::repeat` panics, so the pushes that follow fit.
///
/// # Errors
///
/// Returns [`serde_json::Error`] when the length overflows or cannot be allocated.
fn repeated(
    unit: &str,
    count: usize,
) -> Result<String, serde_json::Error> {
    let mut text = String::new();
    text.try_reserve_exact(unit.len().saturating_mul(count))
        .map_err(|error| {
            serde_json::Error::custom(format_args!(
                "cannot indent by {count} times {} bytes: {error}",
                unit.len()
            ))
        })?;
    for _ in 0..count {
        text.push_str(unit);
    }
    Ok(text)
}

/// Append `value`, nested `level` deep, to `text` as stable JSON.
fn write_json_value(
    text: &mut String,
    value: &serde_json::Value,
    indent: &str,
    level: usize,
) -> Result<(), serde_json::Error> {
    let inner = level.saturating_add(1);
    match value {
        serde_json::Value::Array(items) if !items.is_empty() => {
            text.push('[');
            for (index, item) in items.iter().enumerate() {
                text.push_str(if index == 0 { "\n" } else { ",\n" });
                text.push_str(&repeated(indent, inner)?);
                write_json_value(text, item, indent, inner)?;
            }
            text.push('\n');
            text.push_str(&repeated(indent, level)?);
            text.push(']');
        },
        serde_json::Value::Object(map) if !map.is_empty() => {
            let mut members: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            members.sort_by(|a, b| a.0.cmp(b.0));
            text.push('{');
            for (index, (key, member)) in members.into_iter().enumerate() {
                text.push_str(if index == 0 { "\n" } else { ",\n" });
                text.push_str(&repeated(indent, inner)?);
                text.push_str(&serde_json::to_string(key)?);
                text.push_str(": ");
                write_json_value(text, member, indent, inner)?;
            }
            text.push('\n');
            text.push_str(&repeated(indent, level)?);
            text.push('}');
        },
        other => text.push_str(&serde_json::to_string(other)?),
    }
    Ok(())
}

/// Unit tests for the file operations.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::{fs, io, path::Path};

    use kizz_trace::{AbsolutePath, AccessKind, Accesses};
    use serde_json::json;

    use super::{AccessOutcomes, FileChanges, Files, repeated, stable_json};

    /// The observations of `outcomes`, with `ENOENT` for the error number 2, as
    /// `(path relative to root, "KIND OUTCOMES; ...")` pairs.
    fn noted(
        root: &Path,
        outcomes: &AccessOutcomes,
    ) -> Vec<(String, String)> {
        let names = |errno: i32| {
            if errno == 2 {
                "ENOENT".to_owned()
            } else {
                format!("errno {errno}")
            }
        };
        outcomes
            .observations(names)
            .iter()
            .map(|(path, kinds)| {
                let relative = path.as_path().strip_prefix(root).unwrap_or(path.as_path());
                let kinds: Vec<String> = kinds
                    .iter()
                    .map(|(kind, outcomes)| {
                        let outcomes: Vec<String> =
                            outcomes.iter().map(ToString::to_string).collect();
                        format!("{} {}", kind.name(), outcomes.join(","))
                    })
                    .collect();
                (relative.display().to_string(), kinds.join("; "))
            })
            .collect()
    }

    /// With noting, each access has its outcome: a read, a listing and the writes
    /// that succeed are `ok`, whether a write changes the file or not and for the
    /// parent it creates; a missing file's probe, a missing path's existence check
    /// and a missing directory's probe carry the system's error, `ENOENT`; and the
    /// noted paths and kinds are exactly the recorded accesses.
    #[test]
    fn noted_accesses_carry_their_outcomes() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("in.txt"), "in").unwrap();
        fs::create_dir(root.path().join("sub")).unwrap();
        fs::write(root.path().join("sub/a.txt"), "a").unwrap();
        let dir = AbsolutePath::new(root.path()).unwrap();
        let mut accesses = Accesses::new();
        let mut changes = FileChanges::new();
        let mut outcomes = AccessOutcomes::new();
        let mut files = Files::new(&dir, &mut accesses, &mut changes).noting(Some(&mut outcomes));
        assert_eq!(files.read_text("in.txt").unwrap(), "in");
        assert!(files.read_text("missing.txt").is_err());
        assert!(!files.exists("gone"));
        assert!(files.list("nowhere").is_err());
        assert_eq!(files.list("sub").unwrap(), ["a.txt"]);
        assert!(files.write_bytes("out/new.txt", b"new").unwrap());
        assert!(!files.write_bytes("in.txt", b"in").unwrap());
        assert_eq!(noted(root.path(), &outcomes), [
            ("gone".to_owned(), "probe ENOENT".to_owned()),
            ("in.txt".to_owned(), "read ok; write ok".to_owned()),
            ("missing.txt".to_owned(), "probe ENOENT".to_owned()),
            ("nowhere".to_owned(), "probe ENOENT".to_owned()),
            ("out".to_owned(), "write ok".to_owned()),
            ("out/new.txt".to_owned(), "write ok".to_owned()),
            ("sub".to_owned(), "list ok".to_owned()),
        ]);
        assert_eq!(
            outcomes.observations(|errno| errno.to_string()).accesses(),
            accesses
        );
    }

    /// An error without a number is `unknown`; merging keeps every outcome of a
    /// kind once; and nothing noted is empty.
    #[test]
    fn outcomes_merge_and_name_errors_without_numbers_unknown() {
        let path = AbsolutePath::new("/w/f").unwrap();
        let mut outcomes = AccessOutcomes::new();
        assert!(outcomes.is_empty());
        outcomes.note(path.clone(), AccessKind::Read, None);
        let mut other = AccessOutcomes::new();
        other.note(
            path.clone(),
            AccessKind::Read,
            Some(&io::Error::other("no number")),
        );
        other.note(path.clone(), AccessKind::Read, None);
        other.note(
            path,
            AccessKind::Probe,
            Some(&io::Error::from_raw_os_error(13)),
        );
        outcomes.merge(other);
        assert!(!outcomes.is_empty());
        assert_eq!(noted(Path::new("/w"), &outcomes), [(
            "f".to_owned(),
            "read ok,unknown; probe errno 13".to_owned()
        )]);
    }

    /// The accesses as `(path relative to root, kinds)` pairs.
    fn shown(
        root: &Path,
        accesses: &Accesses,
    ) -> Vec<(String, String)> {
        accesses
            .iter()
            .map(|(path, kinds)| {
                let relative = path.as_path().strip_prefix(root).unwrap_or(path.as_path());
                (relative.display().to_string(), kinds.to_string())
            })
            .collect()
    }

    /// Make `link` a symbolic link to the directory `target`.
    #[cfg(unix)]
    fn link_dir(
        target: &Path,
        link: &Path,
    ) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    /// Make `link` a symbolic link to the directory `target`.
    #[cfg(windows)]
    fn link_dir(
        target: &Path,
        link: &Path,
    ) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    /// Stable JSON sorts keys at every level, indents, and ends with a newline.
    #[test]
    fn stable_json_sorts_keys_and_ends_with_a_newline() {
        let value = json!({"b": [1, {"z": true, "a": null}], "a": "x", "B": 2.5, "e": {}, "l": []});
        assert_eq!(
            stable_json(&value, 2).unwrap(),
            "{\n  \"B\": 2.5,\n  \"a\": \"x\",\n  \"b\": [\n    1,\n    {\n      \"a\": null,\n      \
             \"z\": true\n    }\n  ],\n  \"e\": {},\n  \"l\": []\n}\n"
        );
        assert_eq!(
            stable_json(&json!({"k": [1]}), 4).unwrap(),
            "{\n    \"k\": [\n        1\n    ]\n}\n"
        );
        assert_eq!(
            stable_json(&json!({"k": [1]}), 0).unwrap(),
            "{\n\"k\": [\n1\n]\n}\n"
        );
        assert_eq!(stable_json(&json!("s"), 2).unwrap(), "\"s\"\n");
    }

    /// An indentation whose length cannot be reserved is an error of the write,
    /// where `str::repeat` would panic: a count of `usize::MAX` overflows the
    /// largest length a `String` can reserve, so the reservation fails at once,
    /// without allocating, for one byte and, through the saturating product, for
    /// two bytes per level; a length that fits is built in full.
    #[test]
    fn an_indentation_that_cannot_be_reserved_is_an_error() {
        let error = stable_json(&json!({"k": [1]}), usize::MAX).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(&format!("cannot indent by {} times 1 bytes: ", usize::MAX)),
            "{error}"
        );
        let error = repeated("  ", usize::MAX).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(&format!("cannot indent by {} times 2 bytes: ", usize::MAX)),
            "{error}"
        );
        assert_eq!(repeated("ab ", 2).unwrap(), "ab ab ");
        assert_eq!(repeated("ab", 0).unwrap(), "");
    }

    /// A write creates parent directories and records them, and a write with
    /// unchanged content does not touch the file; only the writes that changed the
    /// file are changes, noted once, in order.
    #[test]
    fn writes_create_parents_and_skip_unchanged_content() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("same.txt"), "same").unwrap();
        let dir = AbsolutePath::new(root.path()).unwrap();
        let mut accesses = Accesses::new();
        let mut changes = FileChanges::new();
        let mut files = Files::new(&dir, &mut accesses, &mut changes);
        assert!(files.write_bytes("a/b/c.txt", b"one").unwrap());
        assert!(!files.write_bytes("a/b/c.txt", b"one").unwrap());
        assert!(!files.write_bytes("same.txt", b"same").unwrap());
        assert!(files.write_bytes("d.txt", b"d").unwrap());
        assert!(files.write_bytes("a/b/c.txt", b"two").unwrap());
        assert_eq!(fs::read(root.path().join("a/b/c.txt")).unwrap(), b"two");
        assert_eq!(shown(root.path(), &accesses), [
            ("a".to_owned(), "write".to_owned()),
            ("a/b".to_owned(), "write".to_owned()),
            ("a/b/c.txt".to_owned(), "write".to_owned()),
            ("d.txt".to_owned(), "write".to_owned()),
            ("same.txt".to_owned(), "write".to_owned()),
        ]);
        assert_eq!(changes.paths(), [dir.join("a/b/c.txt"), dir.join("d.txt")]);
        let [first, second] = changes.files() else {
            panic!("{changes:?}");
        };
        // The file changed twice keeps its first place and takes its last time.
        assert!(first.at_nanos >= second.at_nanos, "{changes:?}");
    }

    /// Merged changes keep the order of first change, without repeats, and the
    /// later time of a file changed in both.
    #[test]
    fn changes_merge_in_order_without_repeats() {
        let path = |name: &str| AbsolutePath::new(format!("/w/{name}")).unwrap();
        let mut changes = FileChanges::new();
        assert!(changes.is_empty());
        changes.record(path("a"), 5);
        changes.record(path("b"), 6);
        let mut later = FileChanges::new();
        later.record(path("c"), 7);
        later.record(path("a"), 8);
        changes.merge(later);
        assert!(!changes.is_empty());
        assert_eq!(changes.paths(), [path("a"), path("b"), path("c")]);
        let times: Vec<u64> = changes.files().iter().map(|file| file.at_nanos).collect();
        assert_eq!(times, [8, 6, 7]);
    }

    /// Text reads replace bytes that are not valid UTF-8.
    #[test]
    fn text_reads_replace_invalid_utf8() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("bytes"), b"ok \xff end").unwrap();
        let dir = AbsolutePath::new(root.path()).unwrap();
        let mut accesses = Accesses::new();
        let mut changes = FileChanges::new();
        let mut files = Files::new(&dir, &mut accesses, &mut changes);
        assert_eq!(
            files.read_text("bytes").unwrap(),
            format!("ok {} end", char::REPLACEMENT_CHARACTER)
        );
    }

    /// Glob patterns split at their first glob component, `*` stays within one
    /// component, `**` crosses any number, and a link back to a searched
    /// directory is not followed again.
    #[test]
    fn glob_patterns() {
        let root = tempfile::tempdir().unwrap();
        for file in [
            "a.txt",
            "b.md",
            ".hidden.txt",
            "src/x.txt",
            "src/deep/y.txt",
            "src/deep/z.md",
        ] {
            let path = root.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        link_dir(&root.path().join("src"), &root.path().join("src/deep/loop")).unwrap();
        let dir = AbsolutePath::new(root.path()).unwrap();
        let mut accesses = Accesses::new();
        let mut changes = FileChanges::new();
        let mut files = Files::new(&dir, &mut accesses, &mut changes);
        assert_eq!(files.glob("*.txt").unwrap(), [".hidden.txt", "a.txt"]);
        assert_eq!(files.glob("src/*.txt").unwrap(), ["src/x.txt"]);
        assert_eq!(files.glob("*/*.txt").unwrap(), ["src/x.txt"]);
        assert_eq!(files.glob("src/**/*.txt").unwrap(), [
            "src/deep/y.txt",
            "src/x.txt"
        ]);
        assert_eq!(files.glob("**/loop").unwrap(), ["src/deep/loop"]);
        assert_eq!(files.glob("{a,b}.*").unwrap(), ["a.txt", "b.md"]);
        assert_eq!(files.glob("b.md").unwrap(), ["b.md"]);
        assert_eq!(files.glob("nope/*.txt").unwrap(), Vec::<String>::new());
        let absolute = format!("{}/src/*.txt", root.path().display());
        assert_eq!(files.glob(&absolute).unwrap(), [format!(
            "{}/src/x.txt",
            root.path().display()
        )]);
        assert!(files.glob("[").is_err());
        assert!(files.glob("").is_err());
    }
}
