// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache lock file, `kizz.lock` in the cache directory.
//!
//! A build holds an exclusive `flock` on the file for its whole run, through the
//! locking of [`std::fs::File`], so only one process uses a cache at a time.
//!
//! * The holder writes its [`LockDetails`] into the file.
//! * A waiting process reports the holder's details at once, and again whenever a
//!   different process takes the lock while it waits.
//! * The kernel releases the lock when the holder exits for any reason, so a dead holder
//!   never blocks another process, and the next process takes over a file that a crash
//!   left behind.
//! * After taking the lock, a process checks that the file it locked is still the file at
//!   the path, and retries if not. This closes the race between one process deleting the
//!   file and another locking it.
//! * Releasing the lock, explicitly or by dropping the [`CacheLock`], deletes the file
//!   while the lock is still held, so a normal exit, or an interrupted exit that unwinds
//!   to the release, leaves no file behind.
//!
//! The holder's start time, which messages show in UTC, is converted with checked
//! arithmetic, so no time read from another process's file can make an operator
//! overflow and panic ("Kizz never panics").

use std::{
    ffi::CStr,
    fmt,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use minicbor::{Decode, Encode};

/// The name of the lock file in the cache directory.
pub const LOCK_FILE_NAME: &str = "kizz.lock";

/// What a lock holder writes into the lock file, so that a waiting process can say
/// what it is waiting for.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct LockDetails {
    /// The process ID of the holder.
    #[n(0)]
    pub pid: u32,
    /// The name of the user the holder runs as, or the user ID when it has no name.
    #[n(1)]
    pub user: String,
    /// The host name of the machine.
    #[n(2)]
    pub host: String,
    /// The working directory of the holder.
    #[n(3)]
    pub working_directory: String,
    /// The command line of the holder, one item per argument.
    #[n(4)]
    pub command_line: Vec<String>,
    /// When the holder started, in nanoseconds since the Unix epoch.
    #[n(5)]
    pub start_time_nanos: u64,
}

impl LockDetails {
    /// The details of this process, started now.
    #[must_use]
    pub fn current() -> Self {
        let start_time_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
            });
        Self {
            pid: std::process::id(),
            user: user_name(),
            host: host_name(),
            working_directory: std::env::current_dir().map_or_else(
                |_| String::from("(unknown)"),
                |dir| dir.to_string_lossy().into_owned(),
            ),
            command_line: std::env::args_os()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            start_time_nanos,
        }
    }

    /// The details in the lock file at `path`, if it holds a complete record.
    ///
    /// A file that is missing, empty, or still being written gives `None`.
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        minicbor::decode(&bytes).ok()
    }
}

impl fmt::Display for LockDetails {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(
            f,
            "process {} of user {} on host {}, started {}, in {}: {}",
            self.pid,
            self.user,
            self.host,
            format_utc(self.start_time_nanos),
            self.working_directory,
            self.command_line.join(" ")
        )
    }
}

/// How a process waits for the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockOptions {
    /// How long a waiting process sleeps between two attempts to take the lock.
    pub poll_interval: Duration,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(20),
        }
    }
}

