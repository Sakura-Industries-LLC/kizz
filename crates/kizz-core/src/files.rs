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
//! timestamps; a write is recorded either way. Text is UTF-8: bytes that are not
//! valid UTF-8 are replaced with U+FFFD, as with command output. JSON is written in
//! a stable form: object keys sorted by their UTF-8 bytes, `indent` spaces per
//! level, and a trailing newline.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

use globset::{GlobBuilder, GlobMatcher};
use kizz_trace::{AbsolutePath, AccessKind, Accesses};

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

/// The file operations of one task: paths resolve against the task directory, and
/// every access is recorded.
pub struct Files<'a> {
    /// The task directory.
    dir: &'a AbsolutePath,
    /// The task's accesses.
    accesses: &'a mut Accesses,
}

impl<'a> Files<'a> {
    /// The file operations of the task in `dir`, recording into `accesses`.
    pub const fn new(
        dir: &'a AbsolutePath,
        accesses: &'a mut Accesses,
    ) -> Self {
        Self { dir, accesses }
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
                self.accesses.record(path.clone(), AccessKind::Read);
                Ok(Some(bytes))
            },
            Err(error) if is_missing(&error) => {
                self.accesses.record(path.clone(), AccessKind::Probe);
                Ok(None)
            },
            Err(error) => {
                self.accesses.record(path.clone(), AccessKind::Read);
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
    /// written.
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
        self.accesses.record(path.clone(), AccessKind::Write);
        if fs::read(&path).is_ok_and(|existing| existing == content) {
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
                    self.accesses.record(dir, AccessKind::Write);
                }
                created.map_err(|error| {
                    FileError::io(
                        "cannot create the parent directories of",
                        path.clone(),
                        error,
                    )
                })?;
            }
        }
        fs::write(&path, content).map_err(|error| FileError::io("cannot write", path, error))?;
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
        let exists = fs::metadata(&path).is_ok();
        self.accesses.record(path, AccessKind::Probe);
        exists
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
                self.accesses.record(path.clone(), AccessKind::Probe);
                return Ok(None);
            },
            Err(error) => {
                self.accesses.record(path.clone(), AccessKind::List);
                return Err(FileError::io("cannot list", path.clone(), error));
            },
        };
        self.accesses.record(path.clone(), AccessKind::List);
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
        let (literal, globbed) = components.split_at(split);
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
/// Returns [`serde_json::Error`] when a value cannot be written.
pub fn stable_json(
    value: &serde_json::Value,
    indent: usize,
) -> Result<String, serde_json::Error> {
    let mut text = String::new();
    write_json_value(&mut text, value, &" ".repeat(indent), 0)?;
    text.push('\n');
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
                text.push_str(&indent.repeat(inner));
                write_json_value(text, item, indent, inner)?;
            }
            text.push('\n');
            text.push_str(&indent.repeat(level));
            text.push(']');
        },
        serde_json::Value::Object(map) if !map.is_empty() => {
            let mut members: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            members.sort_by(|a, b| a.0.cmp(b.0));
            text.push('{');
            for (index, (key, member)) in members.into_iter().enumerate() {
                text.push_str(if index == 0 { "\n" } else { ",\n" });
                text.push_str(&indent.repeat(inner));
                text.push_str(&serde_json::to_string(key)?);
                text.push_str(": ");
                write_json_value(text, member, indent, inner)?;
            }
            text.push('\n');
            text.push_str(&indent.repeat(level));
            text.push('}');
        },
        other => text.push_str(&serde_json::to_string(other)?),
    }
    Ok(())
}

/// Unit tests for the file operations.
#[cfg(test)]
mod tests {
    use std::{fs, io, path::Path};

    use kizz_trace::{AbsolutePath, Accesses};
    use serde_json::json;

    use super::{Files, stable_json};

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

    /// A write creates parent directories and records them, and a write with
    /// unchanged content does not touch the file.
    #[test]
    fn writes_create_parents_and_skip_unchanged_content() {
        let root = tempfile::tempdir().unwrap();
        let dir = AbsolutePath::new(root.path()).unwrap();
        let mut accesses = Accesses::new();
        let mut files = Files::new(&dir, &mut accesses);
        assert!(files.write_bytes("a/b/c.txt", b"one").unwrap());
        assert!(!files.write_bytes("a/b/c.txt", b"one").unwrap());
        assert!(files.write_bytes("a/b/c.txt", b"two").unwrap());
        assert_eq!(fs::read(root.path().join("a/b/c.txt")).unwrap(), b"two");
        assert_eq!(shown(root.path(), &accesses), [
            ("a".to_owned(), "write".to_owned()),
            ("a/b".to_owned(), "write".to_owned()),
            ("a/b/c.txt".to_owned(), "write".to_owned()),
        ]);
    }

    /// Text reads replace bytes that are not valid UTF-8.
    #[test]
    fn text_reads_replace_invalid_utf8() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("bytes"), b"ok \xff end").unwrap();
        let dir = AbsolutePath::new(root.path()).unwrap();
        let mut accesses = Accesses::new();
        let mut files = Files::new(&dir, &mut accesses);
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
        let mut files = Files::new(&dir, &mut accesses);
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
