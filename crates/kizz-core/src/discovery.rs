// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Discovery of `kizz.md` files.
//!
//! Discovery starts in one directory and never looks above it. It finds every file
//! named exactly `kizz.md` in that directory and every subdirectory, hidden
//! directories included, with a concurrent walk of its own on the command's worker
//! pool ([`WorkerPool`]), which consults no Git or ignore-file rule, so none hides a
//! `kizz.md`. The walk reads the directories of each depth in parallel, then those of
//! the next depth; it starts no thread of its own.
//!
//! Symbolic links to directories are followed, except a link that would make the walk
//! recurse into one of its own parent directories, which is skipped with a warning:
//! a link to a directory with the device and inode of the directory that holds it or
//! of any directory on the walk's path to it. A path that cannot be read is a warning,
//! with the system's error, except one that vanished during the walk, such as a
//! dangling symbolic link, whose name is not `kizz.md`. A `kizz.md` reached through
//! several paths is kept once, under the first of them in the default order of
//! [`default_order`]. The files and warnings are sorted, so the order in which the
//! walk finds them changes nothing.

use std::{
    cmp::Ordering,
    collections::HashSet,
    ffi::OsStr,
    fs, io, mem,
    os::unix::fs::MetadataExt as _,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use rayon::iter::{IntoParallelIterator as _, ParallelIterator as _};

use crate::{pool::WorkerPool, warning::Warning};

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

/// One result that reading a directory reports.
enum Found {
    /// A `kizz.md`, by the path the walk reached it through.
    File(PathBuf),
    /// A problem that does not stop discovery.
    Warning(Warning),
}

/// A directory on the walk's path.
struct Ancestor {
    /// The device and inode of the directory.
    id: (u64, u64),
    /// The path through which the walk reached it.
    path: PathBuf,
    /// The directory that the walk reached it from, or `None` for the root.
    parent: Option<Arc<Ancestor>>,
}

impl Ancestor {
    /// The directory with the device and inode `id` on the walk's path to this one,
    /// this one included.
    fn find(
        &self,
        id: (u64, u64),
    ) -> Option<&Self> {
        let mut current = Some(self);
        while let Some(ancestor) = current {
            if ancestor.id == id {
                return Some(ancestor);
            }
            current = ancestor.parent.as_deref();
        }
        None
    }
}

/// A directory that the walk still has to read, with the walk's path to it.
struct Pending {
    /// The directory, which is the last directory of the path.
    dir: Arc<Ancestor>,
}

/// What reading one directory found.
#[derive(Default)]
struct Read {
    /// The files and warnings.
    found: Vec<Found>,
    /// The subdirectories to read next.
    next: Vec<Pending>,
}

impl Read {
    /// Note the warning for `error`, which reading `path` met, if it hides a
    /// `kizz.md`.
    fn warn(
        &mut self,
        root: &Path,
        path: &Path,
        error: &io::Error,
    ) {
        if let Some(warning) = walk_warning(root, path, error) {
            self.found.push(Found::Warning(warning));
        }
    }
}

/// The device and inode of the file whose metadata is `metadata`.
fn id_of(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

/// Find every `kizz.md` under `root`, which should be an absolute directory path,
/// with a concurrent walk on `pool`.
///
/// Problems with single entries, such as unreadable directories or recursive
/// symbolic links, become warnings; discovery itself never fails.
#[must_use]
pub fn discover(
    root: &Path,
    pool: &WorkerPool,
) -> Discovery {
    let mut items: Vec<Found> = Vec::new();
    match fs::metadata(root) {
        Ok(metadata) => {
            let mut level = vec![Pending {
                dir: Arc::new(Ancestor {
                    id: id_of(&metadata),
                    path: root.to_path_buf(),
                    parent: None,
                }),
            }];
            while !level.is_empty() {
                let reading = mem::take(&mut level);
                let reads: Vec<Read> = pool.install(move || {
                    reading
                        .into_par_iter()
                        .map(|pending| read(root, &pending))
                        .collect()
                });
                for read in reads {
                    items.extend(read.found);
                    level.extend(read.next);
                }
            }
        },
        Err(error) => {
            if let Some(warning) = walk_warning(root, root, &error) {
                items.push(Found::Warning(warning));
            }
        },
    }

    let mut found: Vec<KizzFile> = Vec::new();
    let mut warnings: Vec<Warning> = Vec::new();
    for item in items {
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

/// Read the directory of `pending`: note each `kizz.md` in it, each warning, and
/// each subdirectory to read next, following symbolic links to directories except
/// those that lead back to a directory on the walk's path.
fn read(
    root: &Path,
    pending: &Pending,
) -> Read {
    let mut read = Read::default();
    let dir = &pending.dir;
    let entries = match fs::read_dir(&dir.path) {
        Ok(entries) => entries,
        Err(error) => {
            read.warn(root, &dir.path, &error);
            return read;
        },
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                read.warn(root, &dir.path, &error);
                continue;
            },
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                read.warn(root, &path, &error);
                continue;
            },
        };
        let link = file_type.is_symlink();
        // A symbolic link is followed: what it leads to decides.
        let metadata = if link || file_type.is_dir() {
            let metadata = if link {
                fs::metadata(&path)
            } else {
                entry.metadata()
            };
            match metadata {
                Ok(metadata) => Some(metadata),
                Err(error) => {
                    read.warn(root, &path, &error);
                    continue;
                },
            }
        } else {
            None
        };
        let is_file = metadata
            .as_ref()
            .map_or(file_type.is_file(), fs::Metadata::is_file);
        if is_file {
            if entry.file_name() == OsStr::new(KIZZ_FILE_NAME) {
                read.found.push(Found::File(path));
            }
            continue;
        }
        let Some(metadata) = metadata.filter(fs::Metadata::is_dir) else {
            continue;
        };
        let id = id_of(&metadata);
        if link && let Some(ancestor) = dir.find(id) {
            read.found.push(Found::Warning(Warning::RecursiveLink {
                link: relative_to(root, &path),
                ancestor: relative_to(root, &ancestor.path),
            }));
            continue;
        }
        read.next.push(Pending {
            dir: Arc::new(Ancestor {
                id,
                path,
                parent: Some(Arc::clone(dir)),
            }),
        });
    }
    read
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
        let directories = path.parent().map_or_default(|parent| {
            parent
                .components()
                .filter(|component| matches!(component, Component::Normal(_)))
                .map(|component| component.as_os_str().as_encoded_bytes())
                .collect()
        });
        let name = path.file_name().map_or_default(OsStr::as_encoded_bytes);
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

/// The warning for `error`, which reading `path` met during the walk, or `None` for
/// an error that hides no `kizz.md`: a path that vanished, such as a dangling
/// symbolic link, whose name is not `kizz.md`.
fn walk_warning(
    root: &Path,
    path: &Path,
    error: &io::Error,
) -> Option<Warning> {
    let path = relative_to(root, path);
    let vanished = error.kind() == io::ErrorKind::NotFound;
    if vanished && path.file_name() != Some(OsStr::new(KIZZ_FILE_NAME)) {
        return None;
    }
    Some(Warning::Unreadable {
        path,
        reason: error.to_string(),
    })
}

/// Unit tests for discovery and its order.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::{
        cmp::Ordering,
        fs, io,
        os::unix::fs::symlink,
        path::{Path, PathBuf},
    };

    use super::{default_order, discover};
    use crate::{pool::WorkerPool, warning::Warning};

    /// A worker pool for one discovery.
    fn pool() -> WorkerPool {
        WorkerPool::new().unwrap()
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

    /// The relative paths of the files that discovery finds under `root`.
    fn found(root: &Path) -> Vec<String> {
        let discovery = discover(root, &pool());
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

    /// The length at which Linux refuses to resolve a path: `PATH_MAX`, 4,096 bytes
    /// with the final NUL byte.
    const PATH_MAX: usize = 4_096;

    /// Make under `root/deep` a chain of directories with long names, deeper than a
    /// path can name, and return the path, relative to `root`, of the first one that
    /// discovery cannot read: the first whose absolute path is [`PATH_MAX`] bytes or
    /// longer, which even a process that may read every directory cannot read.
    fn too_deep(root: &Path) -> PathBuf {
        let long = "x".repeat(200);
        let levels = 25;
        // Short names first, then each is renamed, the deepest first, so no call
        // names a path that is too long.
        let mut short = root.join("deep");
        for _ in 0..levels {
            short.push("d");
        }
        fs::create_dir_all(&short).unwrap();
        for depth in (1..=levels).rev() {
            let mut parent = root.join("deep");
            for _ in 1..depth {
                parent.push("d");
            }
            fs::rename(parent.join("d"), parent.join(&long)).unwrap();
        }
        let mut relative = PathBuf::from("deep");
        loop {
            relative.push(&long);
            if root.join(&relative).as_os_str().len() >= PATH_MAX {
                return relative;
            }
        }
    }

    /// A tree with a hidden directory, a link to a parent, a link to a sibling and an
    /// unreadable directory gives exactly the files, in order, and the warnings that
    /// "Discovery" requires: the link to the parent is skipped with a warning, the
    /// `kizz.md` reached through the sibling link is kept once, under its first path
    /// in the default order, and the unreadable directory is a warning with the
    /// system's error.
    #[test]
    fn hidden_directories_links_and_unreadable_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "kizz.md", "");
        write(root, ".hidden/kizz.md", "");
        write(root, "a/kizz.md", "");
        write(root, "b/kizz.md", "");
        write(root, "deep/kizz.md", "");
        symlink("..", root.join("a/up")).unwrap();
        symlink("../b", root.join("a/sibling")).unwrap();
        let unreadable = too_deep(root);

        let discovery = discover(root, &pool());
        let files: Vec<PathBuf> = discovery
            .files
            .iter()
            .map(|file| file.relative.clone())
            .collect();
        assert_eq!(
            files,
            [
                "kizz.md",
                ".hidden/kizz.md",
                "a/kizz.md",
                "a/sibling/kizz.md",
                "deep/kizz.md"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(discovery.warnings, [
            Warning::RecursiveLink {
                link: PathBuf::from("a/up"),
                ancestor: PathBuf::new(),
            },
            Warning::Unreadable {
                path: unreadable,
                reason: io::Error::from_raw_os_error(36).to_string(),
            },
        ]);
    }
}
