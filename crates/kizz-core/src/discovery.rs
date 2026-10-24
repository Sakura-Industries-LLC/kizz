// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Discovery of `kizz.md` files.
//!
//! Discovery starts in one directory and never looks above it. It finds every file
//! named exactly `kizz.md` in that directory and every subdirectory, hidden
//! directories included, with the `ignore` crate's parallel walker and all of its
//! filtering off, so no Git or ignore-file rule hides a `kizz.md`.
//!
//! Symbolic links to directories are followed, except a link that would make the walk
//! recurse into one of its own parent directories, which is skipped with a warning.
//! A `kizz.md` reached through several paths is kept once, under the first of them in
//! the default order of [`default_order`].

use std::{
    cmp::Ordering,
    collections::HashSet,
    ffi::OsStr,
    fs, io,
    path::{Component, Path, PathBuf},
    sync::mpsc,
};

use ignore::{WalkBuilder, WalkState};

use crate::warning::Warning;

/// The name of every file kizz reads to find work.
pub const KIZZ_FILE_NAME: &str = "kizz.md";

/// A discovered `kizz.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KizzFile {
    /// The absolute path through which discovery reached the file: the discovery
    /// root joined with [`KizzFile::relative`].
    pub path: PathBuf,
    /// The path relative to the discovery root.
    pub relative: PathBuf,
}

/// What discovery found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// The `kizz.md` files, each once, in the default order of [`default_order`].
    pub files: Vec<KizzFile>,
    /// The warnings, sorted, so their order is deterministic too.
    pub warnings: Vec<Warning>,
}

/// One result that a walker thread reports.
enum Found {
    /// A `kizz.md`, by the path the walk reached it through.
    File(PathBuf),
    /// A problem that does not stop discovery.
    Warning(Warning),
}

/// Find every `kizz.md` under `root`, which should be an absolute directory path.
///
/// Problems with single entries, such as unreadable directories or recursive
/// symbolic links, become warnings; discovery itself never fails.
#[must_use]
pub fn discover(root: &Path) -> Discovery {
    let (sender, receiver) = mpsc::channel::<Found>();
    WalkBuilder::new(root)
        .standard_filters(false)
        .follow_links(true)
        .build_parallel()
        .run(|| {
            let sender = sender.clone();
            Box::new(move |result| {
                let found = match result {
                    Ok(entry) => {
                        let is_kizz_file = entry.file_name() == OsStr::new(KIZZ_FILE_NAME)
                            && entry.file_type().is_some_and(|kind| kind.is_file());
                        if !is_kizz_file {
                            return WalkState::Continue;
                        }
                        Found::File(entry.into_path())
                    },
                    Err(error) => {
                        match walk_warning(root, &error) {
                            Some(warning) => Found::Warning(warning),
                            None => return WalkState::Continue,
                        }
                    },
                };
                if sender.send(found).is_err() {
                    return WalkState::Quit;
                }
                WalkState::Continue
            })
        });
    drop(sender);

    let mut found: Vec<KizzFile> = Vec::new();
    let mut warnings: Vec<Warning> = Vec::new();
    for item in receiver {
        match item {
            Found::File(path) => {
                found.push(KizzFile {
                    relative: relative_to(root, &path),
                    path,
                });
            },
            Found::Warning(warning) => warnings.push(warning),
        }
    }
    found.sort_by(|a, b| default_order(&a.relative, &b.relative));

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut files: Vec<KizzFile> = Vec::new();
    for file in found {
        match fs::canonicalize(&file.path) {
            Ok(real) => {
                if seen.insert(real) {
                    files.push(file);
                }
            },
            Err(error) => {
                warnings.push(Warning::Unreadable {
                    path: file.relative,
                    reason: error.to_string(),
                });
            },
        }
    }
    warnings.sort();
    warnings.dedup();
    Discovery { files, warnings }
}

/// The default order of `kizz.md` paths relative to the discovery root.
///
/// A directory's `kizz.md` comes before every `kizz.md` in its subdirectories, and
/// sibling directories come in byte order of their names. Plain path sorting would
/// put `a/b/kizz.md` before `a/kizz.md` instead.
#[must_use]
pub fn default_order(
    a: &Path,
    b: &Path,
) -> Ordering {
    /// The directory components of `path`, as bytes, and its file name.
    fn key(path: &Path) -> (Vec<&[u8]>, &[u8]) {
        let directories = path
            .parent()
            .map(|parent| {
                parent
                    .components()
                    .filter(|component| matches!(component, Component::Normal(_)))
                    .map(|component| component.as_os_str().as_encoded_bytes())
                    .collect()
            })
            .unwrap_or_default();
        let name = path
            .file_name()
            .map(OsStr::as_encoded_bytes)
            .unwrap_or_default();
        (directories, name)
    }
    key(a).cmp(&key(b))
}

/// `path` relative to `root`, or `path` itself when it is not under `root`.
fn relative_to(
    root: &Path,
    path: &Path,
) -> PathBuf {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
}

