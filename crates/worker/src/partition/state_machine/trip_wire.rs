// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Trip wire for simulation testing.
//!
//! This module provides controlled failure injection for testing that the
//! simulation framework correctly detects invariant violations.
//!
//! # Usage
//!
//! Enable the trip wire before running simulation tests:
//!
//! ```ignore
//! use restate_worker::partition::state_machine::trip_wire;
//!
//! // Enable with 0.1% probability of skipping VO unlock
//! trip_wire::enable_skip_unlock(0.001);
//!
//! // Run simulation...
//!
//! // Disable after test
//! trip_wire::disable();
//! ```
//!
//! # Determinism
//!
//! The trip wire uses a counter-based approach for deterministic behavior.
//! Given the same sequence of calls, it will produce the same failures.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Whether the VO unlock skip trip wire is enabled.
static SKIP_UNLOCK_ENABLED: AtomicBool = AtomicBool::new(false);

/// Probability of skipping unlock (in parts per 10000, i.e., 1 = 0.01%).
static SKIP_UNLOCK_PROBABILITY: AtomicU64 = AtomicU64::new(0);

/// Counter for deterministic failure injection.
static SKIP_UNLOCK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Enables the "skip VO unlock" trip wire with the given probability.
///
/// When enabled, `should_skip_unlock()` will return `true` with approximately
/// the given probability, causing VO locks to not be released. This should
/// trigger invariant violations that the simulation checker can detect.
///
/// # Arguments
///
/// * `probability` - Probability of skipping unlock (0.0 - 1.0)
pub fn enable_skip_unlock(probability: f64) {
    let prob_10000 = (probability.clamp(0.0, 1.0) * 10000.0) as u64;
    SKIP_UNLOCK_PROBABILITY.store(prob_10000, Ordering::SeqCst);
    SKIP_UNLOCK_COUNTER.store(0, Ordering::SeqCst);
    SKIP_UNLOCK_ENABLED.store(true, Ordering::SeqCst);
}

/// Disables all trip wires.
pub fn disable() {
    SKIP_UNLOCK_ENABLED.store(false, Ordering::SeqCst);
}

/// Resets the trip wire counter (useful between test iterations).
pub fn reset_counter() {
    SKIP_UNLOCK_COUNTER.store(0, Ordering::SeqCst);
}

/// Checks if we should skip the VO unlock operation.
///
/// This is called from unlock paths to inject failures.
/// Returns `true` if the unlock should be skipped (leaving the VO locked).
#[inline]
pub fn should_skip_unlock() -> bool {
    if !SKIP_UNLOCK_ENABLED.load(Ordering::Relaxed) {
        return false;
    }

    let prob = SKIP_UNLOCK_PROBABILITY.load(Ordering::Relaxed);
    if prob == 0 {
        return false;
    }

    // Counter-based determinism: skip every N-th call where N = 10000/prob
    // E.g., with 1% probability (prob=100), skip every 100th call (when count % 100 == 0)
    let count = SKIP_UNLOCK_COUNTER.fetch_add(1, Ordering::Relaxed);
    let period = 10000 / prob;
    count.is_multiple_of(period)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trip_wire_disabled_by_default() {
        disable(); // Ensure clean state
        assert!(!should_skip_unlock());
        assert!(!should_skip_unlock());
        assert!(!should_skip_unlock());
    }

    #[test]
    fn test_trip_wire_deterministic() {
        // Enable with 1% probability (100/10000)
        enable_skip_unlock(0.01);

        // First 100 calls should include skips at predictable positions
        let mut skips = Vec::new();
        for i in 0..200 {
            if should_skip_unlock() {
                skips.push(i);
            }
        }

        // Reset and verify same pattern
        reset_counter();
        let mut skips2 = Vec::new();
        for i in 0..200 {
            if should_skip_unlock() {
                skips2.push(i);
            }
        }

        assert_eq!(skips, skips2, "Trip wire should be deterministic");
        assert!(
            !skips.is_empty(),
            "Should have some skips with 1% probability"
        );

        disable();
    }

    #[test]
    fn test_trip_wire_probability() {
        enable_skip_unlock(0.01); // 1% = 100/10000 → skip every 100th call

        let mut skip_count = 0;
        for _ in 0..1000 {
            if should_skip_unlock() {
                skip_count += 1;
            }
        }

        // With 1% probability over 1000 calls, expect 10 skips (every 100th call)
        assert_eq!(
            skip_count, 10,
            "Expected exactly 10 skips with 1% over 1000 calls"
        );

        disable();
    }
}
