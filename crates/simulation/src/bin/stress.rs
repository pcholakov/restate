// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Standalone stress test binary for deterministic simulation testing.
//!
//! This binary runs multiple simulation iterations with random seeds for a configurable
//! duration. On failure, it prints detailed reproduction information including the
//! seed, invariant violation, and trace.
//!
//! # Usage
//!
//! Run for 15 minutes with random seeds:
//! ```bash
//! cargo run -p restate-simulation --bin simulation-stress --features stress-bin -- --duration 900
//! ```
//!
//! Reproduce a specific failure:
//! ```bash
//! cargo run -p restate-simulation --bin simulation-stress --features stress-bin -- --seed 12345
//! ```
//!
//! # CPU Parallelism
//!
//! Due to RocksDB singleton constraints, this binary runs single-threaded. To utilize
//! multiple CPU cores, run multiple processes in parallel:
//!
//! ```bash
//! # Run 4 parallel stress test processes
//! for i in {1..4}; do
//!   cargo run -p restate-simulation --bin simulation-stress --features stress-bin \
//!     -- --duration 3600 &
//! done
//! wait
//! ```

use std::num::NonZeroUsize;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tracing::info;

use restate_core::TaskCenterBuilder;
use restate_partition_store::{PartitionStore, PartitionStoreManager};
use restate_rocksdb::RocksDbManager;
use restate_simulation::{
    InvokerBehavior, PartitionSimulation, PartitionSimulationConfig, SimulationError,
    SimulationTrace,
};
use restate_types::config::{
    Configuration, StorageOptions, reset_base_temp_dir, set_current_config,
};
use restate_types::identifiers::{PartitionId, PartitionKey};
use restate_types::partitions::Partition;

#[derive(Parser)]
#[command(name = "simulation-stress")]
#[command(about = "Deterministic simulation stress test for Restate")]
struct Args {
    /// Duration to run in seconds (default: 60)
    #[arg(long, default_value = "60")]
    duration: u64,

    /// Fixed seed for reproduction (random if not set)
    #[arg(long)]
    seed: Option<u64>,

    /// Number of invocations per iteration
    #[arg(long, default_value = "50")]
    invocations: usize,

    /// Maximum steps per iteration
    #[arg(long, default_value = "2000")]
    max_steps: usize,
}

struct FailureInfo {
    iteration: u64,
    seed: u64,
    error: String,
    trace: Option<SimulationTrace>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Setup tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("restate_simulation=info".parse().unwrap()),
        )
        .init();

    // Generate master seed
    let master_seed = args.seed.unwrap_or_else(|| {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let state = RandomState::new();
        let mut hasher = state.build_hasher();
        hasher.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        );
        hasher.finish()
    });

    let duration = Duration::from_secs(args.duration);
    let is_reproduction = args.seed.is_some();

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║              DETERMINISTIC SIMULATION STRESS TEST                    ║");
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!(
        "║ Duration:      {:>10} seconds                                    ║",
        args.duration
    );
    println!(
        "║ Master seed:   {:>20}                              ║",
        master_seed
    );
    println!(
        "║ Invocations:   {:>10} per iteration                              ║",
        args.invocations
    );
    println!(
        "║ Max steps:     {:>10} per iteration                              ║",
        args.max_steps
    );
    if is_reproduction {
        println!("║ Mode:          REPRODUCTION (fixed seed)                            ║");
    } else {
        println!("║ Mode:          EXPLORATION (random seeds)                           ║");
    }
    println!("╚══════════════════════════════════════════════════════════════════════╝");
    println!();

    // Create tokio runtime with paused time for deterministic simulation
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()?;

    // Build TaskCenter with the paused-time runtime
    let tc = TaskCenterBuilder::default()
        .default_runtime_handle(rt.handle().clone())
        .pause_time(true)
        .build()?;

    let result = tc.block_on(async {
        run_stress_test(
            master_seed,
            duration,
            args.invocations,
            args.max_steps,
            is_reproduction,
        )
        .await
    });

    match result {
        Err(e) => {
            eprintln!("Stress test panicked: {:?}", e);
            anyhow::bail!("Stress test panicked")
        }
        Ok(StressTestResult::SimulationFailure(failure)) => {
            print_failure(&failure);
            anyhow::bail!(
                "Simulation failed at iteration {} with seed {}",
                failure.iteration,
                failure.seed
            );
        }
        Ok(StressTestResult::Success(stats)) => {
            println!();
            println!("╔══════════════════════════════════════════════════════════════════════╗");
            println!("║                    ✅ STRESS TEST COMPLETED                          ║");
            println!("╠══════════════════════════════════════════════════════════════════════╣");
            println!(
                "║ Duration:      {:>10.1} seconds                                   ║",
                stats.elapsed_secs
            );
            println!(
                "║ Iterations:    {:>10}                                           ║",
                stats.iterations
            );
            println!(
                "║ Total steps:   {:>10}                                           ║",
                stats.total_steps
            );
            println!(
                "║ Steps/second:  {:>10.0}                                           ║",
                stats.total_steps as f64 / stats.elapsed_secs
            );
            println!(
                "║ Master seed:   {:>20}                             ║",
                master_seed
            );
            println!("╚══════════════════════════════════════════════════════════════════════╝");
            Ok(())
        }
    }
}