/// The warning for an error of the walk, or `None` for an error that hides no
/// `kizz.md`: a path that vanished, such as a dangling symbolic link, whose name is
/// not `kizz.md`.
fn walk_warning(
    root: &Path,
    error: &ignore::Error,
) -> Option<Warning> {
    if let Some((ancestor, child)) = find_loop(error) {
        return Some(Warning::RecursiveLink {
            link: relative_to(root, child),
            ancestor: relative_to(root, ancestor),
        });
    }
    let (path, cause) = unwrap_context(error);
    let path = path.map_or_else(PathBuf::new, |path| relative_to(root, path));
    let vanished = cause
        .io_error()
        .is_some_and(|io| io.kind() == io::ErrorKind::NotFound);
    if vanished && path.file_name() != Some(OsStr::new(KIZZ_FILE_NAME)) {
        return None;
    }
    Some(Warning::Unreadable {
        path,
        reason: cause.to_string(),
    })
}

/// The ancestor and child paths of a symbolic-link loop that `error` reports, if it
/// reports one.
fn find_loop(error: &ignore::Error) -> Option<(&Path, &Path)> {
    match error {
        ignore::Error::Loop { ancestor, child } => Some((ancestor.as_path(), child.as_path())),
        ignore::Error::WithDepth { err, .. }
        | ignore::Error::WithLineNumber { err, .. }
        | ignore::Error::WithPath { err, .. } => find_loop(err),
        ignore::Error::Partial(errors) => errors.iter().find_map(find_loop),
        _ => None,
    }
}

/// The innermost path that `error` names, if any, and the error without its context.
fn unwrap_context(error: &ignore::Error) -> (Option<&Path>, &ignore::Error) {
    match error {
        ignore::Error::WithPath { path, err } => {
            let (inner, cause) = unwrap_context(err);
            (inner.or(Some(path.as_path())), cause)
        },
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            unwrap_context(err)
        },
        _ => (None, error),
    }
}

/// Unit tests for discovery and its order.
#[cfg(test)]
mod tests {
    use std::{cmp::Ordering, fs, path::Path};

    use super::{default_order, discover};

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

    /// The relative paths of the files that discovery finds under `root`.
    fn found(root: &Path) -> Vec<String> {
        let discovery = discover(root);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        discovery
            .files
            .iter()
            .map(|file| {
                assert_eq!(file.path, root.join(&file.relative));
                file.relative.display().to_string()
            })
            .collect()
    }

    /// A parent directory's `kizz.md` comes before its subdirectories', and sibling
    /// directories sort by name in byte order.
    #[test]
    fn default_order_puts_parents_before_subdirectories() {
        let mut paths = vec![
            "b/kizz.md",
            "a/b/kizz.md",
            "a/kizz.md",
            "kizz.md",
            "a-b/kizz.md",
            "B/kizz.md",
            "a/a/z/kizz.md",
            "a/c/kizz.md",
            ".hidden/kizz.md",
        ];
        paths.sort_by(|a, b| default_order(Path::new(a), Path::new(b)));
        assert_eq!(paths, [
            "kizz.md",
            ".hidden/kizz.md",
            "B/kizz.md",
            "a/kizz.md",
            "a/a/z/kizz.md",
            "a/b/kizz.md",
            "a/c/kizz.md",
            "a-b/kizz.md",
            "b/kizz.md",
        ]);
        assert_eq!(
            default_order(Path::new("a/kizz.md"), Path::new("a/kizz.md")),
            Ordering::Equal
        );
    }

    /// Discovery finds every file named exactly `kizz.md`, in hidden directories
    /// too, never reads other Markdown, and ignores no file that Git or ignore files
    /// exclude.
    #[test]
    fn finds_every_kizz_md_in_order() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, ".git/HEAD", "ref: refs/heads/main\n");
        write(root, ".gitignore", "ignored/\nkizz.md\n*.md\n");
        write(root, ".ignore", "ignored/\nkizz.md\n");
        write(root, ".rgignore", "*\n");
        write(root, "kizz.md", "");
        write(root, "README.md", "");
        write(root, "a/kizz.md", "");
        write(root, "a/b/kizz.md", "");
        write(root, "a/KIZZ.md", "");
        write(root, "a/kizz.md.bak", "");
        write(root, "a/other/notes.md", "");
        write(root, ".hidden/kizz.md", "");
        write(root, ".hidden/.deeper/kizz.md", "");
        write(root, "ignored/kizz.md", "");
        write(root, "b/kizz.md", "");
        fs::create_dir_all(root.join("c/kizz.md")).unwrap();
        write(root, "c/kizz.md/kizz.md", "");

        let expected = [
            "kizz.md",
            ".hidden/kizz.md",
            ".hidden/.deeper/kizz.md",
            "a/kizz.md",
            "a/b/kizz.md",
            "b/kizz.md",
            "c/kizz.md/kizz.md",
            "ignored/kizz.md",
        ];
        assert_eq!(found(root), expected);
        // The parallel walk always gives the same result.
        for _ in 0..10 {
            assert_eq!(found(root), expected);
        }
    }

    /// Discovery never looks above its root.
    #[test]
    fn never_looks_above_the_root() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "kizz.md", "");
        write(root.path(), "sub/kizz.md", "");
        write(root.path(), "sub/deeper/kizz.md", "");
        assert_eq!(found(&root.path().join("sub")), [
            "kizz.md",
            "deeper/kizz.md"
        ]);
    }

    /// An empty directory has no `kizz.md` files and no warnings.
    #[test]
    fn an_empty_directory_finds_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(found(root.path()), Vec::<String>::new());
    }
}
