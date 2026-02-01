// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Deterministic clock for simulation testing.
//!
//! This module provides clocks for deterministic simulation testing:
//!
//! - [`SimulationClock`]: Manually advanced clock with explicit time control
//! - [`TokioClock`]: Uses `tokio::time::Instant` for automatic paused time integration
//!
//! # Tokio Integration
//!
//! When using `#[restate_core::test(start_paused = true)]`, tokio's time is paused.
//!
//! [`TokioClock`] automatically synchronizes with tokio's paused time by using
//! `tokio::time::Instant`. When tokio time is paused:
//! - `tokio::time::Instant::now()` returns a frozen instant initially
//! - When time auto-advances or is advanced via `tokio::time::advance()`,
//!   subsequent `Instant::now()` calls reflect that advancement
//!
//! [`SimulationClock`] provides explicit control via `advance_to_async` which
//! calls `tokio::time::advance()` to advance tokio's paused time.
//!
//! Both clocks update the global [`WallClock`] cache so that
//! `MillisSinceEpoch::now()` returns the simulated time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use restate_clock::{Clock, WallClock};
use restate_types::time::MillisSinceEpoch;

/// A clock that uses `tokio::time::Instant` for automatic paused time integration.
///
/// When tokio time is paused (`start_paused = true`), this clock automatically
/// tracks the simulated time as tokio advances it (either via auto-advance
/// when blocked on timers, or via explicit `tokio::time::advance()` calls).
///
/// This is useful for code that needs a `Clock` implementation that automatically
/// stays synchronized with tokio's paused time.
///
/// # Example
///
/// ```ignore
/// #[tokio::test(start_paused = true)]
/// async fn test_with_tokio_clock() {
///     let clock = TokioClock::new();
///
///     // Initial time is BASE_TIME
///     let t0 = clock.now();
///
///     // After a tokio sleep, clock reflects elapsed time
///     tokio::time::sleep(Duration::from_secs(5)).await;
///     let t1 = clock.now();
///
///     assert_eq!(t1.as_u64() - t0.as_u64(), 5000);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct TokioClock {
    initial_instant: tokio::time::Instant,
}

impl TokioClock {
    /// Fixed base time for deterministic tests (well after RESTATE_EPOCH).
    /// This is Nov 14, 2023 22:13:20 UTC.
    pub const BASE_TIME: MillisSinceEpoch = MillisSinceEpoch::new(1_700_000_000_000);

    /// Creates a new `TokioClock` starting from the current tokio instant.
    ///
    /// Also initializes the global WallClock cache to BASE_TIME so that
    /// `MillisSinceEpoch::now()` returns simulated time.
    pub fn new() -> Self {
        // Initialize WallClock with the base simulated time
        WallClock::set_recent(Self::BASE_TIME);

        Self {
            initial_instant: tokio::time::Instant::now(),
        }
    }

    /// Returns the elapsed time since this clock was created.
    pub fn elapsed(&self) -> Duration {
        tokio::time::Instant::now().duration_since(self.initial_instant)
    }

