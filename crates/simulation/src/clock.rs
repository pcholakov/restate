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
//! This module provides a [`SimulationClock`] that can be manually advanced for
//! deterministic simulation testing. When running with tokio's paused time
//! (`start_paused = true`), the clock integrates with tokio's time to ensure
//! all time-dependent operations see consistent simulated time.
//!
//! # Tokio Integration
//!
//! When using `#[restate_core::test(start_paused = true)]`, tokio's time is paused.
//! The [`SimulationClock`] synchronizes with tokio's paused time via
//! [`advance_to_async`](SimulationClock::advance_to_async) and
//! [`advance_async`](SimulationClock::advance_async).
//!
//! This ensures that:
//! - All `tokio::time::sleep` and `tokio::time::timeout` calls use simulated time
//! - Timer-based logic in the partition processor works correctly
//! - The simulation has deterministic control over all time sources

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use restate_clock::WallClock;
use restate_types::time::MillisSinceEpoch;

/// A deterministic clock for simulation testing.
///
/// This clock can be advanced manually, providing full control over time progression
/// in the simulation. The clock is thread-safe and can be shared across components.
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
    /// Creates a new simulation clock starting at the given time.
    ///
    /// Also sets the restate WallClock cached time so that `MillisSinceEpoch::now()`
    /// returns the simulated time from the start.
    pub fn new(start_time: MillisSinceEpoch) -> Self {
        // Initialize WallClock with the starting simulated time
        WallClock::set_recent(start_time);

        Self {
            inner: Arc::new(SimulationClockInner {
                current_time: AtomicU64::new(start_time.as_u64()),
            }),
        }
    }

    /// Creates a new simulation clock starting at the Restate epoch (time 0).
    pub fn at_epoch() -> Self {
        Self::new(MillisSinceEpoch::UNIX_EPOCH)
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

        // Calculate the duration to advance
        let delta_ms = target.as_u64().saturating_sub(current.as_u64());
        if delta_ms > 0 {
            // Advance tokio's paused time first
            tokio::time::advance(Duration::from_millis(delta_ms)).await;
        }

        // Update our internal time
        self.inner
            .current_time
            .store(target.as_u64(), Ordering::Release);

        // Also update the global WallClock cache so MillisSinceEpoch::now() returns simulated time
        WallClock::set_recent(target);
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
    fn default() -> Self {
        Self::at_epoch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_advance() {
        let clock = SimulationClock::at_epoch();
        assert_eq!(clock.now(), MillisSinceEpoch::UNIX_EPOCH);

        clock.advance_ms(1000);
        assert_eq!(clock.now().as_u64(), 1000);

        clock.advance_ms(500);
        assert_eq!(clock.now().as_u64(), 1500);
    }

    #[test]
    fn test_clock_advance_to() {
        let clock = SimulationClock::at_epoch();

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
        let clock1 = SimulationClock::at_epoch();
        let clock2 = clock1.clone();

        clock1.advance_ms(1000);
        assert_eq!(clock2.now().as_u64(), 1000);
    }

    #[tokio::test(start_paused = true)]
    async fn test_clock_advance_async_with_tokio_paused_time() {
        let clock = SimulationClock::at_epoch();
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
        let clock = SimulationClock::at_epoch();
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

        let clock = SimulationClock::at_epoch();
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
}
