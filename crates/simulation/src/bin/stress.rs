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
//! Run with multiple workers (uses separate partitions for each worker):
//! ```bash
//! cargo run -p restate-simulation --bin simulation-stress --features stress-bin -- --workers 4
//! ```

use std::num::NonZeroUsize;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use parking_lot::Mutex;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tracing::info;

use restate_core::TaskCenterBuilder;
use restate_partition_store::{PartitionStore, PartitionStoreManager};
use restate_rocksdb::RocksDbManager;
use restate_simulation::{
    ClusterSimulation, ClusterSimulationConfig, InvokerBehavior, PartitionSimulation,
    PartitionSimulationConfig, SimulationError, SimulationTrace,
};
use restate_types::Version;
use restate_types::config::{Configuration, StorageOptions, set_current_config};
use restate_types::identifiers::{ClusterSnapshotId, PartitionId, PartitionKey};
use restate_types::partition_table::PartitionTable;
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

    /// Number of parallel workers (default: number of CPUs, 1 for reproduction mode)
    #[arg(long)]
    workers: Option<usize>,

    /// Run multi-partition cluster simulation with snapshot protocol
    #[arg(long)]
    cluster: bool,

    /// Number of partitions in cluster mode (default: 3)
    #[arg(long, default_value = "3")]
    partitions: u16,

    /// Number of snapshots to take per cluster iteration (default: 2)
    #[arg(long, default_value = "2")]
    snapshots: u32,
}

/// Shared state for coordinating workers
struct SharedState {
    /// Signal to stop all workers
    stop: AtomicBool,
    /// Total iterations completed across all workers
    total_iterations: AtomicU64,
    /// Total steps executed across all workers
    total_steps: AtomicUsize,
    /// First failure encountered (if any)
    failure: Mutex<Option<FailureInfo>>,
    /// The shared PartitionStoreManager
    manager: Arc<PartitionStoreManager>,
}

struct FailureInfo {
    worker_id: usize,
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

    // Determine number of workers
    let num_workers = args.workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });

    // For reproduction with fixed seed, use single worker
    let num_workers = if args.seed.is_some() { 1 } else { num_workers };

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
        "║ Workers:       {:>10}                                           ║",
        num_workers
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
        println!("║ Mode:          REPRODUCTION (fixed seed, single worker)             ║");
    } else if args.cluster {
        println!("║ Mode:          CLUSTER (multi-partition with snapshots)             ║");
        println!(
            "║ Partitions:    {:>10}                                           ║",
            args.partitions
        );
        println!(
            "║ Snapshots/iter:{:>10}                                           ║",
            args.snapshots
        );
    } else {
        println!("║ Mode:          EXPLORATION (random seeds, parallel)                 ║");
    }
    println!("╚══════════════════════════════════════════════════════════════════════╝");
    println!();

    // Initialize RocksDB and PartitionStoreManager on the main thread
    // This needs a tokio runtime and TaskCenter for async operations
    let init_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let init_tc = TaskCenterBuilder::default()
        .default_runtime_handle(init_rt.handle().clone())
        .build()?;

    let manager = init_tc
        .block_on(async {
            // Configure RocksDB with a large memory budget
            let mut config = Configuration::default();
            config.common.rocksdb_total_memory_size =
                NonZeroUsize::new(4 * 1024 * 1024 * 1024).unwrap();
            let config = config.apply_cascading_values();
            set_current_config(config);

            RocksDbManager::init();
            let storage_options = StorageOptions::default();
            info!(
                "Using RocksDB temp directory {}",
                storage_options.data_dir("db").display()
            );

            PartitionStoreManager::create().await
        })
        .expect("TaskCenter panicked")
        .unwrap();

    let shared = Arc::new(SharedState {
        stop: AtomicBool::new(false),
        total_iterations: AtomicU64::new(0),
        total_steps: AtomicUsize::new(0),
        failure: Mutex::new(None),
        manager,
    });

    let start = Instant::now();

    let cluster_mode = args.cluster;
    let num_partitions = args.partitions;
    let num_snapshots = args.snapshots;

    // Spawn worker threads
    let handles: Vec<_> = (0..num_workers)
        .map(|worker_id| {
            let shared = Arc::clone(&shared);
            let worker_seed = master_seed.wrapping_add(worker_id as u64);

            thread::spawn(move || {
                run_worker(
                    worker_id,
                    worker_seed,
                    duration,
                    args.invocations,
                    args.max_steps,
                    is_reproduction,
                    cluster_mode,
                    num_partitions,
                    num_snapshots,
                    shared,
                )
            })
        })
        .collect();

    // Progress reporting on main thread
    let progress_shared = Arc::clone(&shared);
    while start.elapsed() < duration && !shared.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));
        let elapsed = start.elapsed();
        let remaining = duration.saturating_sub(elapsed);
        let iterations = progress_shared.total_iterations.load(Ordering::Relaxed);
        let steps = progress_shared.total_steps.load(Ordering::Relaxed);
        println!(
            "[{:>6.1}s] Iterations: {:>8} | Steps: {:>10} | Steps/s: {:>8.0} | Remaining: {:>6.1}s",
            elapsed.as_secs_f64(),
            iterations,
            steps,
            steps as f64 / elapsed.as_secs_f64(),
            remaining.as_secs_f64()
        );
    }

    // Signal workers to stop
    shared.stop.store(true, Ordering::Relaxed);

    // Wait for all workers
    for handle in handles {
        let _ = handle.join();
    }

    // Shutdown RocksDB
    let _ = init_tc.block_on(async {
        if let Some(manager) = RocksDbManager::maybe_get() {
            manager.shutdown().await;
        }
    });

    let total_iterations = shared.total_iterations.load(Ordering::Relaxed);
    let total_steps = shared.total_steps.load(Ordering::Relaxed);
    let elapsed_secs = start.elapsed().as_secs_f64();

    // Check for failures
    if let Some(failure) = shared.failure.lock().take() {
        print_failure(&failure);
        anyhow::bail!(
            "Simulation failed at iteration {} with seed {}",
            failure.iteration,
            failure.seed
        );
    }

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║                    ✅ STRESS TEST COMPLETED                          ║");
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!(
        "║ Duration:      {:>10.1} seconds                                   ║",
        elapsed_secs
    );
    println!(
        "║ Workers:       {:>10}                                           ║",
        num_workers
    );
    println!(
        "║ Iterations:    {:>10}                                           ║",
        total_iterations
    );
    println!(
        "║ Total steps:   {:>10}                                           ║",
        total_steps
    );
    println!(
        "║ Steps/second:  {:>10.0}                                           ║",
        total_steps as f64 / elapsed_secs
    );
    println!(
        "║ Master seed:   {:>20}                             ║",
        master_seed
    );
    println!("╚══════════════════════════════════════════════════════════════════════╝");

    Ok(())
}

