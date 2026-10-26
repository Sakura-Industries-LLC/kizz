// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The one place where kizz starts threads ("Kizz never panics").
//!
//! [`spawn`] starts a named thread, and [`spawn_scoped`] starts one in a
//! [`std::thread::scope`]. Neither panics: where `std::thread::spawn` and
//! `std::thread::Scope::spawn` panic when the system refuses a thread, these return
//! a [`StartError`]. When the system cannot start a thread for the moment, which it
//! reports as `EAGAIN` and Rust as [`io::ErrorKind::WouldBlock`], they wait and try
//! again, after each of the waits of [`WAITS`] in turn: 100, 200, 400, 800, 1,600
//! and 3,200 milliseconds, then 4 seconds four times, 22.3 seconds in all. Any other
//! error, or a tenth retry that fails, is the error, which names the thread, the
//! attempts, the time waited and the system's error. The waits have no random part,
//! and each start retries on its own, so a start never waits longer than that.
//!
//! `std::thread::Builder` drops the work of a start that fails, so the helper keeps
//! the work outside each attempt, and a thread that starts after retries runs it
//! exactly once. The helper's calls of `std::thread::Builder` sit in one private
//! module, under the one allowance of production code for the bans of `clippy.toml`,
//! which forbid every other way of starting a thread.
//!
//! In builds with `debug_assertions`, which tests use and release builds lack, the
//! module `testing` can make the next attempts to start threads fail on purpose,
//! for one thread and the threads that it starts through the helper, or for every
//! thread of the process. Those attempts go through the same retries, which then
//! record each wait instead of sleeping, so tests check the waits exactly and finish
//! at once. A release build has no such module and cannot fail a start on purpose.

use std::{
    any::Any,
    fmt, io,
    sync::{Arc, Mutex, PoisonError},
    thread::{self, Scope, Thread},
    time::Duration,
};

/// The waits before each retry of a start that the system refused for the moment:
/// they double from 100 milliseconds up to 4 seconds, 22.3 seconds in all, so a
/// start makes at most 11 attempts.
pub const WAITS: [Duration; 10] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1_600),
    Duration::from_millis(3_200),
    Duration::from_secs(4),
    Duration::from_secs(4),
    Duration::from_secs(4),
    Duration::from_secs(4),
];

/// A thread that could not be started.
#[derive(Debug)]
pub struct StartError {
    /// What the thread is for, such as "the tracing thread".
    what: String,
    /// How many attempts were made.
    attempts: u32,
    /// How long the attempts waited in all.
    waited: Duration,
    /// The error of the last attempt.
    error: io::Error,
}

impl StartError {
    /// What the thread is for, such as "the tracing thread".
    #[must_use]
    pub fn what(&self) -> &str {
        &self.what
    }

    /// How many attempts were made: 11 when every retry met `EAGAIN`.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// How long the attempts waited in all.
    #[must_use]
    pub const fn waited(&self) -> Duration {
        self.waited
    }

    /// The system's error of the last attempt.
    #[must_use]
    pub const fn error(&self) -> &io::Error {
        &self.error
    }
}

impl fmt::Display for StartError {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let tenths = self.waited.as_millis().checked_div(100).unwrap_or_default();
        write!(
            f,
            "could not start {} after {} {} in {}.{} seconds: {}",
            self.what,
            self.attempts,
            if self.attempts == 1 {
                "attempt"
            } else {
                "attempts"
            },
            tenths.checked_div(10).unwrap_or_default(),
            tenths.checked_rem(10).unwrap_or_default(),
            self.error,
        )
    }
}

// The message holds the system's error, so the error has no source of its own,
// and a chain of errors does not print it twice.
impl std::error::Error for StartError {}

/// The handle of a thread that [`spawn`] started.
#[derive(Debug)]
pub struct JoinHandle<T>(thread::JoinHandle<Option<T>>);

impl<T> JoinHandle<T> {
    /// Wait for the thread to finish and return its result, or the payload of its
    /// panic.
    ///
    /// # Errors
    ///
    /// The payload of the thread's panic, as [`std::thread::JoinHandle::join`]
    /// returns it.
    pub fn join(self) -> thread::Result<T> {
        self.0.join().and_then(ran)
    }

    /// The thread.
    #[must_use]
    pub fn thread(&self) -> &Thread {
        self.0.thread()
    }
}

