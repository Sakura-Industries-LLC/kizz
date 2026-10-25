// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Program lookup through the task environment's `PATH`.
//!
//! A program name without `/` is looked up in the `PATH` of the task environment,
//! never in kizz's own. Each directory of `PATH` is tried in order, as `execvp`
//! does: an empty entry means the working directory, and a relative entry resolves
//! against it. Every location checked is reported as a probe, so it becomes a
//! dependency of the step and a program that appears earlier in `PATH` re-runs the
//! step. The first location that holds a regular file with an execute bit is the
//! program.
//!
//! A program name with `/` is not looked up: it resolves against the working
//! directory, and the tracer reports whether it can be run.

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _};

use kizz_trace::{AbsolutePath, AccessKind, Accesses};

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
/// working directory is `cwd`.
///
/// # Errors
///
/// Returns [`NoPathError`] when `name` has no `/` and `env` has no `PATH`.
pub fn find_program(
    name: &str,
    env: &BTreeMap<String, String>,
    cwd: &AbsolutePath,
) -> Result<ProgramLookup, NoPathError> {
    if name.contains('/') {
        return Ok(ProgramLookup::Found {
            program: cwd.join(name),
            checked: Accesses::new(),
        });
    }
    let Some(search) = env.get(PATH_VARIABLE) else {
        return Err(NoPathError {
            program: name.to_owned(),
        });
    };
    let mut checked = Accesses::new();
    if name.is_empty() {
        return Ok(ProgramLookup::NotFound { checked });
    }
    let mut not_executable = None;
    for entry in search.split(':') {
        let candidate = cwd.join(entry).join(name);
        checked.record(candidate.clone(), AccessKind::Probe);
        let Ok(metadata) = fs::metadata(candidate.as_path()) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & EXECUTE_BITS != 0 {
            return Ok(ProgramLookup::Found {
                program: candidate,
                checked,
            });
        }
        not_executable.get_or_insert(candidate);
    }
    Ok(match not_executable {
        Some(candidate) => ProgramLookup::NotExecutable { candidate, checked },
        None => ProgramLookup::NotFound { checked },
    })
}