fn print_failure(failure: &FailureInfo) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║                    ❌ SIMULATION FAILURE DETECTED                    ║");
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!(
        "║ Iteration:     {:>10}                                           ║",
        failure.iteration
    );
    println!(
        "║ Seed:          {:>20}                             ║",
        failure.seed
    );
    println!(
        "║ Steps before:  {:>10}                                           ║",
        failure.trace.as_ref().map(|t| t.len()).unwrap_or(0)
    );
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!("║ Error:                                                               ║");
    for line in failure.error.lines() {
        println!("║   {}", line);
    }
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!("║ TO REPRODUCE:                                                        ║");
    println!("║                                                                      ║");
    println!("║   cargo run -p restate-simulation --bin simulation-stress \\         ║");
    println!(
        "║     --features stress-bin -- --seed {}              ║",
        failure.seed
    );
    println!("╚══════════════════════════════════════════════════════════════════════╝");

    // Print trace summary if available
    if let Some(ref trace) = failure.trace {
        println!();
        println!("=== TRACE SUMMARY ===");
        println!("Total entries: {}", trace.len());

        let entries = trace.entries();
        let start_idx = entries.len().saturating_sub(10);
        println!("Last {} entries:", entries.len() - start_idx);
        for entry in entries.iter().skip(start_idx) {
            println!(
                "  Step {:>4}: time={:>12}, command={:?}, actions={}",
                entry.step,
                entry.time.as_u64(),
                entry.command,
                entry.actions.len()
            );
        }

        // Save trace to file
        if let Ok(json) = trace.to_json() {
            let filename = format!("simulation_failure_seed_{}.json", failure.seed);
            if std::fs::write(&filename, &json).is_ok() {
                println!();
                println!("Trace saved to: {}", filename);
            }
        }
    }
}

struct StressTestStats {
    iterations: u64,
    total_steps: usize,
    elapsed_secs: f64,
}

enum StressTestResult {
    Success(StressTestStats),
    SimulationFailure(FailureInfo),
}

