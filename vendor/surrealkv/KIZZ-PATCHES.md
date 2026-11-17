# Kizz's patch to SurrealKV 1.4.0

This directory is SurrealKV 1.4.0 as published on crates.io, without Cargo's `.cargo-ok` marker, with the changes below.
Kizz builds it in place of the published crate through `[patch.crates-io]`.
The patch lets a tree run without a clock thread, and keeps SurrealKV from panicking when a thread cannot start.
Each change is listed by file and by its lines in this copy, with the lines of 1.4.0 that it replaces, and with its reason.
The patch is whole, so it can be offered upstream as it is.
It was first written against SurrealKV 1.0.0 and carried to 1.4.0 with the same behavior.

## A clock without a thread

The default logical clock starts a thread that resyncs it with the system clock once a second.
`Options::default()` builds that clock, and `TreeBuilder::new()` and `..Default::default()` build those options.
So an option set on the options afterwards would come after the thread had already started.

* `src/clock.rs` lines 20 through 30 add the public enum `ClockMode`.
    * `ClockMode::Threaded`, the default, is the clock of 1.4.0.
    * `ClockMode::Direct` has no thread: every reading reads the system clock.
* `src/clock.rs` lines 56 through 58 add the field `direct` to the clock's inner state.
    * When it is set, `current_time_ns` reads the system clock directly (lines 140 through 143).
    * `now` still makes each reading greater than the one before, so the clock keeps the promise of `LogicalClock`.
* `src/clock.rs` lines 87 through 122 replace lines 72 through 91 of 1.4.0.
    * `DefaultLogicalClock::new` keeps its behavior and builds the clock with its thread.
    * The new `DefaultLogicalClock::direct` builds the clock with `direct` set, and never starts the thread.
    * Both go through the private `with_thread`, which starts the thread only for `new`.
* `src/lib.rs` lines 422 through 436 replace line 423 of 1.4.0, and wrap the body of `Options::default()`.
    * The new public `Options::new_with_clock` builds the default options with the clock that a `ClockMode` selects.
    * `Options::default()` calls it with `ClockMode::Threaded`, so the defaults do not change.
    * With `ClockMode::Direct`, no clock thread starts at any point, not even one that stops at once.
* `src/lib.rs` lines 492 through 496 add the public `Options::clock`, which returns the clock of the options.
    * A caller can then read the clock that its option selects, as kizz's test of the direct clock does.
* `src/lib.rs` lines 87 and 88 replace line 87 of 1.4.0, and export `ClockMode` and `LogicalClock`.
    * `LogicalClock` is exported so that a caller can read the clock that `Options::clock` returns.

## A threaded clock that never panics

* `src/clock.rs` line 184 replaces line 146 of 1.4.0.
    * The thread starts with `std::thread::Builder::spawn`, which returns an error, instead of `std::thread::spawn`.
    * `std::thread::spawn` panics when the system cannot start a thread, for example with `EAGAIN`.
* `src/clock.rs` lines 197 through 208 replace lines 159 and 160 of 1.4.0.
    * When the thread cannot start, the clock sets `direct` and reads the system clock directly from then on.
    * The `unwrap` of the lock at line 160 becomes a lock that takes the value even when the mutex is poisoned.
* `src/clock.rs` lines 166 through 172 replace lines 131 through 133 of 1.4.0, in `shutdown`, which `Drop` calls.
    * The `unwrap` of the lock at line 131 becomes a lock that takes the value even when the mutex is poisoned.
    * The `unwrap` of the join at line 133 is dropped: the thread returns nothing, and a panic of it is no reason to panic again.
* `src/clock.rs` line 147 replaces line 112 of 1.4.0, and adds with `saturating_add`.
* `src/clock.rs` line 230 replaces line 182 of 1.4.0, and adds with `saturating_add`.
* `src/clock.rs` line 243 replaces line 195 of 1.4.0, and counts the spins with `saturating_add`.
    * The additions overflowed with a panic in builds with overflow checks, such as test builds.
* `src/clock.rs` line 3 replaces line 3 of 1.4.0, and imports `PoisonError` for the locks.

## Write pacing that starts no thread

In 1.4.0 the pacing wait moved from `src/stall.rs` to the runtime shim in `src/rt.rs`.

* `src/rt.rs` lines 112 through 125 replace lines 112 through 120 of 1.4.0, the native `pace`.
    * `pace` waited for its delay on a thread of Tokio's blocking pool, through `tokio::task::spawn_blocking`.
    * Tokio panics when its blocking pool cannot start a thread while the pool has none.
    * The new `pace` yields to the runtime until the delay has passed, and starts no thread, whatever the runtime.
    * It needs no time driver either, which the comment of 1.4.0 gives as the reason for the blocking pool.
    * The delay is a few hundred microseconds at most.

## Log replay when a thread cannot start

* `src/wal/parallel_recovery.rs` lines 114 through 129 replace lines 114 through 124 of 1.4.0, in `decode_sealed_segments`.
    * The thread that decodes several segments starts with `std::thread::Builder::spawn` instead of `std::thread::spawn`.
    * When it cannot start, the segments are decoded on the calling thread, in order, as for one segment.
* `src/wal/parallel_recovery.rs` lines 180 through 195 replace lines 175 through 185 of 1.4.0, in `replay_segments_sync`.
    * The thread that replays several segments starts with `std::thread::Builder::spawn` instead of `std::thread::spawn`.
    * When it cannot start, the segments are replayed on the calling thread with `replay_segments_serial`.
* `src/wal/parallel_recovery.rs` lines 199 through 203 replace lines 189 through 194 of 1.4.0.
    * They drop the `cfg` that compiled `replay_segments_serial` only for tests and wasm32, so the fallback above has it.
    * 1.4.0 wrote `replay_segments_serial` for targets without threads, and its test
      `the_serial_replay_matches_the_parallel_replay` holds its result to that of `replay_segments_parallel`.
    * It takes the place of the `replay_segments_in_order` and `populate_memtables` that the 1.0.0 patch added.
* The decoding and the replay keep `tokio::task::block_in_place` on a multi-thread runtime, as in 1.4.0.

## What kizz relies on

* Kizz opens its tree with `ClockMode::Direct`, on a current-thread Tokio runtime, which starts no thread.
* Kizz sets the stall thresholds to `usize::MAX`, so pacing never runs, and the patched `pace` covers any other caller.
* `affinitypool::spawn`, which 1.4.0 also reaches as `rt::run_blocking`, runs its work on the calling thread when no
  global pool exists, and neither kizz nor SurrealKV's production code installs one.
* The other thread starts in 1.4.0 are in test code only.
* Nothing else in this copy differs from SurrealKV 1.4.0.
