// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Warnings found while discovering `kizz.md` files and parsing their task blocks.
//!
//! A warning never stops kizz, but it is probably a mistake, so the command line
//! shows every warning loudly and repeats it in its final summary.

use std::{
    fmt,
    path::{Path, PathBuf},
};

/// A problem found during discovery or parsing that does not stop kizz.
///
/// Every path is relative to the directory discovery started in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Warning {
    /// A symbolic link that would make the walk recurse into one of its own parent
    /// directories, so discovery skipped it.
    RecursiveLink {
        /// The symbolic link that was skipped.
        link: PathBuf,
        /// The parent directory the link leads back to.
        ancestor: PathBuf,
    },
    /// A directory or file that discovery could not read, so any `kizz.md` in or
    /// behind it was not found.
    Unreadable {
        /// The path that could not be read.
        path: PathBuf,
        /// Why it could not be read.
        reason: String,
    },
    /// A `starlark` block with neither targets nor a name, which is not a task.
    UntaggedBlock {
        /// The `kizz.md` that holds the block.
        file: PathBuf,
        /// The line of the block's opening fence, counting from 1.
        line: usize,
        /// The nearest heading above the block, if there is one.
        heading: Option<String>,
    },
}

impl fmt::Display for Warning {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::RecursiveLink { link, ancestor } => {
                write!(
                    f,
                    "skipped the symbolic link {}: it leads back to {}, one of its own parent \
                     directories, so following it would recurse",
                    shown(link),
                    shown(ancestor)
                )
            },
            Self::Unreadable { path, reason } => {
                write!(
                    f,
                    "could not read {}: {reason}; any kizz.md in or behind it was not found",
                    shown(path)
                )
            },
            Self::UntaggedBlock {
                file,
                line,
                heading,
            } => {
                write!(
                    f,
                    "{}: this starlark block has neither targets nor a name, so it is not a task",
                    location(file, *line, heading.as_deref())
                )
            },
        }
    }
}

/// A path relative to the discovery root as output shows it, with `.` for the root
/// itself.
#[must_use]
pub fn shown(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        path.display().to_string()
    }
}

/// A place in a `kizz.md` as output shows it: `path/kizz.md:LINE (Heading)`, or
/// `path/kizz.md:LINE` when no heading is above it.
#[must_use]
pub fn location(
    file: &Path,
    line: usize,
    heading: Option<&str>,
) -> String {
    match heading {
        Some(heading) => format!("{}:{line} ({heading})", shown(file)),
        None => format!("{}:{line}", shown(file)),
    }
}