async fn run_stress_test(
    master_seed: u64,
    duration: Duration,
    num_invocations: usize,
    max_steps: usize,
    is_reproduction: bool,
) -> StressTestResult {
    let start = Instant::now();

    // Create storage
    let mut storage = create_test_storage().await;

    // Create RNG for iteration seeds
    let mut rng = StdRng::seed_from_u64(master_seed);

    let mut iteration_counter = 0u64;
    let mut total_steps = 0usize;

    // Progress reporting interval
    let mut last_progress = Instant::now();
    const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

    // Reset storage periodically to prevent resource exhaustion
    const RESET_INTERVAL: u64 = 500;
    let mut iterations_since_reset = 0u64;

    while start.elapsed() < duration {
        // Check if we need to reset storage
        if iterations_since_reset >= RESET_INTERVAL {
            drop(storage);
            reset_test_env().await;
            reset_base_temp_dir();
            storage = create_test_storage().await;
            iterations_since_reset = 0;
        }

        // Generate seed for this iteration
        let iteration_seed = if is_reproduction {
            master_seed
        } else {
            rng.random()
        };

        iteration_counter += 1;
        iterations_since_reset += 1;

        // Run single iteration
        let result =
            run_single_iteration(iteration_seed, num_invocations, max_steps, storage.clone()).await;

        match result {
            Ok(steps) => {
                total_steps += steps;
            }
            Err((error, trace)) => {
                // Clean shutdown before returning
                if let Some(manager) = RocksDbManager::maybe_get() {
                    manager.shutdown().await;
                }

                return StressTestResult::SimulationFailure(FailureInfo {
                    iteration: iteration_counter,
                    seed: iteration_seed,
                    error: format!("{}", error),
                    trace,
                });
            }
        }

        // Progress reporting
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            let elapsed = start.elapsed();
            let remaining = duration.saturating_sub(elapsed);
            println!(
                "[{:>6.1}s] Iterations: {:>8} | Steps: {:>10} | Steps/s: {:>8.0} | Remaining: {:>6.1}s",
                elapsed.as_secs_f64(),
                iteration_counter,
                total_steps,
                total_steps as f64 / elapsed.as_secs_f64(),
                remaining.as_secs_f64()
            );
            last_progress = Instant::now();
        }

        // Break early if in reproduction mode (run only one iteration)
        if is_reproduction {
            break;
        }
    }

    // Clean shutdown
    if let Some(manager) = RocksDbManager::maybe_get() {
        manager.shutdown().await;
    }

    StressTestResult::Success(StressTestStats {
        iterations: iteration_counter,
        total_steps,
        elapsed_secs: start.elapsed().as_secs_f64(),
    })
}

/// Creates a test storage setup with appropriate RocksDB configuration.
async fn create_test_storage() -> PartitionStore {
    // Configure RocksDB with a large memory budget to reduce SST file creation
    let mut config = Configuration::default();
    config.common.rocksdb_total_memory_size = NonZeroUsize::new(4 * 1024 * 1024 * 1024).unwrap();
    let config = config.apply_cascading_values();
    set_current_config(config);

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
async fn reset_test_env() {
    if let Some(manager) = RocksDbManager::maybe_get() {
        let _ = manager.reset().await;
    }
}

/// Result type for single iteration.
type IterationResult = std::result::Result<usize, (SimulationError, Option<SimulationTrace>)>;

/// Runs a single simulation iteration and returns the number of steps or an error with trace.
async fn run_single_iteration(
    seed: u64,
    num_invocations: usize,
    max_steps: usize,
    storage: PartitionStore,
) -> IterationResult {
    let config = PartitionSimulationConfig {
        seed,
        max_steps,
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

    // Enqueue invocations
    for _ in 0..num_invocations {
        let invocation = sim.random_vo_invocation();
        sim.enqueue_invocation(invocation);
    }

    match sim.run().await {
        Ok(outcome) => {
            if outcome.success {
                Ok(outcome.steps_executed)
            } else {
                let trace = sim.take_trace();
                let error = if let Some(violation) = outcome.violations.into_iter().next() {
                    SimulationError::Invariant(violation)
                } else {
                    SimulationError::NoPendingWork
                };
                Err((error, trace))
            }
        }
        Err(e) => {
            let trace = sim.take_trace();
            Err((e, trace))
        }
    }
}
