// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Integration tests for the deterministic simulation framework.

use std::ops::RangeInclusive;

use googletest::prelude::*;
use test_log::test;
use tracing::info;

use restate_core::TaskCenter;
use restate_partition_store::PartitionStoreManager;
use restate_rocksdb::RocksDbManager;
use restate_simulation::{
    InvokerBehavior, PartitionSimulation, PartitionSimulationConfig, SimulationTrace,
};
use restate_types::config::StorageOptions;
use restate_types::identifiers::{InvocationId, InvocationUuid, PartitionId, PartitionKey};
use restate_types::partitions::Partition;
use restate_worker::state_machine::Action;

/// Creates a test storage setup.
///
/// The `RocksDbManager` is a singleton that persists for the lifetime of the process.
/// After calling `shutdown()`, no new DBs can be opened. Use `reset()` instead if you
/// need to run multiple independent test scenarios within the same process.
async fn create_test_storage() -> restate_partition_store::PartitionStore {
    RocksDbManager::init();
    let storage_options = StorageOptions::default();
    info!(
        "Using RocksDB temp directory {}",
        storage_options.data_dir("db").display()
    );
    let manager = PartitionStoreManager::create().await.unwrap();
    manager
        .open(
            &Partition::new(
                PartitionId::MIN,
                RangeInclusive::new(PartitionKey::MIN, PartitionKey::MAX),
            ),
            None,
        )
        .await
        .unwrap()
}

/// Resets the RocksDB environment between test scenarios.
///
/// This closes all open databases but allows new ones to be opened afterward.
/// Use this between independent test scenarios within the same test function.
#[allow(dead_code)]
async fn reset_test_env() {
    RocksDbManager::get()
        .reset()
        .await
        .expect("RocksDB reset failed");
}

/// Shuts down the test environment completely.
///
/// Call this only at the end of all tests - after this, no new DBs can be opened.
async fn shutdown_test_env() {
    TaskCenter::shutdown_node("test complete", 0).await;
    RocksDbManager::get().shutdown().await;
}

