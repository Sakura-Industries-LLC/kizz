// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Program lookup through an environment's `PATH`.
//!
//! A program name without `/` is looked up in the `PATH` of the environment that
//! the program gets: for a step, the task environment, never kizz's own; for
//! `kizz execute`, the caller's. Each directory of `PATH` is tried in order, as
//! `execvp` does: an empty entry means the working directory, and a relative entry
//! resolves against it. Every location checked is reported as a probe, so it
//! becomes a dependency of the step and a program that appears earlier in `PATH`
//! re-runs the step. The first location that holds a regular file with an execute
//! bit is the program.
//!
//! A program name with `/` is not looked up: it resolves against the working
//! directory, and the tracer reports whether it can be run.
//!
//! [`search_program`] is the lookup itself, for any name and `PATH` of bytes, and
//! reports each checked location with its outcome: `ok` when something is there,
//! and otherwise the name of the error, which it gets from the caller, such as the
//! tracer's error-name query ([`kizz_trace::Tracer::error_name`]).
//! [`find_program`] is the lookup of `run()`, for a task environment of strings,
//! with the checked locations as accesses.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
};

use kizz_trace::{AbsolutePath, AccessKind, Accesses, Observations, Outcome};

/// The variable that lists the directories to search for programs.
pub const PATH_VARIABLE: &str = "PATH";

/// The permission bits that make a file executable by someone.
const EXECUTE_BITS: u32 = 0o111;

/// The result of looking up a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramLookup {
    /// The program was found.
    Found {
        /// The absolute path of the program.
        program: AbsolutePath,
        /// Every location checked, the program's included, as probes.
        checked: Accesses,
    },
    /// No location holds the program.
    NotFound {
        /// Every location checked, as probes.
        checked: Accesses,
    },
    /// A location holds the program, but it is not an executable regular file, and
    /// no later location holds an executable one.
    NotExecutable {
        /// The first location that holds the program.
        candidate: AbsolutePath,
        /// Every location checked, as probes.
        checked: Accesses,
    },
}

impl ProgramLookup {
    /// Every location checked, as probes.
    #[must_use]
    pub const fn checked(&self) -> &Accesses {
        match self {
            Self::Found { checked, .. }
            | Self::NotFound { checked }
            | Self::NotExecutable { checked, .. } => checked,
        }
    }
}

/// The result of [`search_program`]: the result of a lookup, with every location
/// that it checked as a probe with its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramSearch {
    /// The program was found.
    Found {
        /// The absolute path of the program.
        program: AbsolutePath,
        /// Every location checked, the program's included, as probes.
        checked: Observations,
    },
    /// No location holds the program.
    NotFound {
        /// Every location checked, as probes.
        checked: Observations,
    },
    /// A location holds the program, but it is not an executable regular file, and
    /// no later location holds an executable one.
    NotExecutable {
        /// The first location that holds the program.
        candidate: AbsolutePath,
        /// Every location checked, as probes.
        checked: Observations,
    },
}

impl ProgramSearch {
    /// Every location checked, as probes with their outcomes.
    #[must_use]
    pub const fn checked(&self) -> &Observations {
        match self {
            Self::Found { checked, .. }
            | Self::NotFound { checked }
            | Self::NotExecutable { checked, .. } => checked,
        }
    }

    /// The same result as a [`ProgramLookup`], whose checked locations are
    /// accesses without outcomes.
    #[must_use]
    pub fn into_lookup(self) -> ProgramLookup {
        match self {
            Self::Found { program, checked } => {
                ProgramLookup::Found {
                    program,
                    checked: checked.accesses(),
                }
            },
            Self::NotFound { checked } => {
                ProgramLookup::NotFound {
                    checked: checked.accesses(),
                }
            },
            Self::NotExecutable { candidate, checked } => {
                ProgramLookup::NotExecutable {
                    candidate,
                    checked: checked.accesses(),
                }
            },
        }
    }
}

/// A bare program name in a task environment without `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "cannot run `{program}`: a program name without `/` is looked up in the task \
     environment's PATH, but the task environment has no PATH; set it with \
     env.set(\"PATH\", ...) or inherit it with env.inherit(\"PATH\")"
)]
pub struct NoPathError {
    /// The program name.
    pub program: String,
}

/// Look up the program `name` for a step whose environment is `env` and whose
/// working directory is `cwd`, with [`search_program`].
///
/// # Errors
///
/// Returns [`NoPathError`] when `name` has no `/` and `env` has no `PATH`.
pub fn find_program(
    name: &str,
    env: &BTreeMap<String, String>,
    cwd: &AbsolutePath,
) -> Result<ProgramLookup, NoPathError> {
    let search = env.get(PATH_VARIABLE).map(OsStr::new);
    search_program(OsStr::new(name), search, cwd, kizz_trace::unnamed_error)
        .map(ProgramSearch::into_lookup)
        .ok_or_else(|| {
            NoPathError {
                program: name.to_owned(),
            }
        })
}

