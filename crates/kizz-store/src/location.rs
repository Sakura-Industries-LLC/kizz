// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The location of the cache directory.
//!
//! The cache is `KIZZ_CACHE_DIR` when set, otherwise `$XDG_CACHE_HOME/kizz`,
//! otherwise `~/.cache/kizz`. [`cache_dir`] computes it from the three values, so
//! that callers and tests never need to change the process environment;
//! [`cache_dir_from_env`] reads them from the environment of the process.
//!
//! The rules for unusual values:
//!
//! * An empty `KIZZ_CACHE_DIR` counts as unset, as an empty `XDG_CACHE_HOME` does.
//! * A relative `KIZZ_CACHE_DIR` is an error: the cache serves every build on the machine
//!   wherever it starts, and a relative path would name a different cache in every
//!   directory.
//! * An empty or relative `XDG_CACHE_HOME` is ignored, as the XDG Base Directory
//!   Specification requires.
//! * When `HOME` is needed and is unset, empty, or relative, there is no cache location,
//!   and the error says which variables can provide one.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

/// The variable that names the cache directory directly.
pub const KIZZ_CACHE_DIR: &str = "KIZZ_CACHE_DIR";
/// The XDG Base Directory variable for user cache files.
pub const XDG_CACHE_HOME: &str = "XDG_CACHE_HOME";
/// The variable that names the home directory of the user.
pub const HOME: &str = "HOME";

/// Why the cache directory cannot be located.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocationError {
    /// `KIZZ_CACHE_DIR` is set to a relative path.
    #[error(
        "KIZZ_CACHE_DIR must be an absolute path, but it is `{}`",
        .0.display()
    )]
    RelativeCacheDir(PathBuf),
    /// Neither `KIZZ_CACHE_DIR` nor a usable `XDG_CACHE_HOME` is set, and `HOME` is
    /// unset, empty, or relative.
    #[error(
        "cannot locate the kizz cache: set KIZZ_CACHE_DIR or XDG_CACHE_HOME to an \
         absolute path, or set HOME to an absolute path"
    )]
    NoHome,
}

/// The cache directory from the values of `KIZZ_CACHE_DIR`, `XDG_CACHE_HOME`, and
/// `HOME`, each `None` when the variable is unset.
///
/// # Errors
///
/// [`LocationError::RelativeCacheDir`] when `KIZZ_CACHE_DIR` is relative, and
/// [`LocationError::NoHome`] when `HOME` is needed but unusable.
pub fn cache_dir(
    kizz_cache_dir: Option<&OsStr>,
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, LocationError> {
    if let Some(value) = non_empty(kizz_cache_dir) {
        let path = Path::new(value);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        return Err(LocationError::RelativeCacheDir(path.to_path_buf()));
    }
    if let Some(value) = non_empty(xdg_cache_home) {
        let path = Path::new(value);
        if path.is_absolute() {
            return Ok(path.join("kizz"));
        }
    }
    if let Some(value) = non_empty(home) {
        let path = Path::new(value);
        if path.is_absolute() {
            return Ok(path.join(".cache").join("kizz"));
        }
    }
    Err(LocationError::NoHome)
}

/// The cache directory from the environment of this process.
///
/// # Errors
///
/// As for [`cache_dir`].
pub fn cache_dir_from_env() -> Result<PathBuf, LocationError> {
    let kizz_cache_dir: Option<OsString> = std::env::var_os(KIZZ_CACHE_DIR);
    let xdg_cache_home: Option<OsString> = std::env::var_os(XDG_CACHE_HOME);
    let home: Option<OsString> = std::env::var_os(HOME);
    cache_dir(
        kizz_cache_dir.as_deref(),
        xdg_cache_home.as_deref(),
        home.as_deref(),
    )
}

/// `value` unless it is unset or empty.
fn non_empty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|value| !value.is_empty())
}

/// Unit tests for the location rules.
#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for the value of a variable that is set.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the tests pass the result where `cache_dir` takes an optional value"
    )]
    fn var(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[test]
    fn kizz_cache_dir_wins_over_xdg_cache_home_and_home() {
        assert_eq!(
            cache_dir(var("/kizz"), var("/xdg"), var("/home/user")),
            Ok(PathBuf::from("/kizz"))
        );
    }

    #[test]
    fn xdg_cache_home_wins_over_home() {
        assert_eq!(
            cache_dir(None, var("/xdg"), var("/home/user")),
            Ok(PathBuf::from("/xdg/kizz"))
        );
    }

    #[test]
    fn home_is_the_last_resort() {
        assert_eq!(
            cache_dir(None, None, var("/home/user")),
            Ok(PathBuf::from("/home/user/.cache/kizz"))
        );
    }

    #[test]
    fn empty_kizz_cache_dir_counts_as_unset() {
        assert_eq!(
            cache_dir(var(""), var("/xdg"), var("/home/user")),
            Ok(PathBuf::from("/xdg/kizz"))
        );
        assert_eq!(
            cache_dir(var(""), None, var("/home/user")),
            Ok(PathBuf::from("/home/user/.cache/kizz"))
        );
    }

    #[test]
    fn relative_kizz_cache_dir_is_an_error() {
        let error = cache_dir(var("cache"), var("/xdg"), var("/home/user")).unwrap_err();
        assert_eq!(
            error,
            LocationError::RelativeCacheDir(PathBuf::from("cache"))
        );
        assert!(error.to_string().contains("KIZZ_CACHE_DIR"));
        assert!(error.to_string().contains("`cache`"));
    }

    #[test]
    fn empty_or_relative_xdg_cache_home_is_ignored() {
        assert_eq!(
            cache_dir(None, var(""), var("/home/user")),
            Ok(PathBuf::from("/home/user/.cache/kizz"))
        );
        assert_eq!(
            cache_dir(None, var("relative/cache"), var("/home/user")),
            Ok(PathBuf::from("/home/user/.cache/kizz"))
        );
    }

    #[test]
    fn missing_empty_or_relative_home_is_an_error_when_needed() {
        assert_eq!(cache_dir(None, None, None), Err(LocationError::NoHome));
        assert_eq!(cache_dir(None, None, var("")), Err(LocationError::NoHome));
        assert_eq!(
            cache_dir(None, var("relative"), var("home")),
            Err(LocationError::NoHome)
        );
        let message = LocationError::NoHome.to_string();
        assert!(message.contains("KIZZ_CACHE_DIR"));
        assert!(message.contains("XDG_CACHE_HOME"));
        assert!(message.contains("HOME"));
    }

    #[test]
    fn home_is_not_needed_when_another_variable_gives_the_location() {
        assert_eq!(
            cache_dir(var("/kizz"), None, None),
            Ok(PathBuf::from("/kizz"))
        );
        assert_eq!(
            cache_dir(None, var("/xdg"), None),
            Ok(PathBuf::from("/xdg/kizz"))
        );
    }
}
