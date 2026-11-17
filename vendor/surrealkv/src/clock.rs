use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::RwLock;

use crate::ring::sync::backoff;
use crate::rt::{self, Instant};

/// Defines the logical clock that SurrealKV will use to measure
/// commit timestamps and retention periods checks.
pub trait LogicalClock: Debug + Send + Sync {
	/// Returns the current logical timestamp.
	/// Guarantees monotonicity: each call returns a value > previous calls.
	fn now(&self) -> u64;
}

/// Selects how the logical clock of a tree reads the time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClockMode {
	/// A background thread resyncs the clock with the system clock once a
	/// second, and readings in between use the monotonic clock. If the thread
	/// cannot start, the clock reads the system clock directly.
	#[default]
	Threaded,
	/// No thread: every reading reads the system clock directly.
	Direct,
}

/// A high-performance logical clock that combines wall-clock time with monotonicity.
///
/// Uses a background thread to periodically sync with system time,
/// and fast monotonic `Instant::now()` for high-performance time queries
/// between syncs to avoid expensive syscalls on every commit.
pub struct DefaultLogicalClock {
	/// The inner strcuture of this clock
	inner: Arc<DefaultLogicalClockInner>,
}

impl Drop for DefaultLogicalClock {
	fn drop(&mut self) {
		self.shutdown();
	}
}

/// The inner structure of the timestamp oracle
pub struct DefaultLogicalClockInner {
	/// The latest monotonic counter for this oracle
	timestamp: AtomicU64,
	/// Reference time when this clock was last synced with system clock. Not read on a browser,
	/// where the clock reads the wall clock on every call.
	#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), allow(dead_code))]
	reference: RwLock<(u64, Instant)>,
	/// Whether the clock reads the system clock directly on every reading,
	/// because it has no syncing thread
	direct: AtomicBool,
	/// Specifies whether timestamp syncing is enabled in the background
	#[cfg_attr(target_family = "wasm", allow(dead_code))]
	resync_enabled: AtomicBool,
	/// Stores a handle to the current timestamp syncing background thread
	#[cfg_attr(target_family = "wasm", allow(dead_code))]
	resync_handle: Mutex<Option<JoinHandle<()>>>,
	/// Interval at which the clock resyncs with the system clock
	#[cfg_attr(target_family = "wasm", allow(dead_code))]
	resync_interval: Duration,
}

impl Default for DefaultLogicalClock {
	fn default() -> Self {
		Self::new()
	}
}

impl std::fmt::Debug for DefaultLogicalClock {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DefaultLogicalClock")
			.field("current_timestamp", &self.current_timestamp())
			.field("current_unix_ns", &Self::current_unix_ns())
			.field("current_time_ns", &self.current_time_ns())
			.finish()
	}
}

impl DefaultLogicalClock {
	pub fn new() -> Self {
		// Return a clock with a resyncing thread
		Self::with_thread(true)
	}

	/// Returns a clock without a background thread, which reads the system
	/// clock directly on every reading, and is still monotonic
	pub fn direct() -> Self {
		// Return a clock that never starts a resyncing thread
		Self::with_thread(false)
	}

	/// Returns a clock, which starts its resyncing thread if `thread` is set
	fn with_thread(thread: bool) -> Self {
		// Get the current unix time in nanoseconds
		let reference_unix = Self::current_unix_ns();
		// Get a new monotonically increasing clock
		let reference_time = Instant::now();
		// Return the current timestamp clock
		let clock = Self {
			inner: Arc::new(DefaultLogicalClockInner {
				timestamp: AtomicU64::new(reference_unix),
				reference: RwLock::new((reference_unix, reference_time)),
				direct: AtomicBool::new(!thread),
				resync_enabled: AtomicBool::new(thread),
				resync_handle: Mutex::new(None),
				resync_interval: Duration::from_secs(1),
			}),
		};
		// Start up the resyncing thread
		if thread {
			clock.spawn_clock_sync();
		}
		// Return the clock
		clock
	}

	/// Returns the current timestamp for this oracle
	#[inline]
	pub fn current_timestamp(&self) -> u64 {
		self.inner.timestamp.load(Ordering::Acquire)
	}