/// Look up the program `name` in `search`, the value of `PATH`, or `None` when the
/// environment has no `PATH`, for a program whose working directory is `cwd`.
/// Each checked location is a probe whose outcome is `ok` when something is there,
/// and otherwise the name that `error_name` gives the error number of the failed
/// check, or `unknown` when the failure has no number.
///
/// A name with `/` is found at once, resolved against `cwd`, with nothing checked.
/// An empty name is not found, with nothing checked.
///
/// Returns `None` when `name` has no `/` and `search` is `None`, since such a name
/// cannot be looked up.
pub fn search_program(
    name: &OsStr,
    search: Option<&OsStr>,
    cwd: &AbsolutePath,
    error_name: impl Fn(i32) -> String,
) -> Option<ProgramSearch> {
    if name.as_bytes().contains(&b'/') {
        return Some(ProgramSearch::Found {
            program: cwd.join(name),
            checked: Observations::new(),
        });
    }
    let search = search?;
    let mut checked = Observations::new();
    if name.is_empty() {
        return Some(ProgramSearch::NotFound { checked });
    }
    let mut not_executable = None;
    for entry in search.as_bytes().split(|byte| *byte == b':') {
        let candidate = cwd.join(OsStr::from_bytes(entry)).join(name);
        let metadata = match fs::metadata(candidate.as_path()) {
            Ok(metadata) => metadata,
            Err(error) => {
                let outcome = error
                    .raw_os_error()
                    .map_or(Outcome::Unknown, |errno| Outcome::Error(error_name(errno)));
                checked.record(candidate, AccessKind::Probe, outcome);
                continue;
            },
        };
        checked.record(candidate.clone(), AccessKind::Probe, Outcome::Ok);
        if metadata.is_file() && metadata.permissions().mode() & EXECUTE_BITS != 0 {
            return Some(ProgramSearch::Found {
                program: candidate,
                checked,
            });
        }
        not_executable.get_or_insert(candidate);
    }
    Some(match not_executable {
        Some(candidate) => ProgramSearch::NotExecutable { candidate, checked },
        None => ProgramSearch::NotFound { checked },
    })
}

/// Unit tests of the lookup.
#[cfg(test)]
#[allow(clippy::disallowed_macros, reason = "test code: the tests assert")]
mod tests {
    use std::{
        collections::BTreeMap, ffi::OsStr, fs, os::unix::fs::PermissionsExt as _, path::Path,
    };

    use kizz_trace::{AbsolutePath, AccessKind, Observations, Outcome};

    use super::{ProgramLookup, ProgramSearch, find_program, search_program};

    /// The name that the tests give each error number.
    fn named(errno: i32) -> String {
        format!("E{errno}")
    }

    /// A temporary directory with `bin/tool`, executable, and `text/tool`, not
    /// executable, and the directory as an absolute path.
    fn fixture() -> Result<(tempfile::TempDir, AbsolutePath), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let root = AbsolutePath::new(dir.path().canonicalize()?)?;
        for (sub, mode) in [("bin", 0o755), ("text", 0o644)] {
            fs::create_dir(root.as_path().join(sub))?;
            let tool = root.as_path().join(sub).join("tool");
            fs::write(&tool, "#!/bin/sh\n")?;
            fs::set_permissions(&tool, fs::Permissions::from_mode(mode))?;
        }
        Ok((dir, root))
    }

    /// The probe of `path` with the outcome `outcome`, added to `observations`.
    fn probe(
        observations: &mut Observations,
        path: &Path,
        outcome: Outcome,
    ) {
        observations.record(AbsolutePath::new(path).unwrap(), AccessKind::Probe, outcome);
    }

    /// The search reports each checked location with its outcome: the error's name
    /// for a location where nothing is, and `ok` for one where something is, the
    /// found program's included; `find_program` gives the same locations as
    /// accesses.
    #[test]
    fn each_checked_location_has_its_outcome() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, root) = fixture()?;
        let missing = root.as_path().join("missing");
        let bin = root.as_path().join("bin");
        let search = format!("{}:{}", missing.display(), bin.display());
        let found = search_program(OsStr::new("tool"), Some(OsStr::new(&search)), &root, named);
        let mut checked = Observations::new();
        probe(
            &mut checked,
            &missing.join("tool"),
            Outcome::Error(named(2)),
        );
        probe(&mut checked, &bin.join("tool"), Outcome::Ok);
        let program = AbsolutePath::new(bin.join("tool"))?;
        assert_eq!(
            found,
            Some(ProgramSearch::Found {
                program: program.clone(),
                checked: checked.clone(),
            })
        );
        let env = BTreeMap::from([("PATH".to_owned(), search)]);
        assert_eq!(find_program("tool", &env, &root)?, ProgramLookup::Found {
            program,
            checked: checked.accesses(),
        });
        Ok(())
    }

    /// A location that holds a file without execute permission is checked with `ok`
    /// and gives a program that is not executable; a name that no location holds is
    /// not found; an empty name and a name with `/` check nothing; and a name
    /// without `/` cannot be looked up without `PATH`.
    #[test]
    fn other_results_of_the_search() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, root) = fixture()?;
        let text = root.as_path().join("text");
        let search = text.display().to_string();
        let mut checked = Observations::new();
        probe(&mut checked, &text.join("tool"), Outcome::Ok);
        assert_eq!(
            search_program(OsStr::new("tool"), Some(OsStr::new(&search)), &root, named),
            Some(ProgramSearch::NotExecutable {
                candidate: AbsolutePath::new(text.join("tool"))?,
                checked,
            })
        );
        let mut absent = Observations::new();
        probe(&mut absent, &text.join("other"), Outcome::Error(named(2)));
        assert_eq!(
            search_program(OsStr::new("other"), Some(OsStr::new(&search)), &root, named),
            Some(ProgramSearch::NotFound { checked: absent })
        );
        assert_eq!(
            search_program(OsStr::new(""), Some(OsStr::new(&search)), &root, named),
            Some(ProgramSearch::NotFound {
                checked: Observations::new(),
            })
        );
        assert_eq!(
            search_program(OsStr::new("./tool"), None, &root, named),
            Some(ProgramSearch::Found {
                program: root.join("tool"),
                checked: Observations::new(),
            })
        );
        assert_eq!(search_program(OsStr::new("tool"), None, &root, named), None);
        Ok(())
    }
}