/// The handle of a thread that [`spawn_scoped`] started.
#[derive(Debug)]
pub struct ScopedJoinHandle<'scope, T>(thread::ScopedJoinHandle<'scope, Option<T>>);

impl<T> ScopedJoinHandle<'_, T> {
    /// Wait for the thread to finish and return its result, or the payload of its
    /// panic.
    ///
    /// # Errors
    ///
    /// The payload of the thread's panic, as
    /// [`std::thread::ScopedJoinHandle::join`] returns it.
    pub fn join(self) -> thread::Result<T> {
        self.0.join().and_then(ran)
    }

    /// The thread.
    #[must_use]
    pub fn thread(&self) -> &Thread {
        self.0.thread()
    }
}

/// The result of a thread's work, or, as the payload of a panic, the message that
/// says that the thread found no work. Neither function that starts a thread gives
/// a thread that runs without its work, so the payload is never seen.
fn ran<T>(result: Option<T>) -> thread::Result<T> {
    result.ok_or_else(missing_work)
}

/// The payload that [`ran`] gives a thread that found no work.
fn missing_work() -> Box<dyn Any + Send> {
    Box::new("the thread found no work to run")
}

/// The work of a start, kept outside each attempt so that an attempt that fails
/// drops only its own reference to it.
struct Slot<F>(Mutex<Option<F>>);

impl<F> Slot<F> {
    /// A slot that holds `work`.
    const fn new(work: F) -> Self {
        Self(Mutex::new(Some(work)))
    }

    /// The work, the first time it is taken.
    fn take(&self) -> Option<F> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// Start a thread named `name`, which is `what`, such as "the tracing thread", to
/// run `work`, retrying while the system cannot start it for the moment.
///
/// # Errors
///
/// A [`StartError`] when the system refuses the thread with an error other than
/// `EAGAIN`, at once, or when its tenth retry fails with `EAGAIN`, after every wait
/// of [`WAITS`]. A `name` that holds a NUL byte is refused without an attempt. The
/// work never runs then.
pub fn spawn<F, T>(
    name: &str,
    what: &str,
    work: F,
) -> Result<JoinHandle<T>, StartError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    check_name(name, what)?;
    let slot = Arc::new(Slot::new(work));
    let inherited = hooks::current();
    retry(name, what, || {
        let slot = Arc::clone(&slot);
        let inherited = inherited.clone();
        os::spawn(name, move || {
            hooks::adopt(inherited);
            slot.take().map(|work| work())
        })
    })
    .map(JoinHandle)
}

/// Start a thread named `name` in `scope`, which is `what`, such as "the thread that
/// feeds stdin", to run `work`, retrying while the system cannot start it for the
/// moment.
///
/// # Errors
///
/// A [`StartError`] when the system refuses the thread with an error other than
/// `EAGAIN`, at once, or when its tenth retry fails with `EAGAIN`, after every wait
/// of [`WAITS`]. A `name` that holds a NUL byte is refused without an attempt. The
/// work never runs then.
pub fn spawn_scoped<'scope, F, T>(
    scope: &'scope Scope<'scope, '_>,
    name: &str,
    what: &str,
    work: F,
) -> Result<ScopedJoinHandle<'scope, T>, StartError>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    check_name(name, what)?;
    let slot = Arc::new(Slot::new(work));
    let inherited = hooks::current();
    retry(name, what, || {
        let slot = Arc::clone(&slot);
        let inherited = inherited.clone();
        os::spawn_scoped(scope, name, move || {
            hooks::adopt(inherited);
            slot.take().map(|work| work())
        })
    })
    .map(ScopedJoinHandle)
}

/// Refuse a thread name that holds a NUL byte, which `std::thread::Builder` would
/// panic on.
fn check_name(
    name: &str,
    what: &str,
) -> Result<(), StartError> {
    if name.contains('\0') {
        return Err(StartError {
            what: what.to_owned(),
            attempts: 0,
            waited: Duration::ZERO,
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "the name of the thread holds a NUL byte",
            ),
        });
    }
    Ok(())
}