fn print_failure(failure: &FailureInfo) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║                    ❌ SIMULATION FAILURE DETECTED                    ║");
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!(
        "║ Worker:        {:>10}                                           ║",
        failure.worker_id
    );
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

/// Worker function that runs on a separate OS thread with its own tokio runtime.
#[allow(clippy::too_many_arguments)]
fn run_worker(
    worker_id: usize,
    worker_seed: u64,
    duration: Duration,
    num_invocations: usize,
    max_steps: usize,
    is_reproduction: bool,
    cluster_mode: bool,
    num_partitions: u16,
    num_snapshots: u32,
    shared: Arc<SharedState>,
) {
    // Each worker gets its own single-threaded tokio runtime with paused time
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("Failed to create tokio runtime");

    let tc = TaskCenterBuilder::default()
        .default_runtime_handle(rt.handle().clone())
        .pause_time(true)
        .build()
        .expect("Failed to create TaskCenter");

    let _ = tc.block_on(async {
        run_worker_loop(
            worker_id,
            worker_seed,
            duration,
            num_invocations,
            max_steps,
            is_reproduction,
            cluster_mode,
            num_partitions,
            num_snapshots,
            shared,
        )
        .await
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_loop(
    worker_id: usize,
    worker_seed: u64,
    duration: Duration,
    num_invocations: usize,
    max_steps: usize,
    is_reproduction: bool,
    cluster_mode: bool,
    num_partitions: u16,
    num_snapshots: u32,
    shared: Arc<SharedState>,
) {
    let start = Instant::now();

    // Create RNG for iteration seeds
    let mut rng = StdRng::seed_from_u64(worker_seed);

    if cluster_mode {
        // Cluster mode: open N partition stores
        let partition_table =
            PartitionTable::with_equally_sized_partitions(Version::MIN, num_partitions);
        let mut store_map = std::collections::HashMap::new();
        // Use worker_id * num_partitions offset to avoid partition ID collisions across workers
        for (pid, partition) in partition_table.iter() {
            let store = shared
                .manager
                .open(partition, None)
                .await
                .expect("Failed to open partition store");
            store_map.insert(*pid, store);
        }
        let store_map = Arc::new(store_map);

        let mut iteration_counter = 0u64;
        while start.elapsed() < duration && !shared.stop.load(Ordering::Relaxed) {
            let iteration_seed = if is_reproduction {
                worker_seed
            } else {
                rng.random()
            };
            iteration_counter += 1;

            let result = run_cluster_iteration(
                iteration_seed,
                num_invocations,
                max_steps,
                num_partitions,
                num_snapshots,
                partition_table.clone(),
                Arc::clone(&store_map),
            )
            .await;

            match result {
                Ok(steps) => {
                    shared.total_iterations.fetch_add(1, Ordering::Relaxed);
                    shared.total_steps.fetch_add(steps, Ordering::Relaxed);
                }
                Err(error) => {
                    let mut failure_guard = shared.failure.lock();
                    if failure_guard.is_none() {
                        *failure_guard = Some(FailureInfo {
                            worker_id,
                            iteration: iteration_counter,
                            seed: iteration_seed,
                            error: error.to_string(),
                            trace: None,
                        });
                    }
                    shared.stop.store(true, Ordering::Relaxed);
                    break;
                }
            }

            if is_reproduction {
                break;
            }
        }
    } else {
        // Single-partition mode (original behavior)
        let partition_id = PartitionId::from(worker_id as u16);
        let partition = Partition::new(
            partition_id,
            RangeInclusive::new(PartitionKey::MIN, PartitionKey::MAX),
        );

        let storage = shared
            .manager
            .open(&partition, None)
            .await
            .expect("Failed to open partition store");

        let mut iteration_counter = 0u64;
        while start.elapsed() < duration && !shared.stop.load(Ordering::Relaxed) {
            let iteration_seed = if is_reproduction {
                worker_seed
            } else {
                rng.random()
            };
            iteration_counter += 1;

            let result =
                run_single_iteration(iteration_seed, num_invocations, max_steps, storage.clone())
                    .await;

            match result {
                Ok(steps) => {
                    shared.total_iterations.fetch_add(1, Ordering::Relaxed);
                    shared.total_steps.fetch_add(steps, Ordering::Relaxed);
                }
                Err((error, trace)) => {
                    let mut failure_guard = shared.failure.lock();
                    if failure_guard.is_none() {
                        *failure_guard = Some(FailureInfo {
                            worker_id,
                            iteration: iteration_counter,
                            seed: iteration_seed,
                            error: format!("{}", error),
                            trace,
                        });
                    }
                    shared.stop.store(true, Ordering::Relaxed);
                    break;
                }
            }

            if is_reproduction {
                break;
            }
        }
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
        ..Default::default()
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

/// Runs a single cluster simulation iteration with snapshots.
///
/// Creates a multi-partition cluster, injects invocations, runs to process them,
/// then takes N snapshots and verifies invariants after each.
async fn run_cluster_iteration(
    seed: u64,
    num_invocations: usize,
    max_steps: usize,
    num_partitions: u16,
    num_snapshots: u32,
    partition_table: PartitionTable,
    store_map: Arc<std::collections::HashMap<PartitionId, PartitionStore>>,
) -> std::result::Result<usize, String> {
    let cluster_config = ClusterSimulationConfig {
        num_partitions,
        seed,
        max_steps,
        ..Default::default()
    };

    let mut cluster = ClusterSimulation::new(
        cluster_config,
        partition_table,
        |pid| store_map.get(&pid).unwrap().clone(),
        InvokerBehavior::Probabilistic {
            success_rate: 0.6,
            failure_rate: 0.3,
        },
    );

    // Inject invocations
    for _ in 0..num_invocations {
        let invocation = cluster.partition_mut(0).random_vo_invocation();
        cluster.inject_invocation(invocation);
    }

    // Run initial processing
    let outcome = cluster.run().await.map_err(|e| format!("{e}"))?;
    if !outcome.is_ok() {
        return Err(format!(
            "Pre-snapshot invariant violations: {:?}",
            outcome.violations
        ));
    }
    let mut total = outcome.total_steps;

    // Take N snapshots with work in between
    for snap_idx in 1..=num_snapshots {
        // Inject more work between snapshots
        for _ in 0..num_invocations / 2 {
            let invocation = cluster.partition_mut(0).random_vo_invocation();
            cluster.inject_invocation(invocation);
        }

        cluster.initiate_snapshot(ClusterSnapshotId::new(snap_idx as u64));
        let outcome = cluster.run().await.map_err(|e| format!("{e}"))?;

        if !outcome.is_ok() {
            return Err(format!(
                "Snapshot {snap_idx} invariant violations: {:?}",
                outcome.violations
            ));
        }

        // Verify all partitions completed this snapshot
        for (i, completions) in cluster.completed_snapshots().iter().enumerate() {
            if !completions.contains(&ClusterSnapshotId::new(snap_idx as u64)) {
                return Err(format!(
                    "Partition {i} did not complete snapshot {snap_idx}"
                ));
            }
        }

        total += outcome.total_steps;
    }

    Ok(total)
}
