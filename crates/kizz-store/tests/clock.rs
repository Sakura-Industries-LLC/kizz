// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The direct clock of kizz's patched copy of `SurrealKV`, which the store opens the
//! database with: it reads the system clock on every reading, starts no thread, and
//! every reading is greater than the one before.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: threads of a scope read the clock at the same time, and the tests \
              assert, lock and print"
)]

use std::{
    collections::HashSet,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use surrealkv::{ClockMode, LogicalClock, Options};

/// How many readings each thread takes.
const READINGS: usize = 20_000;

/// How many threads read the clock at the same time.
const THREADS: usize = 8;

/// Each reading of the clock that the public option selects is greater than the
/// readings before it on its thread, on one thread and on several threads at once,
/// no two readings are equal, and a reading is the system clock's time, in
/// nanoseconds since the Unix epoch.
#[test]
fn the_direct_clock_rises_on_every_reading() {
    let clock = Options::new_with_clock(ClockMode::Direct).clock();
    let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let first = clock.now();
    let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    assert!(
        u128::from(first) >= before.as_nanos(),
        "{first} is before {before:?}"
    );
    assert!(
        u128::from(first) <= after.as_nanos().saturating_add(1_000_000),
        "{first} is after {after:?}"
    );

    let mut last = first;
    for _ in 0..READINGS {
        let now = clock.now();
        assert!(now > last, "{now} does not follow {last}");
        last = now;
    }

    let clock: &dyn LogicalClock = clock.as_ref();
    let readings: Vec<Vec<u64>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| scope.spawn(|| (0..READINGS).map(|_| clock.now()).collect::<Vec<u64>>()))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    for taken in &readings {
        assert_eq!(taken.len(), READINGS);
        assert!(
            taken.windows(2).all(|pair| pair[0] < pair[1]),
            "a thread's readings do not rise"
        );
        assert!(taken.iter().all(|&reading| reading > last));
    }
    let distinct: HashSet<u64> = readings.iter().flatten().copied().collect();
    assert_eq!(distinct.len(), THREADS * READINGS, "two readings are equal");
}