/// Make attempts to start the thread named `name`, which is `what`, with `attempt`,
/// waiting after each one that fails with `EAGAIN` as [`WAITS`] says, until one
/// succeeds, one fails with another error, or the tenth retry fails.
fn retry<H>(
    name: &str,
    what: &str,
    mut attempt: impl FnMut() -> io::Result<H>,
) -> Result<H, StartError> {
    let mut waits = WAITS.iter().copied();
    let mut attempts: u32 = 0;
    let mut waited = Duration::ZERO;
    loop {
        attempts = attempts.saturating_add(1);
        let (result, injected) = match hooks::next_failure(name) {
            Some((error, injection)) => (Err(error), Some(injection)),
            None => (attempt(), None),
        };
        let error = match result {
            Ok(handle) => return Ok(handle),
            Err(error) => error,
        };
        let wait = if error.kind() == io::ErrorKind::WouldBlock {
            waits.next()
        } else {
            None
        };
        let Some(wait) = wait else {
            return Err(StartError {
                what: what.to_owned(),
                attempts,
                waited,
                error,
            });
        };
        match injected {
            Some(injection) => hooks::record_wait(&injection, wait),
            None => thread::sleep(wait),
        }
        waited = waited.saturating_add(wait);
    }
}

/// The only calls of `std::thread::Builder` in kizz's production code.
#[expect(
    clippy::disallowed_methods,
    reason = "the helper is the one place that starts threads: it retries what \
              std::thread::Builder refuses, where the banned calls would panic"
)]
mod os {
    use std::{
        io,
        thread::{Builder, JoinHandle, Scope, ScopedJoinHandle},
    };

    /// Make one attempt to start a thread named `name` that runs `work`.
    pub(super) fn spawn<F, T>(
        name: &str,
        work: F,
    ) -> io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Builder::new().name(name.to_owned()).spawn(work)
    }

    /// Make one attempt to start a thread named `name` in `scope` that runs `work`.
    pub(super) fn spawn_scoped<'scope, F, T>(
        scope: &'scope Scope<'scope, '_>,
        name: &str,
        work: F,
    ) -> io::Result<ScopedJoinHandle<'scope, T>>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        Builder::new()
            .name(name.to_owned())
            .spawn_scoped(scope, work)
    }
}

/// How the retries reach the failures of [`testing`], in builds with
/// `debug_assertions`.
#[cfg(debug_assertions)]
mod hooks {
    pub(crate) use crate::testing::{adopt, current, next_failure, record_wait};
}

/// The hooks of release builds, which inject nothing.
#[cfg(not(debug_assertions))]
mod hooks {
    use std::{io, time::Duration};

    /// What a thread passes to the threads it starts: nothing.
    pub(crate) const fn current() {}

    /// Take on what the starting thread passed: nothing.
    pub(crate) const fn adopt(_inherited: ()) {}

    /// No attempt fails on purpose.
    pub(crate) const fn next_failure(_name: &str) -> Option<(io::Error, ())> {
        None
    }

    /// Never called, because no attempt fails on purpose.
    pub(crate) const fn record_wait(
        _injection: &(),
        _wait: Duration,
    ) {
    }
}

/// Failures of thread starts that tests cause on purpose, only in builds with
/// `debug_assertions`, which tests use and release builds lack.
///
/// [`fail_starts`](testing::fail_starts) and
/// [`fail_starts_named`](testing::fail_starts_named) make the next attempts of the
/// calling thread fail, and of the threads that it starts through the helper, which
/// share the same count; threads that it starts otherwise, and every other thread,
/// never meet them, so tests that run at once in one process never meet each other's
/// failures. [`fail_starts_in_process`](testing::fail_starts_in_process) makes the
/// next attempts of every thread of the process fail, for the `kizz` binary's
/// end-to-end test. An attempt that fails on purpose goes through the retries, which
/// record its wait in the failures instead of sleeping.
#[cfg(debug_assertions)]
pub mod testing {
    use std::{
        cell::Cell,
        io,
        sync::{Arc, Mutex, MutexGuard, PoisonError},
        time::Duration,
    };

    /// The error number of `EAGAIN` on Linux: the system cannot start a thread for
    /// the moment.
    pub const EAGAIN: i32 = 11;