/// Main integration test that runs all simulation scenarios.
///
/// # Why tests are consolidated
///
/// The `RocksDbManager` is a process-level singleton (`static OnceLock`). Once `shutdown()` is
/// called, it sets `shutting_down = true` and no new databases can be opened.
///
/// To run independent test scenarios within the same process, you have two options:
///
/// 1. **Consolidated tests** (current approach): Run all scenarios in one test function,
///    sharing the same storage. This is simpler and matches the partition-store test pattern.
///
/// 2. **Independent scenarios with reset**: Between scenarios, call `reset()` instead of
///    `shutdown()`. This closes all DBs but resets `shutting_down = false`, allowing new
///    `PartitionStoreManager` instances to open fresh DBs. Note that you need a new
///    `PartitionStoreManager` after reset since it caches DB handles.
#[test(restate_core::test(start_paused = true, rng_seed = 42))]
async fn test_partition_simulation() -> googletest::Result<()> {
    let storage = create_test_storage().await;

    // Test 1: Basic invocation completes with expected actions
    info!("=== Test 1: Basic invocation sequence ===");
    {
        let config = PartitionSimulationConfig {
            seed: 123,
            max_steps: 100,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: true,
        };

        let mut sim =
            PartitionSimulation::new(config, storage.clone(), InvokerBehavior::ImmediateSuccess);

        let invocation = sim.random_invocation();
        sim.enqueue_invocation(invocation);

        let mut saw_invoke_action = false;
        let mut step_count = 0;

        while sim.should_continue() {
            match sim.step().await {
                Ok(result) => {
                    step_count += 1;
                    info!(
                        step = step_count,
                        command = ?result.command,
                        num_actions = result.actions.len(),
                        "Step completed"
                    );

                    for action in &result.actions {
                        if matches!(action, Action::Invoke { .. } | Action::VQInvoke { .. }) {
                            saw_invoke_action = true;
                        }
                    }
                }
                Err(restate_simulation::SimulationError::NoPendingWork) => break,
                Err(e) => return Err(e.into()),
            }
        }

        assert_that!(saw_invoke_action, eq(true));
        assert_that!(step_count, eq(4)); // Invoke + 3 InvokerEffects
        info!(
            "Test 1 passed: Basic invocation completed in {} steps",
            step_count
        );
    }

    // Test 2: VO exclusivity stress test with probabilistic invoker
    info!("=== Test 2: VO exclusivity stress test ===");
    {
        let config = PartitionSimulationConfig {
            seed: 42,
            max_steps: 500,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: true,
        };

        let mut sim = PartitionSimulation::new(
            config,
            storage.clone(),
            InvokerBehavior::Probabilistic {
                success_rate: 0.6,
                failure_rate: 0.2,
            },
        );

        // Enqueue many invocations targeting small key space
        for _ in 0..20 {
            let invocation = sim.random_vo_invocation();
            info!(
                invocation_id = ?invocation.invocation_id,
                target = ?invocation.invocation_target,
                "Enqueueing VO invocation"
            );
            sim.enqueue_invocation(invocation);
        }

        let outcome = sim.run().await?;

        info!(
            steps = outcome.steps_executed,
            success = outcome.success,
            violations = ?outcome.violations,
            "Stress test completed"
        );

        assert_that!(outcome.success, eq(true));
        assert_that!(outcome.violations, empty());
        assert_that!(outcome.steps_executed, gt(0));
        info!(
            "Test 2 passed: VO exclusivity held after {} steps",
            outcome.steps_executed
        );
    }

    // Test 3: Lock release on failure
    info!("=== Test 3: Lock release on failure ===");
    {
        let config = PartitionSimulationConfig {
            seed: 456,
            max_steps: 100,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: true,
        };

        let mut sim = PartitionSimulation::new(
            config,
            storage.clone(),
            InvokerBehavior::Probabilistic {
                success_rate: 0.0,
                failure_rate: 1.0,
            },
        );

        let inv1 = sim.invocation_for_key("key-a");
        let inv2 = sim.invocation_for_key("key-a");
        info!(inv1 = ?inv1.invocation_id, inv2 = ?inv2.invocation_id, "Testing lock release");
        sim.enqueue_invocation(inv1);
        sim.enqueue_invocation(inv2);

        let outcome = sim.run().await?;

        assert_that!(outcome.success, eq(true));
        assert_that!(outcome.violations, empty());
        info!("Test 3 passed: Lock released after failure");
    }

    // Test 4: Trace recording demonstration
    info!("=== Test 4: Trace recording ===");
    {
        let config = PartitionSimulationConfig {
            seed: 333,
            max_steps: 100,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: true,
        };

        let mut sim =
            PartitionSimulation::new(config, storage.clone(), InvokerBehavior::ImmediateSuccess);
        sim.enable_tracing();

        // Add some invocations
        for _ in 0..5 {
            let invocation = sim.random_invocation();
            sim.enqueue_invocation(invocation);
        }

        let outcome = sim.run().await?;
        let trace = sim.take_trace().expect("Trace should be present");

        info!(
            steps = outcome.steps_executed,
            trace_entries = trace.len(),
            "Simulation completed with tracing"
        );

        // Print first 3 trace entries
        for entry in trace.entries().iter().take(3) {
            info!(
                "  Step {}: time={}, command={:?}, actions={}",
                entry.step,
                entry.time.as_u64(),
                entry.command,
                entry.actions.len()
            );
        }

        // Verify serialization roundtrip
        let json = trace.to_json().expect("Serialization failed");
        info!("Trace serialized to {} bytes JSON", json.len());

        let restored = SimulationTrace::from_json(&json).expect("Deserialization failed");
        assert_that!(restored.compare(&trace), ok(eq(())));
        info!("Test 4 passed: Trace recording and serialization verified");
    }

    // Test 5: Racey scenario stress test for determinism
    // This exercises patterns that could expose non-determinism:
    // - Many concurrent invocations to the same keys
    // - Mix of success/failure/timeout outcomes
    // - Rapid enqueue/process cycles
    info!("=== Test 5: Racey scenario stress test ===");
    {
        let config = PartitionSimulationConfig {
            seed: 555,
            max_steps: 2000,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: true,
        };

        let mut sim = PartitionSimulation::new(
            config,
            storage.clone(),
            // Mix of outcomes to create more complex interleavings
            InvokerBehavior::Probabilistic {
                success_rate: 0.5,
                failure_rate: 0.3,
                // 0.2 timeout - no response
            },
        );
        sim.enable_tracing();

        // Rapidly enqueue many invocations targeting the same small key space
        // This creates contention and races for VO locks
        for _ in 0..50 {
            let invocation = sim.random_vo_invocation();
            sim.enqueue_invocation(invocation);
        }

        let outcome = sim.run().await?;
        let trace = sim.take_trace().expect("Trace should be present");

        info!(
            steps = outcome.steps_executed,
            trace_entries = trace.len(),
            success = outcome.success,
            "Racey scenario completed"
        );

        assert_that!(outcome.success, eq(true));
        assert_that!(outcome.violations, empty());

        // The trace should be deterministic - same seed should always produce same trace
        // This is verified by the failing repeated history test (next task)
        info!(
            "Test 5 passed: Racey scenario completed with {} trace entries",
            trace.len()
        );
    }

    // Throughput benchmark: measure commands/second
    info!("=== Throughput benchmark ===");
    {
        let config = PartitionSimulationConfig {
            seed: 789,
            max_steps: 10_000,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: false, // Disable for raw throughput
        };

        let mut sim =
            PartitionSimulation::new(config, storage.clone(), InvokerBehavior::ImmediateSuccess);

        // Enqueue many invocations
        for _ in 0..1000 {
            let invocation = sim.random_invocation();
            sim.enqueue_invocation(invocation);
        }

        let start = std::time::Instant::now();
        let outcome = sim.run().await?;
        let elapsed = start.elapsed();

        let steps_per_sec = outcome.steps_executed as f64 / elapsed.as_secs_f64();
        let simulated_time_ms = sim.clock().now().as_u64();

        info!(
            steps = outcome.steps_executed,
            elapsed_ms = elapsed.as_millis(),
            steps_per_sec = steps_per_sec as u64,
            simulated_time_ms = simulated_time_ms,
            "Throughput benchmark completed"
        );

        // With 1000 invocations * 4 steps each = 4000 steps expected
        assert_that!(outcome.steps_executed, ge(3000));
        info!(
            "Throughput (no invariants): {} commands/sec",
            steps_per_sec as u64
        );

        // Now with invariant checking
        let config_with_invariants = PartitionSimulationConfig {
            seed: 790,
            max_steps: 10_000,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: true,
        };

        let mut sim2 = PartitionSimulation::new(
            config_with_invariants,
            storage.clone(),
            InvokerBehavior::ImmediateSuccess,
        );

        for _ in 0..1000 {
            let invocation = sim2.random_invocation();
            sim2.enqueue_invocation(invocation);
        }

        let start2 = std::time::Instant::now();
        let outcome2 = sim2.run().await?;
        let elapsed2 = start2.elapsed();
        let steps_per_sec2 = outcome2.steps_executed as f64 / elapsed2.as_secs_f64();

        info!(
            "Throughput (with invariants): {} commands/sec, overhead: {:.1}%",
            steps_per_sec2 as u64,
            ((steps_per_sec - steps_per_sec2) / steps_per_sec) * 100.0
        );
    }

    // Test 6: Trace recording for cross-run determinism verification
    // Demonstrates how to capture and log trace information for comparison across runs
    info!("=== Test 6: Trace recording for determinism verification ===");
    {
        let invocations = create_deterministic_invocations(99999, 30);
        info!(
            "Generated {} invocations for determinism test",
            invocations.len()
        );

        let trace = run_simulation_with_trace(99999, invocations, storage.clone()).await;
        info!("Simulation completed: {} trace entries", trace.len());

        // Verify serialization roundtrip
        let json = trace.to_json().expect("Serialization failed");
        let restored = SimulationTrace::from_json(&json).expect("Deserialization failed");
        assert_that!(trace.compare(&restored), ok(eq(())));
        info!("Trace serialization verified: {} bytes JSON", json.len());

        // Log summary for cross-run comparison
        // When verifying determinism across process runs, compare these values
        info!("=== Trace Summary (for cross-run comparison) ===");
        info!("Total steps: {}", trace.len());
        if let Some(first) = trace.entries().first() {
            info!(
                "First entry: step={}, command={:?}",
                first.step, first.command
            );
        }
        if let Some(last) = trace.entries().last() {
            info!("Last entry: step={}, command={:?}", last.step, last.command);
        }
        info!("Test 6 passed: Trace recorded for determinism verification");
    }

    shutdown_test_env().await;
    info!("=== All tests passed ===");
    Ok(())
}

