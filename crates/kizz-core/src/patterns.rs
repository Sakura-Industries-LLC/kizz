// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The path patterns of `ignore` and `track`.
//!
//! A pattern is a path, a glob, or a regular expression prefixed with `re:`, and it
//! is matched against absolute paths:
//!
//! * `re:` patterns are regular expressions that match anywhere in the path unless they
//!   are anchored with `^` and `$`.
//! * Any other pattern with a glob character (`*`, `?`, `[`, or `{`) is a glob, in which
//!   `*` and `?` stay within one path component and `**` crosses components.
//! * Every other pattern is a path, which matches exactly that path.
//!
//! A relative path or glob is anchored at the task directory, and a leading `~/`
//! means the caller's home directory. Paths are normalized as the tracer reports
//! them, so `a/./b` and `a//b` match `a/b`.

use std::{os::unix::ffi::OsStrExt as _, path::Path};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use kizz_trace::AbsolutePath;
use regex::bytes::{Regex, RegexSet};

/// The prefix of a regular-expression pattern.
pub const REGEX_PREFIX: &str = "re:";

/// The prefix that stands for the caller's home directory.
const HOME_PREFIX: &str = "~/";

/// The characters that make a pattern a glob.
const GLOB_CHARACTERS: [char; 4] = ['*', '?', '[', '{'];

/// A pattern that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    /// A glob is malformed.
    #[error("invalid glob pattern `{pattern}`: {message}")]
    Glob {
        /// The pattern, as written.
        pattern: String,
        /// What is wrong with it.
        message: String,
    },
    /// A regular expression is malformed.
    #[error("invalid regular expression in pattern `{pattern}`: {message}")]
    Regex {
        /// The pattern, as written.
        pattern: String,
        /// What is wrong with it.
        message: String,
    },
    /// A pattern starts with `~/`, but the caller environment has no home directory.
    #[error("pattern `{pattern}` starts with `~/`, but the caller environment has no HOME")]
    NoHome {
        /// The pattern, as written.
        pattern: String,
    },
}

/// A set of `ignore` or `track` patterns, ready to match absolute paths.
#[derive(Debug, Clone)]
pub struct PathPatterns {
    /// The exact paths.
    paths: Vec<AbsolutePath>,
    /// The globs, anchored.
    globs: GlobSet,
    /// The regular expressions.
    regexes: RegexSet,
}

impl Default for PathPatterns {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            globs: GlobSet::empty(),
            regexes: RegexSet::empty(),
        }
    }
}

/// Whether `pattern` holds a glob character.
fn is_glob(pattern: &str) -> bool {
    pattern.contains(GLOB_CHARACTERS)
}