	/// Gets the current system time in nanoseconds since the Unix epoch
	#[inline]
	pub(crate) fn current_unix_ns() -> u64 {
		rt::now_unix_ns()
	}

	/// Gets the current estimated time in nanoseconds since the Unix epoch
	#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
	#[inline]
	pub(crate) fn current_time_ns(&self) -> u64 {
		// Read the system clock when there is no syncing thread
		if self.inner.direct.load(Ordering::Acquire) {
			return Self::current_unix_ns();
		}
		// Get the current reference time
		let reference = *self.inner.reference.read();
		// Calculate the nanoseconds since the Unix epoch
		reference.0.saturating_add(reference.1.elapsed().as_nanos() as u64)
	}

	/// Gets the current time in nanoseconds since the Unix epoch. There is no thread that
	/// resyncs a reference time in a browser, so the wall clock is read on every call, which is
	/// cheap there; `now` makes the result strictly increasing.
	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	#[inline]
	pub(crate) fn current_time_ns(&self) -> u64 {
		rt::now_unix_ns()
	}

	/// Shutdown the oracle resync, waiting for background threads to exit
	fn shutdown(&self) {
		#[cfg(not(target_family = "wasm"))]
		{
			// Disable timestamp resyncing
			self.inner.resync_enabled.store(false, Ordering::Release);
			// Take the thread handle and unpark the thread
			let handle =
				self.inner.resync_handle.lock().unwrap_or_else(PoisonError::into_inner).take();
			if let Some(handle) = handle {
				handle.thread().unpark();
				// The thread only reads clocks, and its result is nothing
				let _ = handle.join();
			}
		}
	}

	/// Start the resyncing thread after creating the oracle
	#[cfg(not(target_family = "wasm"))]
	fn spawn_clock_sync(&self) {
		// Clone the underlying clock inner
		let inner = Arc::clone(&self.inner);
		// Store the resync interval for the thread
		let interval = inner.resync_interval;
		// Spawn a new thread to handle timestamp resyncing
		let spawned = std::thread::Builder::new().spawn(move || {
			// Check whether the timestamp resync process is enabled
			while inner.resync_enabled.load(Ordering::Acquire) {
				// Wait for a specified time interval
				std::thread::park_timeout(interval);
				// Get the current unix time in nanoseconds
				let reference_unix = Self::current_unix_ns();
				// Get a new monotonically increasing clock
				let reference_time = Instant::now();
				// Store the timestamp and monotonic instant
				*inner.reference.write() = (reference_unix, reference_time);
			}
		});
		match spawned {
			// Store and track the thread handle
			Ok(handle) => {
				*self.inner.resync_handle.lock().unwrap_or_else(PoisonError::into_inner) =
					Some(handle)
			}
			// Without the thread, read the system clock directly
			Err(_) => {
				self.inner.resync_enabled.store(false, Ordering::Release);
				self.inner.direct.store(true, Ordering::Release);
			}
		}
	}

	#[cfg(target_family = "wasm")]
	fn spawn_clock_sync(&self) {
		// Background threads not supported on WASM
	}
}

