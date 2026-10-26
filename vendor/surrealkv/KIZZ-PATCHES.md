# Kizz's patch to SurrealKV 1.0.0

This directory is SurrealKV 1.0.0 as published on crates.io, without Cargo's `.cargo-ok` marker, with the changes below.
Kizz builds it in place of the published crate through `[patch.crates-io]`.
The patch lets a tree run without a clock thread, and keeps SurrealKV from panicking when a thread cannot start.
Each change is listed by file and by its lines in this copy, with the lines of 1.0.0 that it replaces, and with its reason.
The patch is whole, so it can be offered upstream as it is.

## A clock without a thread

The default logical clock starts a thread that resyncs it with the system clock once a second.
`Options::default()` builds that clock, and `TreeBuilder::new()` and `..Default::default()` build those options.
So an option set on the options afterwards would come after the thread had already started.

* `src/clock.rs` lines 17 through 27 add the public enum `ClockMode`.
    * `ClockMode::Threaded`, the default, is the clock of 1.0.0.
    * `ClockMode::Direct` has no thread: every reading reads the system clock.
* `src/clock.rs` lines 51 through 53 add the field `direct` to the clock's inner state.
    * When it is set, `current_time_ns` reads the system clock directly (lines 137 through 140).
    * `now` still makes each reading greater than the one before, so the clock keeps the promise of `LogicalClock`.
* `src/clock.rs` lines 82 through 117 replace lines 67 through 86 of 1.0.0.
    * `DefaultLogicalClock::new` keeps its behavior and builds the clock with its thread.
    * The new `DefaultLogicalClock::direct` builds the clock with `direct` set, and never starts the thread.
    * Both go through the private `with_thread`, which starts the thread only for `new`.
* `src/lib.rs` lines 331 through 345 replace lines 329 through 332 of 1.0.0.
    * The new public `Options::new_with_clock` builds the default options with the clock that a `ClockMode` selects.
    * `Options::default()` calls it with `ClockMode::Threaded`, so the defaults do not change.
    * With `ClockMode::Direct`, no clock thread starts at any point, not even one that stops at once.
* `src/lib.rs` lines 394 through 398 add the public `Options::clock`, which returns the clock of the options.
    * A caller can then read the clock that its option selects, as kizz's test of the direct clock does.
* `src/lib.rs` lines 58 and 59 replace line 58 of 1.0.0, and export `ClockMode` and `LogicalClock`.
    * `LogicalClock` is exported so that a caller can read the clock that `Options::clock` returns.

## A threaded clock that never panics

* `src/clock.rs` line 172 replaces line 134 of 1.0.0.
    * The thread starts with `std::thread::Builder::spawn`, which returns an error, instead of `std::thread::spawn`.
    * `std::thread::spawn` panics when the system cannot start a thread, for example with `EAGAIN`.
* `src/clock.rs` lines 185 through 196 replace lines 147 and 148 of 1.0.0.
    * When the thread cannot start, the clock sets `direct` and reads the system clock directly from then on.
    * The `unwrap` of the lock at line 148 becomes a lock that takes the value even when the mutex is poisoned.
* `src/clock.rs` lines 154 through 159 replace lines 119 through 121 of 1.0.0, in `shutdown`, which `Drop` calls.
    * The `unwrap` of the lock at line 119 becomes a lock that takes the value even when the mutex is poisoned.
    * The `unwrap` of the join at line 121 is dropped: the thread returns nothing, and a panic of it is no reason to panic again.
* `src/clock.rs` line 144 replaces line 109 of 1.0.0, and adds with `saturating_add`.
* `src/clock.rs` line 218 replaces line 170 of 1.0.0, and adds with `saturating_add`.
* `src/clock.rs` lines 209 and 237 replace lines 161 and 189 of 1.0.0.
    * The spin counter is a `u32` and counts with `saturating_add`.
    * The three additions overflowed with a panic in builds with overflow checks, such as test builds.
* `src/clock.rs` line 3 replaces line 3 of 1.0.0, and imports `PoisonError` for the locks.

## Write pacing that starts no thread

* `src/stall.rs` lines 46 through 56 replace lines 46 through 51 of 1.0.0.
    * `pace` waited for its delay on a thread of Tokio's blocking pool, through `tokio::task::spawn_blocking`.
    * Tokio panics when its blocking pool cannot start a thread while the pool has none.
    * The new `pace` yields to the runtime until the delay has passed, and starts no thread, whatever the runtime.
    * It needs no time driver either, which the comment of 1.0.0 gave as the reason for the blocking pool.
    * The delay is a few hundred microseconds at most.

## Log replay when a thread cannot start

* `src/wal/parallel_recovery.rs` lines 104 through 118 replace lines 104 through 113 of 1.0.0, in `decode_sealed_segments`.
    * The thread that decodes several segments starts with `std::thread::Builder::spawn` instead of `std::thread::spawn`.
    * When it cannot start, the segments are decoded on the calling thread, in order, as for one segment.
* `src/wal/parallel_recovery.rs` lines 193 through 207 replace lines 188 through 197 of 1.0.0, in `replay_segments_sync`.
    * The thread that replays several segments starts with `std::thread::Builder::spawn` instead of `std::thread::spawn`.
    * When it cannot start, the segments are replayed on the calling thread with the new `replay_segments_in_order`.
* `src/wal/parallel_recovery.rs` lines 211 through 225 add `replay_segments_in_order`.
    * It decodes the segments in order, stopping at the first error, as `replay_segments_parallel` awaits them.
    * It then fills the memtables with `populate_memtables`, so its result is that of `replay_segments_parallel`.
* `src/wal/parallel_recovery.rs` lines 253 through 262 are inserted after line 227 of 1.0.0.
    * They end `replay_segments_parallel` with a call of the new `populate_memtables`, which both replays share.
    * `populate_memtables` is the second half of `replay_segments_parallel` of 1.0.0, from line 229 on, unchanged.
* The decoding and the replay keep `tokio::task::block_in_place` on a multi-thread runtime, as in 1.0.0.

## What kizz relies on

* Kizz opens its tree with `ClockMode::Direct`, on a current-thread Tokio runtime, which starts no thread.
* Kizz sets the stall thresholds to `usize::MAX`, so pacing never runs, and the patched `pace` covers any other caller.
* `affinitypool::spawn` runs its work on the calling thread when no global pool exists, and neither kizz nor SurrealKV's
  production code installs one.
* Nothing else in this copy differs from SurrealKV 1.0.0.
