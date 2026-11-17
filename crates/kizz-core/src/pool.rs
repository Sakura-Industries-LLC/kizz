// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The worker pool of a command ("Kizz never panics").
//!
//! A command that has parallel work builds one [`WorkerPool`] before that work, and
//! passes it to the code that needs it: a `rayon` pool of as many threads as
//! [`std::thread::available_parallelism`] gives, at most [`MAX_WORKERS`], and 1 when
//! it gives an error. Each worker starts through `kizz_threads`, which waits and
//! retries while the system cannot start a thread for the moment, and never panics;
//! a worker that still cannot start makes [`WorkerPool::new`] return the error, and
//! `rayon` stops the workers that started.
//!
//! The cache check, the recording of observations, cache cleanup, BLAKE3's parallel
//! hashing and the discovery walk run inside the pool, through
//! [`WorkerPool::install`], so BLAKE3's `rayon::join` runs on it too, and `rayon`'s
//! global pool, which panics when it cannot start a thread, never starts.

use std::{io, sync::Arc, thread};

use rayon::{ThreadPool, ThreadPoolBuildError, ThreadPoolBuilder};

/// The most threads a worker pool has. The measurement report times the check of
/// 10,000 cached steps at 0.73 seconds with 4 threads, 0.83 seconds with 1, and 1.53
/// seconds with the default pool of 32 threads.
pub const MAX_WORKERS: usize = 4;

/// The pool of threads that runs a command's parallel work.
#[derive(Debug, Clone)]
pub struct WorkerPool {
    /// The `rayon` pool, shared by every clone.
    pool: Arc<ThreadPool>,
}

/// A worker pool that could not be built because a worker could not start.
#[derive(Debug, thiserror::Error)]
#[error("cannot start kizz's worker pool: {0}")]
pub struct PoolError(ThreadPoolBuildError);

impl WorkerPool {
    /// A pool of as many threads as [`std::thread::available_parallelism`] gives, at
    /// most [`MAX_WORKERS`], and 1 when it gives an error.
    ///
    /// # Errors
    ///
    /// A [`PoolError`] that names the worker and the system's error when a worker
    /// cannot start after its retries.
    pub fn new() -> Result<Self, PoolError> {
        let threads =
            thread::available_parallelism().map_or(1, |threads| threads.get().min(MAX_WORKERS));
        Self::with_threads(threads)
    }

    /// A pool of `threads` threads, at least 1.
    ///
    /// # Errors
    ///
    /// A [`PoolError`] that names the worker and the system's error when a worker
    /// cannot start after its retries.
    pub fn with_threads(threads: usize) -> Result<Self, PoolError> {
        ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|index| format!("kizz-worker-{index}"))
            .spawn_handler(|worker| {
                let name = worker.name().unwrap_or("kizz-worker").to_owned();
                kizz_threads::spawn(&name, "a worker of kizz's pool", move || worker.run())
                    .map(|_detached| ())
                    .map_err(io::Error::other)
            })
            .build()
            .map(|pool| {
                Self {
                    pool: Arc::new(pool),
                }
            })
            .map_err(PoolError)
    }

    /// The number of threads of the pool.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Run `work` on the pool, so that the parallel work it starts runs on the pool,
    /// and return its result. On a thread of the pool, `work` runs at once.
    pub fn install<R: Send>(
        &self,
        work: impl FnOnce() -> R + Send,
    ) -> R {
        self.pool.install(work)
    }
}

/// Unit tests of building the pool, with the failures of `kizz_threads::testing`.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use kizz_threads::testing::{Failure, fail_starts};
    use rayon::iter::{IntoParallelIterator as _, ParallelIterator as _};

    use super::*;

    /// A pool whose workers cannot start, even after their retries, is an error that
    /// names the worker and error 11.
    #[test]
    fn a_pool_whose_workers_cannot_start_is_an_error() {
        let _failures = fail_starts(u32::MAX, Failure::Busy);
        let message = WorkerPool::with_threads(2).unwrap_err().to_string();
        assert!(
            message.starts_with(
                "cannot start kizz's worker pool: could not start a worker of kizz's pool after \
                 11 attempts in 22.3 seconds: "
            ),
            "{message}"
        );
        assert!(message.ends_with("(os error 11)"), "{message}");
    }

    /// A pool whose workers start after retries runs parallel work.
    #[test]
    fn a_pool_whose_workers_start_after_retries_runs_parallel_work() {
        let failures = fail_starts(3, Failure::Busy);
        let pool = WorkerPool::with_threads(2).unwrap();
        assert_eq!(failures.remaining(), 0);
        assert_eq!(pool.threads(), 2);
        let sum: u64 = pool.install(|| (1..=1_000_u64).into_par_iter().sum());
        assert_eq!(sum, 500_500);
    }

    /// The pool has at least one thread and at most [`MAX_WORKERS`].
    #[test]
    fn the_pool_has_one_to_four_threads() {
        assert!((1..=MAX_WORKERS).contains(&WorkerPool::new().unwrap().threads()));
        assert_eq!(WorkerPool::with_threads(0).unwrap().threads(), 1);
    }
}