impl LogicalClock for DefaultLogicalClock {
	#[inline]
	fn now(&self) -> u64 {
		// Store the number of spins
		let mut spins = 0usize;
		// Loop until we reach the next incremental timestamp
		loop {
			// Get current time estimate
			let mut version = self.current_time_ns();
			// Ensure monotonicity via compare-and-swap loop
			let current = self.inner.timestamp.load(Ordering::Acquire);
			// Ensure version is greater than last timestamp
			if version <= current {
				version = current.saturating_add(1);
			}
			// Try to update the timestamp
			if self
				.inner
				.timestamp
				.compare_exchange_weak(current, version, Ordering::AcqRel, Ordering::Acquire)
				.is_ok()
			{
				return version;
			}
			// Ensure the thread backs off when under contention
			backoff(spins);
			spins = spins.saturating_add(1);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_default_clock_immediate_shutdown() {
		// Test that the clock shuts down quickly without waiting for full resync
		// interval
		for _ in 0..5 {
			let start = Instant::now();
			{
				let _clock = DefaultLogicalClock::new();
				// Clock is dropped here
			}
			let elapsed = start.elapsed();

			// Should drop in well under 100ms (the resync interval)
			// Allow 50ms for slow CI systems
			assert!(
				elapsed < Duration::from_millis(50),
				"Clock took too long to drop: {:?}. Thread may not be shutting down properly.",
				elapsed
			);
		}
	}

	#[test]
	fn test_default_clock_monotonicity() {
		let clock = DefaultLogicalClock::new();

		let mut last_ts = clock.now();
		for _ in 0..100 {
			let current_ts = clock.now();
			assert!(current_ts >= last_ts, "Clock is not monotonic: {} < {}", current_ts, last_ts);
			last_ts = current_ts;
		}
	}

	#[test]
	fn test_default_clock_progresses() {
		let clock = DefaultLogicalClock::new();
		let ts1 = clock.now();

		std::thread::sleep(Duration::from_millis(10));

		let ts2 = clock.now();
		assert!(ts2 > ts1, "Clock did not progress over time: {} >= {}", ts1, ts2);
	}

	#[test]
	fn test_default_clock_concurrent_monotonicity() {
		use std::sync::Mutex as StdMutex;

		let clock = Arc::new(DefaultLogicalClock::new());
		let all_values = Arc::new(StdMutex::new(Vec::new()));
		let mut handles = vec![];

		// Spawn 10 threads, each getting 100 timestamps
		for i in 0..10 {
			let clock = Arc::clone(&clock);
			let all_values = Arc::clone(&all_values);

			let handle = std::thread::spawn(move || {
				for _ in 0..100 {
					let ts = clock.now();
					all_values.lock().unwrap().push((i, ts));
				}
			});

			handles.push(handle);
		}

		// Wait for all threads
		for handle in handles {
			handle.join().unwrap();
		}

		// Check that all values are unique and monotonically increasing when sorted
		let mut values = all_values.lock().unwrap();
		values.sort_by_key(|(_, ts)| *ts);

		let mut last = 0;
		for (thread_id, ts) in values.iter() {
			assert!(
				*ts > last,
				"Clock not monotonic: thread {} got {}, but previous was {}",
				thread_id,
				ts,
				last
			);
			last = *ts;
		}
	}

	#[test]
	fn test_default_clock_no_duplicates() {
		use std::collections::HashSet;

		let clock = Arc::new(DefaultLogicalClock::new());
		let seen = Arc::new(Mutex::new(HashSet::new()));
		let mut handles = vec![];

		// Spawn multiple threads to detect any duplicate timestamps
		for _ in 0..5 {
			let clock = Arc::clone(&clock);
			let seen = Arc::clone(&seen);

			let handle = std::thread::spawn(move || {
				for _ in 0..1000 {
					let ts = clock.now();
					let mut seen_guard = seen.lock().unwrap();
					assert!(seen_guard.insert(ts), "Duplicate timestamp: {}", ts);
				}
			});

			handles.push(handle);
		}

		for handle in handles {
			handle.join().unwrap();
		}
	}

	#[test]
	fn test_default_clock_stays_close_to_wall_time() {
		let clock = DefaultLogicalClock::new();

		// Get a timestamp
		let ts1 = clock.now();
		let wall1 = DefaultLogicalClock::current_unix_ns();

		// Clock should be within 1 second of wall time
		let diff1 = ts1.abs_diff(wall1);
		assert!(diff1 < 1_000_000_000, "Clock too far from wall time: {} ns difference", diff1);

		// Even after many calls under contention
		for _ in 0..10000 {
			clock.now();
		}

		let ts2 = clock.now();
		let wall2 = DefaultLogicalClock::current_unix_ns();
		let diff2 = ts2.abs_diff(wall2);

		// Should still be reasonably close (within 100ms even with forced increments)
		assert!(
			diff2 < 100_000_000,
			"Clock drifted too far from wall time: {} ns difference",
			diff2
		);
	}
}