/// Tests that tokio's auto-advance works correctly with start_paused.
/// This demonstrates the infrastructure for time acceleration.
#[test(restate_core::test(start_paused = true, rng_seed = 42))]
async fn test_tokio_auto_advance() {
    use std::time::Duration;

    // Record both wall clock and simulated time
    let wall_start = std::time::Instant::now();
    let sim_start = tokio::time::Instant::now();

    // Sleep for 1 hour in simulated time
    tokio::time::sleep(Duration::from_secs(3600)).await;

    let wall_elapsed = wall_start.elapsed();
    let sim_elapsed = sim_start.elapsed();

    // Verify that 1 hour of simulated time has passed
    assert!(
        sim_elapsed >= Duration::from_secs(3600),
        "Should have advanced 1 hour simulated"
    );

    // Wall clock time should be nearly instant
    assert!(
        wall_elapsed < Duration::from_secs(1),
        "Wall clock should be < 1s, got {:?}",
        wall_elapsed
    );

    let acceleration = sim_elapsed.as_secs_f64() / wall_elapsed.as_secs_f64();
    info!(
        "Time acceleration: simulated {:?} in {:?} wall clock = {:.0}x",
        sim_elapsed, wall_elapsed, acceleration
    );
}

/// Tests that the seeded RNG produces deterministic results.
/// This test doesn't need RocksDB so it can run separately.
#[test]
fn test_seeded_rng_determinism() {
    use rand::{Rng, SeedableRng, rngs::StdRng};

    let seed = 999u64;

    let mut rng1 = StdRng::seed_from_u64(seed);
    let mut rng2 = StdRng::seed_from_u64(seed);

    for _ in 0..100 {
        let v1: u64 = rng1.random();
        let v2: u64 = rng2.random();
        assert_that!(v1, eq(v2));
    }

    let mut rng3 = StdRng::seed_from_u64(seed + 1);
    let v1: u64 = StdRng::seed_from_u64(seed).random();
    let v3: u64 = rng3.random();
    assert_that!(v1, not(eq(v3)));
}