    /// Updates the global WallClock cache to reflect the current simulated time.
    ///
    /// Call this after tokio time advances to ensure `MillisSinceEpoch::now()`
    /// returns the correct simulated time.
    pub fn sync_wallclock(&self) {
        WallClock::set_recent(self.now());
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TokioClock {
    fn recent(&self) -> MillisSinceEpoch {
        let elapsed = tokio::time::Instant::now().duration_since(self.initial_instant);
        Self::BASE_TIME + elapsed
    }

    fn now(&self) -> MillisSinceEpoch {
        self.recent()
    }
}

/// A deterministic clock for simulation testing.
///
/// This clock can be advanced manually, providing full control over time progression
/// in the simulation. The clock is thread-safe and can be shared across components.
///
/// # Important
///
/// When used with tokio paused time (`start_paused = true`), call the async methods
/// (`advance_async`, `advance_to_async`) to keep tokio time synchronized.
#[derive(Debug, Clone)]
pub struct SimulationClock {
    inner: Arc<SimulationClockInner>,
}

#[derive(Debug)]
struct SimulationClockInner {
    /// Current time in milliseconds since the Restate epoch
    current_time: AtomicU64,
}

impl SimulationClock {
    /// Default base time for simulation (well after RESTATE_EPOCH).
    /// This is Nov 14, 2023 22:13:20 UTC - same as TokioClock::BASE_TIME.
    ///
    /// Using a non-zero time is important because `MillisSinceEpoch::now()` only uses
    /// the cached WallClock when it is non-zero; otherwise it falls back to system time.
    pub const BASE_TIME: MillisSinceEpoch = MillisSinceEpoch::new(1_700_000_000_000);

    /// Creates a new simulation clock starting at the given time.
    ///
    /// Also sets the restate WallClock cached time so that `MillisSinceEpoch::now()`
    /// returns the simulated time from the start.
    ///
    /// # Note
    ///
    /// The start time should be non-zero to ensure `MillisSinceEpoch::now()` uses
    /// the cached time instead of falling back to system time.
    pub fn new(start_time: MillisSinceEpoch) -> Self {
        // Initialize WallClock with the starting simulated time
        WallClock::set_recent(start_time);

        Self {
            inner: Arc::new(SimulationClockInner {
                current_time: AtomicU64::new(start_time.as_u64()),
            }),
        }
    }

    /// Creates a new simulation clock starting at [`BASE_TIME`](Self::BASE_TIME).
    ///
    /// This is the recommended way to create a simulation clock as it starts
    /// at a non-zero time, ensuring `MillisSinceEpoch::now()` uses the cached time.
    pub fn new_at_base_time() -> Self {
        Self::new(Self::BASE_TIME)
    }

    /// Clears the global WallClock cache, causing `MillisSinceEpoch::now()` to
    /// fall back to system time.
    ///
    /// Call this at the end of tests to avoid leaking simulated time to other tests.
    pub fn clear_wallclock() {
        WallClock::clear_recent();
    }

    /// Returns the current time.
    pub fn now(&self) -> MillisSinceEpoch {
        MillisSinceEpoch::new(self.inner.current_time.load(Ordering::Acquire))
    }

    /// Advances the clock by the specified number of milliseconds.
    /// Also updates the restate WallClock cached time.
    /// Returns the new current time.
    pub fn advance_ms(&self, millis: u64) -> MillisSinceEpoch {
        let new_time = self.inner.current_time.fetch_add(millis, Ordering::AcqRel) + millis;
        let new_time = MillisSinceEpoch::new(new_time);

        // Also update the global WallClock cache
        WallClock::set_recent(new_time);

        new_time
    }

    /// Advances the clock to the specified time.
    /// Also updates the restate WallClock cached time so that `MillisSinceEpoch::now()`
    /// returns the simulated time.
    ///
    /// # Panics
    ///
    /// Panics if the target time is before the current time.
    pub fn advance_to(&self, target: MillisSinceEpoch) {
        let current = self.now();
        assert!(
            target >= current,
            "Cannot move clock backwards: current={}, target={}",
            current.as_u64(),
            target.as_u64()
        );
        self.inner
            .current_time
            .store(target.as_u64(), Ordering::Release);

        // Also update the global WallClock cache so MillisSinceEpoch::now() returns simulated time
        WallClock::set_recent(target);
    }

    /// Sets the clock to a specific time, allowing backward movement.
    /// Also updates the restate WallClock cached time.
    /// Use with caution - primarily for test setup.
    pub fn set(&self, time: MillisSinceEpoch) {
        self.inner
            .current_time
            .store(time.as_u64(), Ordering::Release);

        // Also update the global WallClock cache
        WallClock::set_recent(time);
    }

    /// Advances the clock by the specified duration.
    /// Returns the new current time.
    pub fn advance(&self, duration: Duration) -> MillisSinceEpoch {
        let millis = u64::try_from(duration.as_millis()).expect("duration fits in u64");
        self.advance_ms(millis)
    }

    /// Advances the simulation clock, tokio's paused time, and restate WallClock to the target.
    ///
    /// This method should be used when tokio time is paused (`start_paused = true`)
    /// to ensure that:
    /// - `tokio::time::sleep` and other timer-based operations complete correctly
    /// - `MillisSinceEpoch::now()` returns the simulated time
    ///
    /// # Panics
    ///
    /// Panics if the target time is before the current time.
    pub async fn advance_to_async(&self, target: MillisSinceEpoch) {
        let current = self.now();
        assert!(
            target >= current,
            "Cannot move clock backwards: current={}, target={}",
            current.as_u64(),
            target.as_u64()
        );

        // Update our internal time and WallClock BEFORE advancing tokio time.
        // This ensures that any tasks that wake up during tokio::time::advance()
        // will see the new simulated time via MillisSinceEpoch::now().
        self.inner
            .current_time
            .store(target.as_u64(), Ordering::Release);
        WallClock::set_recent(target);

        // Now advance tokio's paused time
        let delta_ms = target.as_u64().saturating_sub(current.as_u64());
        if delta_ms > 0 {
            tokio::time::advance(Duration::from_millis(delta_ms)).await;
        }
    }

    /// Advances both the simulation clock and tokio's paused time by the given duration.
    ///
    /// This method should be used when tokio time is paused (`start_paused = true`)
    /// to ensure that `tokio::time::sleep` and other timer-based operations
    /// complete at the correct simulated time.
    pub async fn advance_async(&self, duration: Duration) -> MillisSinceEpoch {
        let millis = u64::try_from(duration.as_millis()).expect("duration fits in u64");
        let target = MillisSinceEpoch::new(self.now().as_u64() + millis);
        self.advance_to_async(target).await;
        target
    }
}

impl Default for SimulationClock {
    /// Creates a simulation clock at [`BASE_TIME`](Self::BASE_TIME).
    fn default() -> Self {
        Self::new_at_base_time()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_advance() {
        let clock = SimulationClock::new(MillisSinceEpoch::UNIX_EPOCH);
        assert_eq!(clock.now(), MillisSinceEpoch::UNIX_EPOCH);

        clock.advance_ms(1000);
        assert_eq!(clock.now().as_u64(), 1000);

        clock.advance_ms(500);
        assert_eq!(clock.now().as_u64(), 1500);
    }

    #[test]
    fn test_clock_advance_to() {
        let clock = SimulationClock::new(MillisSinceEpoch::UNIX_EPOCH);

        clock.advance_to(MillisSinceEpoch::new(5000));
        assert_eq!(clock.now().as_u64(), 5000);
    }

    #[test]
    #[should_panic(expected = "Cannot move clock backwards")]
    fn test_clock_advance_to_backwards_panics() {
        let clock = SimulationClock::new(MillisSinceEpoch::new(1000));
        clock.advance_to(MillisSinceEpoch::new(500));
    }

    #[test]
    fn test_clock_clone_shares_state() {
        let clock1 = SimulationClock::new(MillisSinceEpoch::UNIX_EPOCH);
        let clock2 = clock1.clone();

        clock1.advance_ms(1000);
        assert_eq!(clock2.now().as_u64(), 1000);
    }

    #[tokio::test(start_paused = true)]
    async fn test_clock_advance_async_with_tokio_paused_time() {
        let clock = SimulationClock::new(MillisSinceEpoch::UNIX_EPOCH);
        let tokio_start = tokio::time::Instant::now();

        // Advance clock by 5 seconds
        clock.advance_async(Duration::from_secs(5)).await;

        // Verify simulation clock advanced
        assert_eq!(clock.now().as_u64(), 5000);

        // Verify tokio's paused time also advanced
        let tokio_elapsed = tokio_start.elapsed();
        assert_eq!(tokio_elapsed, Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn test_clock_advance_to_async_with_tokio_paused_time() {
        let clock = SimulationClock::new(MillisSinceEpoch::UNIX_EPOCH);
        let tokio_start = tokio::time::Instant::now();

        // Advance to specific time
        clock.advance_to_async(MillisSinceEpoch::new(10_000)).await;

        // Verify simulation clock advanced
        assert_eq!(clock.now().as_u64(), 10_000);

        // Verify tokio's paused time also advanced
        let tokio_elapsed = tokio_start.elapsed();
        assert_eq!(tokio_elapsed, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn test_clock_advance_async_allows_sleeps_to_complete() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let clock = SimulationClock::new(MillisSinceEpoch::UNIX_EPOCH);
        let sleep_completed = Arc::new(AtomicBool::new(false));
        let sleep_completed_clone = sleep_completed.clone();

        // Spawn a task that sleeps for 3 seconds
        let sleep_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            sleep_completed_clone.store(true, Ordering::SeqCst);
        });

        // Sleep hasn't completed yet
        assert!(!sleep_completed.load(Ordering::SeqCst));

        // Advance clock by 4 seconds (past the sleep duration)
        clock.advance_async(Duration::from_secs(4)).await;

        // Wait for the spawned task to complete
        sleep_handle.await.unwrap();

        // Now the sleep should have completed
        assert!(sleep_completed.load(Ordering::SeqCst));
    }

    #[test]
    fn test_clock_sets_wallclock_on_creation() {
        let start_time = MillisSinceEpoch::new(5_000_000);
        let _clock = SimulationClock::new(start_time);

        // WallClock's cached time should be set
        assert_eq!(WallClock::recent_ms(), start_time);
    }

    #[test]
    fn test_clock_updates_wallclock_on_advance() {
        let clock = SimulationClock::new(MillisSinceEpoch::new(1_000_000));

        // Advance by 5000ms
        clock.advance_ms(5000);

        // WallClock should also be updated
        assert_eq!(WallClock::recent_ms(), MillisSinceEpoch::new(1_005_000));
    }

    #[test]
    fn test_millis_since_epoch_now_returns_simulated_time() {
        let start_time = MillisSinceEpoch::new(2_000_000);
        let clock = SimulationClock::new(start_time);

        // MillisSinceEpoch::now() should return the simulated time
        // (uses WallClock::recent_ms() when cached time is set)
        assert_eq!(MillisSinceEpoch::now(), start_time);

        // Advance clock
        clock.advance_ms(1000);

        // MillisSinceEpoch::now() should return the new time
        assert_eq!(MillisSinceEpoch::now(), MillisSinceEpoch::new(2_001_000));
    }

    // --- TokioClock tests ---

    #[tokio::test(start_paused = true)]
    async fn test_tokio_clock_starts_at_base_time() {
        let clock = TokioClock::new();
        assert_eq!(clock.now(), TokioClock::BASE_TIME);
    }

    #[tokio::test(start_paused = true)]
    async fn test_tokio_clock_tracks_elapsed_time() {
        let clock = TokioClock::new();
        let t0 = clock.now();

        // Advance tokio time by 5 seconds
        tokio::time::advance(Duration::from_secs(5)).await;

        let t1 = clock.now();
        assert_eq!(t1.as_u64() - t0.as_u64(), 5000);
    }

    #[tokio::test(start_paused = true)]
    async fn test_tokio_clock_auto_advances_with_sleep() {
        let clock = TokioClock::new();
        let t0 = clock.now();

        // Sleep for 3 seconds - tokio will auto-advance when paused
        tokio::time::sleep(Duration::from_secs(3)).await;

        let t1 = clock.now();
        assert_eq!(t1.as_u64() - t0.as_u64(), 3000);
    }

    #[tokio::test(start_paused = true)]
    async fn test_tokio_clock_sync_wallclock() {
        let clock = TokioClock::new();

        // Advance tokio time
        tokio::time::advance(Duration::from_secs(10)).await;

        // Sync WallClock
        clock.sync_wallclock();

        // MillisSinceEpoch::now() should return the advanced time
        let expected = TokioClock::BASE_TIME + Duration::from_secs(10);
        assert_eq!(MillisSinceEpoch::now(), expected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_tokio_clock_implements_clock_trait() {
        use restate_clock::Clock;

        let clock = TokioClock::new();

        // Both now() and recent() should return the same value
        assert_eq!(clock.now(), clock.recent());

        tokio::time::advance(Duration::from_secs(1)).await;

        // After advancing, both should still be equal
        assert_eq!(clock.now(), clock.recent());
        assert_eq!(clock.now(), TokioClock::BASE_TIME + Duration::from_secs(1));
    }
}