    /// How an attempt that fails on purpose fails.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Failure {
        /// With `EAGAIN`, which the helper retries.
        Busy,
        /// With the system error of this number.
        Os(i32),
    }

    impl Failure {
        /// The error of an attempt that fails this way.
        fn error(self) -> io::Error {
            io::Error::from_raw_os_error(match self {
                Self::Busy => EAGAIN,
                Self::Os(number) => number,
            })
        }
    }

    /// The failures of one injection, and the waits of the attempts that met them.
    #[derive(Debug)]
    pub(crate) struct Failures {
        /// How many attempts still fail.
        remaining: u32,
        /// How they fail.
        failure: Failure,
        /// The one thread name whose attempts fail, or `None` for every name.
        name: Option<String>,
        /// The wait after each attempt that failed with `EAGAIN` and was retried, in
        /// order.
        waits: Vec<Duration>,
    }

    /// Failures shared by the threads that meet them.
    pub(crate) type Shared = Arc<Mutex<Failures>>;

    thread_local! {
        /// The failures of the calling thread, if any.
        static CURRENT: Cell<Option<Shared>> = const { Cell::new(None) };
    }

    /// The failures of every thread of the process, if any.
    static PROCESS: Mutex<Option<Shared>> = Mutex::new(None);

    /// Lock `mutex`, whether or not a thread panicked while holding it.
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Put `failures` in place for the calling thread, and return those it had.
    fn replace(failures: Option<Shared>) -> Option<Shared> {
        CURRENT
            .try_with(|cell| cell.replace(failures))
            .ok()
            .flatten()
    }

    /// The failures of the calling thread, which a thread that it starts takes on.
    pub(crate) fn current() -> Option<Shared> {
        CURRENT
            .try_with(|cell| {
                let failures = cell.take();
                cell.set(failures.clone());
                failures
            })
            .ok()
            .flatten()
    }

    /// Take on `failures` in a new thread, as the thread that started it had them.
    pub(crate) fn adopt(failures: Option<Shared>) {
        drop(replace(failures));
    }

    /// The error of the next attempt to start a thread named `name`, when it fails on
    /// purpose, with the failures that it met.
    pub(crate) fn next_failure(name: &str) -> Option<(io::Error, Shared)> {
        let shared = current().or_else(|| lock(&PROCESS).clone())?;
        let failure = {
            let mut failures = lock(&shared);
            let applies = failures.name.as_deref().is_none_or(|only| only == name);
            if !applies || failures.remaining == 0 {
                return None;
            }
            failures.remaining = failures.remaining.saturating_sub(1);
            failures.failure
        };
        Some((failure.error(), shared))
    }

    /// Record `wait` in `failures`, in place of sleeping.
    pub(crate) fn record_wait(
        failures: &Shared,
        wait: Duration,
    ) {
        lock(failures).waits.push(wait);
    }

    /// Failures that the calling thread, and the threads that it starts through the
    /// helper, meet until this is dropped; then the thread has the failures it had
    /// before.
    #[derive(Debug)]
    #[must_use = "the failures end when this is dropped"]
    pub struct Injected {
        /// The failures.
        shared: Shared,
        /// The failures that the calling thread had before.
        previous: Option<Shared>,
    }

    impl Injected {
        /// The wait after each attempt that failed on purpose with `EAGAIN` and was
        /// retried, in order.
        #[must_use]
        pub fn waits(&self) -> Vec<Duration> {
            lock(&self.shared).waits.clone()
        }

        /// How many attempts still fail.
        #[must_use]
        pub fn remaining(&self) -> u32 {
            lock(&self.shared).remaining
        }
    }

    impl Drop for Injected {
        fn drop(&mut self) {
            adopt(self.previous.take());
        }
    }

    /// Make the next `count` attempts to start a thread fail as `failure` says.
    pub fn fail_starts(
        count: u32,
        failure: Failure,
    ) -> Injected {
        inject(count, failure, None)
    }

    /// Make the next `count` attempts to start a thread named `name` fail as
    /// `failure` says, and leave the starts of threads of other names alone.
    pub fn fail_starts_named(
        count: u32,
        failure: Failure,
        name: &str,
    ) -> Injected {
        inject(count, failure, Some(name.to_owned()))
    }

    /// Put failures in place for the calling thread.
    fn inject(
        count: u32,
        failure: Failure,
        name: Option<String>,
    ) -> Injected {
        let shared = Arc::new(Mutex::new(Failures {
            remaining: count,
            failure,
            name,
            waits: Vec::new(),
        }));
        let previous = replace(Some(Arc::clone(&shared)));
        Injected { shared, previous }
    }

    /// Make the next `count` attempts to start any thread of the process fail with
    /// `EAGAIN`, where the starting thread has no failures of its own.
    pub fn fail_starts_in_process(count: u32) {
        *lock(&PROCESS) = Some(Arc::new(Mutex::new(Failures {
            remaining: count,
            failure: Failure::Busy,
            name: None,
            waits: Vec::new(),
        })));
    }
}

