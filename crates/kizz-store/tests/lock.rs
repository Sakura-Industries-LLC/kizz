// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache lock between real processes.
//!
//! Each lock holder or waiter is this test executable started again in a helper
//! mode: [`helper_process`] does nothing in a normal test run, and takes the lock of
//! the cache that its explicit environment names when started with the helper
//! variable set. A helper reports what happens on its standard output, in lines that
//! start with [`MARK`], and releases the lock when its standard input closes.

use std::{
    ffi::OsStr,
    fs, io,
    io::{BufRead as _, BufReader, Read as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use kizz_store::{CacheLock, LOCK_FILE_NAME, LockDetails, LockOptions};

/// The variable that selects the helper mode of this test executable.
const HELPER_VARIABLE: &str = "KIZZ_STORE_TEST_HELPER";
/// The variable that names the cache directory of a helper.
const CACHE_VARIABLE: &str = "KIZZ_STORE_TEST_CACHE";
/// The variable that sets a helper's poll interval, in milliseconds.
const POLL_VARIABLE: &str = "KIZZ_STORE_TEST_POLL_MS";
/// The start of every line a helper reports.
const MARK: &str = "kizz-store-helper\t";
/// How long a test waits for something another process does.
const PATIENCE: Duration = Duration::from_secs(30);

/// A helper process and the lines it has reported.
struct Helper {
    /// The process.
    child: Child,
    /// Its standard input; closing it tells the helper to release the lock.
    stdin: Option<ChildStdin>,
    /// The reported lines, without the mark.
    lines: mpsc::Receiver<String>,
}

impl Helper {
    /// Starts this executable in helper `mode` for the cache in `cache`, polling every
    /// `poll_ms` milliseconds while it waits.
    fn start(
        mode: &str,
        cache: &Path,
        poll_ms: u64,
    ) -> io::Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "helper_process",
                "--nocapture",
                "--test-threads=1",
                "--quiet",
            ])
            .env_clear()
            .env(HELPER_VARIABLE, mode)
            .env(CACHE_VARIABLE, cache)
            .env(POLL_VARIABLE, poll_ms.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("the helper has no standard output"))?;
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    return;
                };
                if let Some(report) = line.strip_prefix(MARK)
                    && sender.send(report.to_owned()).is_err()
                {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
        })
    }

    /// The next reported line, split at tabs, or an error when none arrives in time.
    fn next(&self) -> Result<Vec<String>, mpsc::RecvTimeoutError> {
        let line = self.lines.recv_timeout(PATIENCE)?;
        Ok(line.split('\t').map(str::to_owned).collect())
    }

    /// The line the helper reports within `quiet`, if any.
    fn report_within(
        &self,
        quiet: Duration,
    ) -> Option<String> {
        self.lines.recv_timeout(quiet).ok()
    }

    /// Tells the helper to release the lock by closing its standard input.
    fn release(&mut self) {
        drop(self.stdin.take());
    }

    /// Tells the helper to release the lock, and waits for it to exit.
    fn finish(mut self) -> io::Result<ExitStatus> {
        self.release();
        self.child.wait()
    }
}

/// The work of a helper mode, run by this test executable when a test starts it with
/// [`HELPER_VARIABLE`] set. In a normal test run it does nothing.
///
/// Modes:
///
/// * `hold`: take the lock, reporting each holder it waits for; report `locked` with the
///   inode of the lock file; release the lock when standard input closes.
/// * `interrupt`: take the lock and release it when `SIGINT` arrives, as the build
///   command does on Ctrl-C.
#[test]
fn helper_process() {
    let Some(mode) = std::env::var_os(HELPER_VARIABLE) else {
        return;
    };
    let cache = std::env::var_os(CACHE_VARIABLE).unwrap();
    let poll_ms: u64 = std::env::var(POLL_VARIABLE).unwrap().parse().unwrap();
    let options = LockOptions {
        poll_interval: Duration::from_millis(poll_ms),
    };
    if mode == OsStr::new("interrupt") {
        install_interrupt_handler().unwrap();
    }
    let details = LockDetails::current();
    let lock = CacheLock::acquire_with(Path::new(&cache), &details, options, |holder| {
        println!("{MARK}waiting\t{}\t{holder}", holder.pid);
    })
    .unwrap();
    let inode = fs::metadata(lock.path()).unwrap().ino();
    println!("{MARK}locked\t{}\t{details}\t{inode}", details.pid);
    if mode == OsStr::new("interrupt") {
        while !INTERRUPTED.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
    } else {
        let mut rest = Vec::new();
        io::stdin().read_to_end(&mut rest).unwrap();
    }
    lock.release().unwrap();
    println!("{MARK}released");
}

