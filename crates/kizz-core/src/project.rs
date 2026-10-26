// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Loading a project: discovering its `kizz.md` files and parsing their tasks.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    discovery::{Discovery, KizzFile, discover},
    pool::WorkerPool,
    targets::TARGET_NAME_RULE,
    tasks::{Task, parse_tasks},
    warning::{Warning, location, shown},
};

/// The discovered tasks under one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The absolute directory discovery started in.
    pub root: PathBuf,
    /// The `kizz.md` files, each once, in the default order.
    pub files: Vec<KizzFile>,
    /// The tasks: the files in the default order, and each file's tasks in file
    /// order.
    pub tasks: Vec<Task>,
    /// Every warning from discovery, then every warning from parsing, in file order.
    pub warnings: Vec<Warning>,
}

/// A configuration error in a `kizz.md`, which kizz reports with the usage-error
/// status.
///
/// Every path is relative to the directory discovery started in.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A `kizz.md` could not be read, or is not UTF-8.
    Read {
        /// The file.
        file: PathBuf,
        /// Why it could not be read.
        source: io::Error,
    },
    /// A task block has a target that is not a valid target name.
    InvalidTarget {
        /// The `kizz.md` that holds the block.
        file: PathBuf,
        /// The line of the block's opening fence, counting from 1.
        line: usize,
        /// The nearest heading above the block, if there is one.
        heading: Option<String>,
        /// The invalid target.
        target: String,
    },
    /// A `kizz.md` has more task blocks than a task position can number.
    TooManyTasks {
        /// The file.
        file: PathBuf,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Read { file, source } => write!(f, "could not read {}: {source}", shown(file)),
            Self::InvalidTarget {
                file,
                line,
                heading,
                target,
            } => {
                write!(
                    f,
                    "{}: invalid target name `{target}`: {TARGET_NAME_RULE}",
                    location(file, *line, heading.as_deref())
                )
            },
            Self::TooManyTasks { file } => {
                write!(f, "{}: more task blocks than kizz can number", shown(file))
            },
        }
    }
}

/// Why a project could not be loaded.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The directory to start discovery in is not a readable directory.
    Root {
        /// The directory.
        root: PathBuf,
        /// What went wrong.
        source: io::Error,
    },
    /// One or more `kizz.md` files have configuration errors.
    Config {
        /// Every configuration error, in file order.
        errors: Vec<ConfigError>,
        /// Every warning found while loading, as [`Project::warnings`] would hold
        /// them, so they can still be reported with the errors.
        warnings: Vec<Warning>,
    },
}

impl fmt::Display for LoadError {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Root { root, source } => {
                write!(f, "cannot discover tasks in {}: {source}", root.display())
            },
            Self::Config { errors, .. } => {
                for (position, error) in errors.iter().enumerate() {
                    if position > 0 {
                        f.write_str("\n")?;
                    }
                    write!(f, "{error}")?;
                }
                Ok(())
            },
        }
    }
}

/// Discover every `kizz.md` under `root`, walking on `pool`, and parse its tasks.
///
/// # Errors
///
/// [`LoadError::Root`] when `root` is not a readable directory, and
/// [`LoadError::Config`] with every configuration error found when any `kizz.md`
/// cannot be read or has an invalid target name, together with every discovery and
/// parsing warning found.
pub fn load(
    root: &Path,
    pool: &WorkerPool,
) -> Result<Project, LoadError> {
    let root_error = |source| {
        LoadError::Root {
            root: root.to_path_buf(),
            source,
        }
    };
    let root = std::path::absolute(root).map_err(root_error)?;
    if !fs::metadata(&root).map_err(root_error)?.is_dir() {
        return Err(root_error(io::ErrorKind::NotADirectory.into()));
    }

    let Discovery {
        files,
        mut warnings,
    } = discover(&root, pool);
    let mut tasks: Vec<Task> = Vec::new();
    let mut errors: Vec<ConfigError> = Vec::new();
    for file in &files {
        match fs::read_to_string(&file.path) {
            Ok(source) => {
                let parsed = parse_tasks(&file.path, &file.relative, &source);
                tasks.extend(parsed.tasks);
                warnings.extend(parsed.warnings);
                errors.extend(parsed.errors);
            },
            Err(source) => {
                errors.push(ConfigError::Read {
                    file: file.relative.clone(),
                    source,
                });
            },
        }
    }
    if !errors.is_empty() {
        return Err(LoadError::Config { errors, warnings });
    }
    Ok(Project {
        root,
        files,
        tasks,
        warnings,
    })
}