impl PathPatterns {
    /// Compile `patterns`, anchoring relative ones at `task_dir` and expanding a
    /// leading `~/` to `home`, the caller's home directory.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError`] for a malformed glob or regular expression, and for a
    /// pattern that starts with `~/` when `home` is `None`.
    pub fn new(
        patterns: &[String],
        task_dir: &AbsolutePath,
        home: Option<&AbsolutePath>,
    ) -> Result<Self, PatternError> {
        let mut paths = Vec::new();
        let mut globs = GlobSetBuilder::new();
        let mut regexes = Vec::new();
        for pattern in patterns {
            if let Some(regex) = pattern.strip_prefix(REGEX_PREFIX) {
                Regex::new(regex).map_err(|error| {
                    PatternError::Regex {
                        pattern: pattern.clone(),
                        message: error.to_string(),
                    }
                })?;
                regexes.push(regex);
                continue;
            }
            let (base, rest) = if let Some(rest) = pattern.strip_prefix(HOME_PREFIX) {
                let home = home.ok_or_else(|| {
                    PatternError::NoHome {
                        pattern: pattern.clone(),
                    }
                })?;
                (home, rest)
            } else {
                // An absolute pattern replaces the task directory when joined to it.
                (task_dir, pattern.as_str())
            };
            if is_glob(rest) {
                let glob = GlobBuilder::new(&anchor_glob(base, rest))
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                    .map_err(|error| {
                        PatternError::Glob {
                            pattern: pattern.clone(),
                            message: error.kind().to_string(),
                        }
                    })?;
                globs.add(glob);
            } else {
                paths.push(base.join(rest));
            }
        }
        let globs = globs.build().map_err(|error| {
            PatternError::Glob {
                pattern: error.glob().unwrap_or_default().to_owned(),
                message: error.kind().to_string(),
            }
        })?;
        let regexes = RegexSet::new(&regexes).map_err(|error| {
            PatternError::Regex {
                pattern: regexes
                    .iter()
                    .map(|regex| format!("{REGEX_PREFIX}{regex}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            paths,
            globs,
            regexes,
        })
    }

    /// Whether the set holds no pattern.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.globs.is_empty() && self.regexes.is_empty()
    }

    /// Whether any pattern matches the absolute path `path`.
    #[must_use]
    pub fn matches(
        &self,
        path: &AbsolutePath,
    ) -> bool {
        self.paths.contains(path)
            || self.globs.is_match(path.as_path())
            || self.regexes.is_match(path.as_path().as_os_str().as_bytes())
    }
}

/// The glob `rest`, anchored at `base` unless it is absolute, with `.` components and
/// repeated separators removed, as the tracer normalizes paths.
fn anchor_glob(
    base: &AbsolutePath,
    rest: &str,
) -> String {
    let anchored = if Path::new(rest).is_absolute() {
        rest.to_owned()
    } else {
        format!(
            "{}/{rest}",
            globset::escape(&base.as_path().to_string_lossy())
        )
    };
    let components: Vec<&str> = anchored
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    format!("/{}", components.join("/"))
}

/// Unit tests for each pattern form.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use super::*;

    /// An absolute path for a test.
    fn path(text: &str) -> AbsolutePath {
        AbsolutePath::new(text).unwrap()
    }

    /// The patterns `patterns` for a task in `/work/task` of a caller whose home is
    /// `/home/me`.
    fn compile(patterns: &[&str]) -> Result<PathPatterns, PatternError> {
        let patterns: Vec<String> = patterns.iter().map(|&pattern| pattern.to_owned()).collect();
        PathPatterns::new(&patterns, &path("/work/task"), Some(&path("/home/me")))
    }

    #[test]
    fn paths_match_exactly_after_anchoring() {
        let patterns = compile(&["out/./log.txt", "/etc//hosts", "~/.profile"]).unwrap();
        assert!(patterns.matches(&path("/work/task/out/log.txt")));
        assert!(patterns.matches(&path("/etc/hosts")));
        assert!(patterns.matches(&path("/home/me/.profile")));
        assert!(!patterns.matches(&path("/work/task/out/log.txt.bak")));
        assert!(!patterns.matches(&path("/work/task/out")));
        assert!(!patterns.matches(&path("/etc/hosts/x")));
    }

    #[test]
    fn globs_are_anchored_and_keep_components_apart() {
        let patterns = compile(&["build/*.o", "~/.cache/go-build/**", "/tmp/[ab]?"]).unwrap();
        assert!(patterns.matches(&path("/work/task/build/main.o")));
        assert!(!patterns.matches(&path("/work/task/build/sub/main.o")));
        assert!(!patterns.matches(&path("/elsewhere/build/main.o")));
        assert!(patterns.matches(&path("/home/me/.cache/go-build/ab/cd-d")));
        assert!(patterns.matches(&path("/tmp/a1")));
        assert!(!patterns.matches(&path("/tmp/c1")));
    }

    #[test]
    fn regular_expressions_match_absolute_paths() {
        let patterns = compile(&[r"re:\.pyc$", r"re:^/var/"]).unwrap();
        assert!(patterns.matches(&path("/work/task/a/b.pyc")));
        assert!(patterns.matches(&path("/var/log")));
        assert!(!patterns.matches(&path("/work/var/x")));
        assert!(!patterns.is_empty());
        assert!(compile(&[]).unwrap().is_empty());
    }

    #[test]
    fn task_directories_with_glob_characters_are_escaped() {
        let dir = path("/work/[x]");
        let patterns = PathPatterns::new(&["*.txt".to_owned()], &dir, None).unwrap();
        assert!(patterns.matches(&path("/work/[x]/a.txt")));
        assert!(!patterns.matches(&path("/work/x/a.txt")));
    }

    #[test]
    fn bad_patterns_are_errors() {
        assert!(matches!(
            compile(&["re:("]),
            Err(PatternError::Regex { pattern, .. }) if pattern == "re:("
        ));
        assert!(matches!(
            compile(&["a/[b"]),
            Err(PatternError::Glob { pattern, .. }) if pattern == "a/[b"
        ));
        let error = PathPatterns::new(&["~/x".to_owned()], &path("/w"), None).unwrap_err();
        assert_eq!(error, PatternError::NoHome {
            pattern: "~/x".to_owned()
        });
        assert!(error.to_string().contains("no HOME"), "{error}");
    }
}