/// Set when the `interrupt` helper receives `SIGINT`.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Records that `SIGINT` arrived.
extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Makes `SIGINT` set [`INTERRUPTED`] instead of ending the process.
fn install_interrupt_handler() -> io::Result<()> {
    let handler: extern "C" fn(libc::c_int) = on_interrupt;
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    let previous = unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t) };
    if previous == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The lock file of `cache`.
fn lock_file(cache: &Path) -> PathBuf {
    cache.join(LOCK_FILE_NAME)
}

#[test]
fn a_second_process_waits_and_prints_the_first_process_details() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    let first = Helper::start("hold", &cache, 20).unwrap();
    let locked = first.next().unwrap();
    assert_eq!(locked[0], "locked");
    let (first_pid, first_details) = (locked[1].clone(), locked[2].clone());
    assert!(first_details.starts_with(&format!("process {first_pid} of user ")));

    let second = Helper::start("hold", &cache, 20).unwrap();
    assert_eq!(second.next().unwrap(), vec![
        "waiting".to_owned(),
        first_pid,
        first_details
    ]);
    assert_eq!(second.report_within(Duration::from_millis(300)), None);

    assert!(first.finish().unwrap().success());
    assert_eq!(second.next().unwrap()[0], "locked");
    assert!(second.finish().unwrap().success());
    assert!(!lock_file(&cache).exists());
}

#[test]
fn the_waiting_process_reports_again_when_a_different_process_takes_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    let first = Helper::start("hold", &cache, 20).unwrap();
    let first_locked = first.next().unwrap();
    assert_eq!(first_locked[0], "locked");

    // The waiter polls slowly, and the next holder quickly, so that the next holder
    // takes the lock between two of the waiter's attempts.
    let waiter = Helper::start("hold", &cache, 3_000).unwrap();
    assert_eq!(waiter.next().unwrap()[..3], [
        "waiting",
        first_locked[1].as_str(),
        first_locked[2].as_str()
    ]);

    let next = Helper::start("hold", &cache, 1).unwrap();
    assert_eq!(next.next().unwrap()[..2], [
        "waiting",
        first_locked[1].as_str()
    ]);

    assert!(first.finish().unwrap().success());
    let next_locked = next.next().unwrap();
    assert_eq!(next_locked[0], "locked");
    assert_ne!(next_locked[1], first_locked[1]);

    assert_eq!(waiter.next().unwrap()[..3], [
        "waiting",
        next_locked[1].as_str(),
        next_locked[2].as_str()
    ]);
    assert!(next.finish().unwrap().success());
    assert_eq!(waiter.next().unwrap()[0], "locked");
    assert!(waiter.finish().unwrap().success());
    assert!(!lock_file(&cache).exists());
}

#[test]
fn after_the_holder_is_killed_the_next_process_takes_the_lock_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    let mut killed = Helper::start("hold", &cache, 20).unwrap();
    let locked = killed.next().unwrap();
    assert_eq!(locked[0], "locked");
    // `Child::kill` sends SIGKILL.
    killed.child.kill().unwrap();
    killed.child.wait().unwrap();

    // The crash left the file, with the dead holder's details, behind.
    let stale = lock_file(&cache);
    assert_eq!(
        LockDetails::read(&stale).map(|details| details.pid.to_string()),
        Some(locked[1].clone())
    );
    let stale_inode = fs::metadata(&stale).unwrap().ino();

    let next = Helper::start("hold", &cache, 20).unwrap();
    let report = next.next().unwrap();
    // The first report is the lock itself: the next process never waited.
    assert_eq!(report[0], "locked", "{report:?}");
    // It took over the file that the crash left behind.
    assert_eq!(report[3], stale_inode.to_string());
    assert_eq!(
        LockDetails::read(&stale).map(|details| details.pid.to_string()),
        Some(report[1].clone())
    );
    assert!(next.finish().unwrap().success());
    assert!(!stale.exists());
}

#[test]
fn the_lock_file_is_gone_after_a_clean_exit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let mut holder = Helper::start("hold", &cache, 20).unwrap();
    assert_eq!(holder.next().unwrap()[0], "locked");
    assert!(lock_file(&cache).exists());
    holder.release();
    assert_eq!(holder.next().unwrap(), vec!["released".to_owned()]);
    assert!(holder.child.wait().unwrap().success());
    assert!(!lock_file(&cache).exists());
}

#[test]
fn the_lock_file_is_gone_after_an_interrupted_exit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let mut holder = Helper::start("interrupt", &cache, 20).unwrap();
    assert_eq!(holder.next().unwrap()[0], "locked");
    assert!(lock_file(&cache).exists());
    let pid = libc::pid_t::try_from(holder.child.id()).unwrap();
    // SAFETY: `kill` has no memory-safety preconditions; the helper is our child and
    // has not been waited for, so its process ID is still its own.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    assert_eq!(holder.next().unwrap(), vec!["released".to_owned()]);
    assert!(holder.child.wait().unwrap().success());
    assert!(!lock_file(&cache).exists());
}