/// Unit tests for loading projects.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::{fs, io, path::Path};

    use super::{ConfigError, LoadError};
    use crate::{pool::WorkerPool, tasks::Task, warning::Warning};

    /// Load the project under `root` with a worker pool of its own.
    fn load(root: &Path) -> Result<super::Project, LoadError> {
        super::load(root, &WorkerPool::new().unwrap())
    }

    /// The recursive-link warning that [`link_up`] causes for `dir`.
    fn recursive_link_warning(dir: &str) -> String {
        format!(
            "skipped the symbolic link {dir}/up: it leads back to ., one of its own parent \
             directories, so following it would recurse"
        )
    }

    /// Make `dir/up` under `root` a symbolic link to `..`, the root, which discovery
    /// must skip with a warning.
    #[cfg(unix)]
    fn link_up(
        root: &Path,
        dir: &str,
    ) -> io::Result<()> {
        std::os::unix::fs::symlink("..", root.join(dir).join("up"))
    }

    /// Make `dir/up` under `root` a symbolic link to `..`, the root, which discovery
    /// must skip with a warning.
    #[cfg(windows)]
    fn link_up(
        root: &Path,
        dir: &str,
    ) -> io::Result<()> {
        std::os::windows::fs::symlink_dir("..", root.join(dir).join("up"))
    }

    /// Write `content` to `path` under `root`, creating its directories.
    fn write(
        root: &Path,
        path: &str,
        content: &str,
    ) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Tasks come in the default order of their files, and in file order within
    /// each; each task is identified by its file's absolute path and its position.
    #[test]
    fn loads_tasks_in_the_default_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a/b/kizz.md", "# Deep\n\n```starlark ci\n```\n");
        write(
            root,
            "a/kizz.md",
            "# A\n\n```starlark ci\n```\n\n## A2\n\n~~~starlark check\n~~~\n",
        );
        write(
            root,
            "kizz.md",
            "# Top\n\n```starlark\n```\n\n```starlark ci\n```\n",
        );
        write(root, ".gitignore", "a/\n");

        let project = load(root).unwrap();
        assert_eq!(project.root, root);
        let locations: Vec<String> = project.tasks.iter().map(Task::location).collect();
        assert_eq!(locations, [
            "kizz.md:6 (Top)",
            "a/kizz.md:3 (A)",
            "a/kizz.md:8 (A2)",
            "a/b/kizz.md:3 (Deep)",
        ]);
        let ids: Vec<(&Path, u32)> = project
            .tasks
            .iter()
            .map(|task| (task.id.file.as_path(), task.id.index))
            .collect();
        assert_eq!(ids, [
            (root.join("kizz.md").as_path(), 0),
            (root.join("a/kizz.md").as_path(), 0),
            (root.join("a/kizz.md").as_path(), 1),
            (root.join("a/b/kizz.md").as_path(), 0),
        ]);
        assert_eq!(project.warnings, [Warning::UntaggedBlock {
            file: "kizz.md".into(),
            line: 3,
            heading: Some("Top".to_owned()),
        }]);
    }

    /// Invalid target names in any file make loading fail with every configuration
    /// error.
    #[test]
    fn invalid_targets_fail_loading() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "kizz.md", "# One\n\n```starlark ok bad!\n```\n");
        write(dir.path(), "sub/kizz.md", "```starlark a/b\n```\n");
        let Err(LoadError::Config { errors, .. }) = load(dir.path()) else {
            panic!("expected configuration errors");
        };
        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert_eq!(messages, [
            "kizz.md:3 (One): invalid target name `bad!`: target names use only letters, \
             digits, `_`, `.` and `-`",
            "sub/kizz.md:1: invalid target name `a/b`: target names use only letters, digits, \
             `_`, `.` and `-`",
        ]);
    }

    /// A `kizz.md` that is not UTF-8 is a configuration error that names it.
    #[test]
    fn unreadable_files_fail_loading() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("kizz.md"), [0xFF, 0xFE, 0x00]).unwrap();
        let Err(LoadError::Config { errors, .. }) = load(dir.path()) else {
            panic!("expected a configuration error");
        };
        assert!(
            matches!(&errors[..], [ConfigError::Read { file, .. }] if file == Path::new("kizz.md")),
            "{errors:?}"
        );
    }

    /// Configuration errors keep every discovery and parsing warning: a recursive
    /// link and a block without targets, with an invalid target in the same file.
    #[test]
    fn invalid_targets_keep_the_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "kizz.md",
            "# Notes\n\n```starlark\n```\n\n```starlark ci bad!\n```\n",
        );
        write(root, "a/kizz.md", "# A\n\n```starlark check\n```\n");
        link_up(root, "a").unwrap();
        let Err(LoadError::Config { errors, warnings }) = load(root) else {
            panic!("expected configuration errors");
        };
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert_eq!(errors, [
            "kizz.md:6 (Notes): invalid target name `bad!`: target names use only letters, \
             digits, `_`, `.` and `-`",
        ]);
        let warnings: Vec<String> = warnings.iter().map(ToString::to_string).collect();
        assert_eq!(warnings, [
            recursive_link_warning("a"),
            "kizz.md:3 (Notes): this starlark block has no targets, so it is not a task".to_owned(),
        ]);
    }

    /// A `kizz.md` that cannot be read keeps every discovery and parsing warning from
    /// the other files.
    #[test]
    fn unreadable_files_keep_the_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("kizz.md"), [0xFF, 0xFE, 0x00]).unwrap();
        write(root, "sub/kizz.md", "# Sub\n\n```starlark\n```\n");
        link_up(root, "sub").unwrap();
        let Err(LoadError::Config { errors, warnings }) = load(root) else {
            panic!("expected a configuration error");
        };
        assert!(
            matches!(&errors[..], [ConfigError::Read { file, .. }] if file == Path::new("kizz.md")),
            "{errors:?}"
        );
        let warnings: Vec<String> = warnings.iter().map(ToString::to_string).collect();
        assert_eq!(warnings, [
            recursive_link_warning("sub"),
            "sub/kizz.md:3 (Sub): this starlark block has no targets, so it is not a task"
                .to_owned(),
        ]);
    }

    /// A root that is missing or not a directory cannot be loaded.
    #[test]
    fn a_missing_root_fails() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "file", "");
        for root in [dir.path().join("missing"), dir.path().join("file")] {
            assert!(
                matches!(load(&root), Err(LoadError::Root { .. })),
                "{}",
                root.display()
            );
        }
    }
}