/// Tests trace comparison logic without RocksDB.
/// This validates the trace diff detection mechanism.
#[test]
fn z_test_trace_comparison() {
    // Create two identical traces
    let trace1 = SimulationTrace::new();
    let trace2 = SimulationTrace::new();

    // Test that empty traces are equal
    assert!(trace1.compare(&trace2).is_ok());

    // Test serialization roundtrip
    let json = trace1.to_json().expect("Serialization failed");
    let restored = SimulationTrace::from_json(&json).expect("Deserialization failed");
    assert!(restored.compare(&trace1).is_ok());
}

/// Creates a deterministic set of invocations for repeated history testing.
/// Uses a seeded RNG to generate consistent invocations.
fn create_deterministic_invocations(
    seed: u64,
    count: usize,
) -> Vec<restate_types::invocation::ServiceInvocation> {
    use rand::{Rng, SeedableRng, rngs::StdRng};
    use restate_types::invocation::{
        InvocationTarget, ServiceInvocation, Source, VirtualObjectHandlerType,
    };

    let keys = ["key-a", "key-b", "key-c"];
    let mut rng = StdRng::seed_from_u64(seed);
    let mut invocations = Vec::with_capacity(count);

    for _ in 0..count {
        let key_idx = rng.random_range(0..keys.len());
        let key = keys[key_idx];
        let target = InvocationTarget::virtual_object(
            "TestService",
            key,
            "handler",
            VirtualObjectHandlerType::Exclusive,
        );
        // Generate deterministic invocation ID using seeded RNG instead of
        // InvocationId::generate which uses Ulid::new() (non-deterministic)
        let partition_key: PartitionKey = rng.random();
        let uuid_high: u64 = rng.random();
        let uuid_low: u64 = rng.random();
        let uuid = ((uuid_high as u128) << 64) | (uuid_low as u128);
        // Ensure non-zero UUID (zero is not a valid invocation UUID)
        let uuid = if uuid == 0 { 1 } else { uuid };
        let invocation_id =
            InvocationId::from_parts(partition_key, InvocationUuid::from_u128(uuid));
        invocations.push(ServiceInvocation::initialize(
            invocation_id,
            target,
            Source::Ingress(Default::default()),
        ));
    }

    invocations
}

/// Runs a simulation with the given invocations and returns the trace.
async fn run_simulation_with_trace(
    seed: u64,
    invocations: Vec<restate_types::invocation::ServiceInvocation>,
    storage: restate_partition_store::PartitionStore,
) -> SimulationTrace {
    let config = PartitionSimulationConfig {
        seed,
        max_steps: 2000,
        partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
        check_invariants: true,
    };

    let mut sim = PartitionSimulation::new(
        config,
        storage,
        InvokerBehavior::Probabilistic {
            success_rate: 0.5,
            failure_rate: 0.3,
        },
    );
    sim.enable_tracing();

    for invocation in invocations {
        sim.enqueue_invocation(invocation);
    }

    sim.run().await.expect("Simulation failed");
    sim.take_trace().expect("Trace should be present")
}