/// Why the lock could not be taken or released.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// The cache directory could not be created.
    #[error("cannot create the cache directory `{}`", path.display())]
    CreateDir {
        /// The cache directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// The lock file could not be opened, locked, or checked.
    #[error("cannot lock `{}`", path.display())]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// The holder's details could not be written into the lock file.
    #[error("cannot write the lock holder's details to `{}`", path.display())]
    WriteDetails {
        /// The lock file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// The lock file could not be deleted on release.
    #[error("cannot delete the lock file `{}`", path.display())]
    Remove {
        /// The lock file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
}

/// The exclusive lock on a cache, held until it is released or dropped.
#[derive(Debug)]
pub struct CacheLock {
    /// The lock file.
    path: PathBuf,
    /// The locked file, `None` once released.
    file: Option<File>,
}

impl CacheLock {
    /// Takes the lock of the cache in `cache_dir`, creating the directory if needed,
    /// and writes `details` into the lock file.
    ///
    /// While another process holds the lock, this waits, and calls `on_wait` with the
    /// holder's details at once, and again whenever a different process takes the
    /// lock.
    ///
    /// # Errors
    ///
    /// A [`LockError`] when the directory or the lock file cannot be created, locked,
    /// or written.
    pub fn acquire(
        cache_dir: &Path,
        details: &LockDetails,
        on_wait: impl FnMut(&LockDetails),
    ) -> Result<Self, LockError> {
        Self::acquire_with(cache_dir, details, LockOptions::default(), on_wait)
    }

    /// As [`CacheLock::acquire`], with explicit [`LockOptions`].
    ///
    /// # Errors
    ///
    /// As for [`CacheLock::acquire`].
    pub fn acquire_with(
        cache_dir: &Path,
        details: &LockDetails,
        options: LockOptions,
        mut on_wait: impl FnMut(&LockDetails),
    ) -> Result<Self, LockError> {
        acquire_inner(cache_dir, details, options, &mut on_wait, &mut |_| {})
    }

    /// The lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Deletes the lock file, then releases the lock.
    ///
    /// # Errors
    ///
    /// [`LockError::Remove`] when the file cannot be deleted; the lock is released
    /// anyway.
    pub fn release(mut self) -> Result<(), LockError> {
        self.release_inner()
    }

    /// Deletes the lock file while the lock is still held, then releases the lock by
    /// closing the file. Does nothing when already released.
    fn release_inner(&mut self) -> Result<(), LockError> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        let removed = match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(LockError::Remove {
                    path: self.path.clone(),
                    source: error,
                })
            },
            _ => Ok(()),
        };
        drop(file);
        removed
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        // A failure to delete the file is harmless: the next process takes it over.
        let _ignored: Result<(), LockError> = self.release_inner();
    }
}

/// Takes the lock as [`CacheLock::acquire_with`] does, and calls `after_lock` with
/// the lock file each time it has locked a file, before checking that the file is
/// still the one at the path. Tests use `after_lock` to replace the file at exactly
/// that moment.
fn acquire_inner(
    cache_dir: &Path,
    details: &LockDetails,
    options: LockOptions,
    on_wait: &mut dyn FnMut(&LockDetails),
    after_lock: &mut dyn FnMut(&Path),
) -> Result<CacheLock, LockError> {
    fs::create_dir_all(cache_dir).map_err(|source| {
        LockError::CreateDir {
            path: cache_dir.to_path_buf(),
            source,
        }
    })?;
    let path = cache_dir.join(LOCK_FILE_NAME);
    let lock_error = |source: io::Error| {
        LockError::Lock {
            path: path.clone(),
            source,
        }
    };
    let mut reported: Option<LockDetails> = None;
    loop {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(lock_error)?;
        match file.try_lock() {
            Ok(()) => {
                after_lock(&path);
                if !is_at_path(&file, &path).map_err(lock_error)? {
                    // The holder before us deleted the file after we opened it: the
                    // lock we hold is on a file nobody else will open again.
                    continue;
                }
                write_details(&file, details).map_err(|source| {
                    LockError::WriteDetails {
                        path: path.clone(),
                        source,
                    }
                })?;
                return Ok(CacheLock {
                    path,
                    file: Some(file),
                });
            },
            Err(TryLockError::WouldBlock) => {
                drop(file);
                if let Some(holder) = LockDetails::read(&path)
                    && reported.as_ref() != Some(&holder)
                {
                    on_wait(&holder);
                    reported = Some(holder);
                }
                thread::sleep(options.poll_interval);
            },
            Err(TryLockError::Error(source)) => return Err(lock_error(source)),
        }
    }
}