/// Unit tests of the retries, with the failures of [`testing`].
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: a plain std thread shows that failures stay with their thread"
)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use super::{
        WAITS, spawn, spawn_scoped,
        testing::{Failure, fail_starts, fail_starts_named},
    };

    /// How long a test that records its waits may take: far less than the waits.
    const AT_ONCE: Duration = Duration::from_secs(5);

    /// A plain start that meets `EAGAIN` 1, 5 or 10 times starts at the next attempt,
    /// runs its work exactly once, and records exactly the first 1, 5 or 10 waits.
    #[test]
    fn a_plain_start_retries_after_each_wait() {
        for count in [1_usize, 5, 10] {
            let started = Instant::now();
            let failures = fail_starts(u32::try_from(count).unwrap(), Failure::Busy);
            let runs = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&runs);
            let handle = spawn("kizz-test", "the test thread", move || {
                counter.fetch_add(1, Ordering::SeqCst);
                7
            })
            .unwrap();
            assert_eq!(handle.join().unwrap(), 7);
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            assert_eq!(failures.waits(), WAITS[..count]);
            assert_eq!(failures.remaining(), 0);
            assert!(started.elapsed() < AT_ONCE);
        }
    }

    /// A scoped start that meets `EAGAIN` 1, 5 or 10 times starts at the next
    /// attempt, runs its work exactly once, and records exactly the first 1, 5 or 10
    /// waits.
    #[test]
    fn a_scoped_start_retries_after_each_wait() {
        for count in [1_usize, 5, 10] {
            let started = Instant::now();
            let failures = fail_starts(u32::try_from(count).unwrap(), Failure::Busy);
            let runs = AtomicUsize::new(0);
            let value = thread::scope(|scope| {
                spawn_scoped(scope, "kizz-test", "the test thread", || {
                    runs.fetch_add(1, Ordering::SeqCst);
                    11
                })
                .unwrap()
                .join()
                .unwrap()
            });
            assert_eq!(value, 11);
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            assert_eq!(failures.waits(), WAITS[..count]);
            assert!(started.elapsed() < AT_ONCE);
        }
    }

    /// A plain start that meets `EAGAIN` 11 times fails after the 10 waits, 22.3
    /// seconds in all, with an error that names the thread, the attempts and error 11,
    /// and its work never runs.
    #[test]
    fn a_plain_start_fails_after_its_tenth_retry() {
        let started = Instant::now();
        let failures = fail_starts(11, Failure::Busy);
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let error = spawn("kizz-test", "the test thread", move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap_err();
        assert_eq!(error.attempts(), 11);
        assert_eq!(error.waited(), Duration::from_millis(22_300));
        assert_eq!(error.error().raw_os_error(), Some(11));
        assert_eq!(error.what(), "the test thread");
        let message = error.to_string();
        assert!(
            message
                .starts_with("could not start the test thread after 11 attempts in 22.3 seconds: "),
            "{message}"
        );
        assert!(message.ends_with("(os error 11)"), "{message}");
        assert_eq!(failures.waits(), WAITS);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(started.elapsed() < AT_ONCE);
    }

    /// A scoped start that meets `EAGAIN` 11 times fails after the 10 waits, with an
    /// error that names the thread, the attempts and error 11, and its work never
    /// runs.
    #[test]
    fn a_scoped_start_fails_after_its_tenth_retry() {
        let failures = fail_starts(11, Failure::Busy);
        let runs = AtomicUsize::new(0);
        let message = thread::scope(|scope| {
            spawn_scoped(scope, "kizz-test", "the scoped test thread", || {
                runs.fetch_add(1, Ordering::SeqCst);
            })
            .map(|_| ())
            .unwrap_err()
            .to_string()
        });
        assert!(
            message.starts_with(
                "could not start the scoped test thread after 11 attempts in 22.3 seconds: "
            ),
            "{message}"
        );
        assert!(message.ends_with("(os error 11)"), "{message}");
        assert_eq!(failures.waits(), WAITS);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    /// A start that fails with another error returns it at once, without a wait, for
    /// plain and scoped starts.
    #[test]
    fn another_error_fails_at_once() {
        let failures = fail_starts(2, Failure::Os(12));
        let error = spawn("kizz-test", "the test thread", || ()).unwrap_err();
        assert_eq!(error.attempts(), 1);
        assert_eq!(error.waited(), Duration::ZERO);
        assert_eq!(error.error().raw_os_error(), Some(12));
        assert!(
            error
                .to_string()
                .starts_with("could not start the test thread after 1 attempt in 0.0 seconds: "),
            "{error}"
        );
        let scoped = thread::scope(|scope| {
            spawn_scoped(scope, "kizz-test", "the test thread", || ())
                .map(|_| ())
                .unwrap_err()
                .attempts()
        });
        assert_eq!(scoped, 1);
        assert_eq!(failures.waits(), Vec::<Duration>::new());
    }

    /// Failures reach the thread that set them and the threads that it starts
    /// through the helper, which share their count, and no other thread, such as one
    /// that runs another test at the same time.
    #[test]
    fn failures_reach_only_their_thread_and_its_helper_threads() {
        let failures = fail_starts(1, Failure::Busy);
        // A thread started without the helper, like another test's, never meets them.
        let other = thread::spawn(|| {
            spawn("kizz-test", "the other thread", || ())
                .map(|handle| handle.join().is_ok())
                .unwrap()
        });
        assert!(other.join().unwrap());
        assert_eq!(failures.remaining(), 1);
        // A thread that this one starts through the helper meets them in its own
        // starts.
        let inner = spawn("kizz-test", "the outer thread", || {
            spawn("kizz-test", "the inner thread", || 3)
                .unwrap()
                .join()
                .unwrap()
        });
        // The outer start met the one failure, so the inner one met none.
        assert_eq!(inner.unwrap().join().unwrap(), 3);
        assert_eq!(failures.remaining(), 0);
        assert_eq!(failures.waits(), [WAITS[0]]);
        drop(failures);
        // A thread started through the helper takes on the failures of the thread
        // that started it: only the inner thread is named so that it meets them.
        let failures = fail_starts_named(2, Failure::Busy, "kizz-inner");
        let nested = spawn("kizz-outer", "the outer thread", || {
            spawn("kizz-inner", "the inner thread", || 5)
                .map(|handle| handle.join().unwrap_or_default())
        })
        .unwrap()
        .join()
        .unwrap()
        .unwrap();
        assert_eq!(nested, 5);
        assert_eq!(failures.remaining(), 0);
        assert_eq!(failures.waits(), WAITS[..2]);
    }

    /// Failures set for one thread name leave the starts of other names alone.
    #[test]
    fn failures_for_one_name_leave_other_names_alone() {
        let failures = fail_starts_named(3, Failure::Busy, "kizz-chosen");
        spawn("kizz-other", "another thread", || ())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(failures.remaining(), 3);
        spawn("kizz-chosen", "the chosen thread", || ())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(failures.remaining(), 0);
        assert_eq!(failures.waits(), WAITS[..3]);
    }

    /// Dropping the failures gives the thread back those it had before.
    #[test]
    fn dropping_failures_restores_the_earlier_ones() {
        let outer = fail_starts(1, Failure::Busy);
        drop(fail_starts(5, Failure::Os(12)));
        spawn("kizz-test", "the test thread", || ())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(outer.remaining(), 0);
        assert_eq!(outer.waits(), [WAITS[0]]);
    }

    /// A name that holds a NUL byte is refused without an attempt, where
    /// `std::thread::Builder` would panic.
    #[test]
    fn a_name_with_a_nul_byte_is_refused() {
        let error = spawn("kizz\0test", "the test thread", || ()).unwrap_err();
        assert_eq!(error.attempts(), 0);
        assert_eq!(error.error().kind(), std::io::ErrorKind::InvalidInput);
        let scoped = thread::scope(|scope| {
            spawn_scoped(scope, "kizz\0test", "the test thread", || ())
                .map(|_| ())
                .unwrap_err()
                .attempts()
        });
        assert_eq!(scoped, 0);
    }

    /// The waits of the retry rule add up to 22.3 seconds.
    #[test]
    fn the_waits_add_up_to_22_3_seconds() {
        assert_eq!(
            WAITS.iter().sum::<Duration>(),
            Duration::from_millis(22_300)
        );
    }
}