/// Whether `file` is still the file at `path`.
fn is_at_path(
    file: &File,
    path: &Path,
) -> io::Result<bool> {
    let held = file.metadata()?;
    match fs::metadata(path) {
        Ok(current) => Ok(current.dev() == held.dev() && current.ino() == held.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Replaces the contents of the locked `file` with `details`.
fn write_details(
    mut file: &File,
    details: &LockDetails,
) -> io::Result<()> {
    let bytes = minicbor::to_vec(details).map_err(io::Error::other)?;
    file.set_len(0)?;
    file.write_all(&bytes)?;
    file.flush()
}

/// The name of the effective user, or its user ID when it has no name.
fn user_name() -> String {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer: Vec<libc::c_char> = vec![0; 16_384];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call, and `buffer.len()` is the length of
    // the buffer that `buffer.as_mut_ptr()` points to.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut result,
        )
    };
    if status != 0 || result.is_null() {
        return uid.to_string();
    }
    // SAFETY: `getpwuid_r` succeeded and set `result`, so it initialized `entry`.
    let name = unsafe { entry.assume_init_ref() }.pw_name;
    if name.is_null() {
        return uid.to_string();
    }
    // SAFETY: `pw_name` is a NUL-terminated string in `buffer`, which is still alive.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

/// The host name of the machine, or `(unknown)` when it cannot be read; the lock
/// file and the cache identity both name it.
pub(crate) fn host_name() -> String {
    let mut buffer = [0_u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which outlives the call.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return String::from("(unknown)");
    }
    CStr::from_bytes_until_nul(&buffer).map_or_else(
        |_| String::from_utf8_lossy(&buffer).into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Formats nanoseconds since the Unix epoch as a UTC time, such as
/// `2026-10-24T05:12:00Z`, or as the number of nanoseconds after the epoch when the
/// conversion cannot be made, which no `u64` causes.
fn format_utc(nanos: u64) -> String {
    civil_utc(nanos).unwrap_or_else(|| format!("{nanos} nanoseconds after the epoch"))
}

/// The UTC time `nanos` nanoseconds after the Unix epoch, converted to a civil date
/// with checked arithmetic, which cannot panic where the arithmetic operators would
/// on an overflow ("Kizz never panics"); `None` when a step overflows.
fn civil_utc(nanos: u64) -> Option<String> {
    let seconds = nanos.checked_div(1_000_000_000)?;
    let days = seconds.checked_div(86_400)?;
    let second_of_day = seconds.checked_rem(86_400)?;
    // The conversion from days since the epoch to a civil date, for days on or after
    // 1970-01-01.
    let shifted = days.checked_add(719_468)?;
    let era = shifted.checked_div(146_097)?;
    let day_of_era = shifted.checked_rem(146_097)?;
    let year_of_era = day_of_era
        .checked_sub(day_of_era.checked_div(1_460)?)?
        .checked_add(day_of_era.checked_div(36_524)?)?
        .checked_sub(day_of_era.checked_div(146_096)?)?
        .checked_div(365)?;
    let day_of_year = day_of_era.checked_sub(
        year_of_era
            .checked_mul(365)?
            .checked_add(year_of_era.checked_div(4)?)?
            .checked_sub(year_of_era.checked_div(100)?)?,
    )?;
    let shifted_month = day_of_year
        .checked_mul(5)?
        .checked_add(2)?
        .checked_div(153)?;
    let day = day_of_year
        .checked_sub(
            shifted_month
                .checked_mul(153)?
                .checked_add(2)?
                .checked_div(5)?,
        )?
        .checked_add(1)?;
    let month = if shifted_month < 10 {
        shifted_month.checked_add(3)?
    } else {
        shifted_month.checked_sub(9)?
    };
    let year = year_of_era
        .checked_add(era.checked_mul(400)?)?
        .checked_add(u64::from(month <= 2))?;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day.checked_div(3_600)?,
        second_of_day.checked_rem(3_600)?.checked_div(60)?,
        second_of_day.checked_rem(60)?
    ))
}

/// Unit tests for the lock file within one process.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: a thread of a scope waits for the lock, and the tests assert"
)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Details of a made-up holder.
    fn details(pid: u32) -> LockDetails {
        LockDetails {
            pid,
            user: String::from("builder"),
            host: String::from("build-host"),
            working_directory: String::from("/work"),
            command_line: vec![String::from("kizz"), String::from("build")],
            start_time_nanos: 1_761_282_720_000_000_000,
        }
    }

    #[test]
    fn holder_details_are_written_and_the_file_is_deleted_on_release() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let lock = CacheLock::acquire(&cache, &details(7), |_| {
            panic!("nobody else holds the lock")
        })
        .unwrap();
        assert_eq!(lock.path(), cache.join(LOCK_FILE_NAME));
        assert_eq!(LockDetails::read(lock.path()), Some(details(7)));
        lock.release().unwrap();
        assert!(!cache.join(LOCK_FILE_NAME).exists());
    }

    #[test]
    fn dropping_the_lock_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock = CacheLock::acquire(dir.path(), &details(7), |_| {}).unwrap();
        let path = lock.path().to_path_buf();
        assert!(path.exists());
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn a_stale_file_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE_NAME);
        fs::write(&path, minicbor::to_vec(details(1)).unwrap()).unwrap();
        let stale_inode = fs::metadata(&path).unwrap().ino();
        let lock = CacheLock::acquire(dir.path(), &details(2), |_| {
            panic!("a stale file is not a holder")
        })
        .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), stale_inode);
        assert_eq!(LockDetails::read(&path), Some(details(2)));
        drop(lock);
    }

    #[test]
    fn the_holder_retries_when_the_locked_file_is_no_longer_at_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE_NAME);
        let locked = Cell::new(0_u32);
        let replaced_inode = Cell::new(None);
        let lock = acquire_inner(
            dir.path(),
            &details(3),
            LockOptions::default(),
            &mut |_| panic!("nobody else holds the lock"),
            &mut |lock_path| {
                locked.set(locked.get().saturating_add(1));
                if locked.get() == 1 {
                    // Another process deletes the file we just locked, and a third
                    // creates a new file at the path.
                    fs::remove_file(lock_path).unwrap();
                    fs::write(lock_path, b"").unwrap();
                    replaced_inode.set(Some(fs::metadata(lock_path).unwrap().ino()));
                }
            },
        )
        .unwrap();
        assert_eq!(
            locked.get(),
            2,
            "the first lock was dropped and taken again"
        );
        let held = lock.file.as_ref().unwrap().metadata().unwrap().ino();
        assert_eq!(Some(held), replaced_inode.get());
        assert_eq!(fs::metadata(&path).unwrap().ino(), held);
        assert_eq!(LockDetails::read(&path), Some(details(3)));
        drop(lock);
    }

    #[test]
    fn a_waiting_lock_in_the_same_process_reports_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let holder = CacheLock::acquire(dir.path(), &details(10), |_| {}).unwrap();
        let reports = std::sync::Mutex::new(Vec::new());
        thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                CacheLock::acquire(dir.path(), &details(11), |seen| {
                    reports.lock().unwrap().push(seen.clone());
                })
            });
            while reports.lock().unwrap().is_empty() {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(!waiter.is_finished());
            drop(holder);
            let lock = waiter.join().unwrap().unwrap();
            assert_eq!(LockDetails::read(lock.path()), Some(details(11)));
        });
        assert_eq!(reports.into_inner().unwrap(), vec![details(10)]);
    }

    #[test]
    fn incomplete_details_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE_NAME);
        assert_eq!(LockDetails::read(&path), None);
        fs::write(&path, b"").unwrap();
        assert_eq!(LockDetails::read(&path), None);
        let bytes = minicbor::to_vec(details(4)).unwrap();
        fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert_eq!(LockDetails::read(&path), None);
    }

    #[test]
    fn details_display_every_field() {
        let text = details(42).to_string();
        assert_eq!(
            text,
            "process 42 of user builder on host build-host, started 2025-10-24T05:12:00Z, in \
             /work: kizz build"
        );
    }

    #[test]
    fn utc_formatting_handles_leap_years_and_the_epoch() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400_000_000_000), "2000-02-29T00:00:00Z");
        assert_eq!(
            format_utc(1_798_761_599_000_000_000),
            "2026-12-31T23:59:59Z"
        );
    }

    #[test]
    fn current_details_describe_this_process() {
        let current = LockDetails::current();
        assert_eq!(current.pid, std::process::id());
        assert_ne!(current.user, "");
        assert_ne!(current.host, "");
        assert_ne!(current.command_line, Vec::<String>::new());
        assert!(current.start_time_nanos > 0);
    }
}
