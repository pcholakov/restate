// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Multi-partition cluster simulation.
//!
//! Orchestrates N [`PartitionSimulation`] instances with cross-partition
//! message routing and snapshot protocol coordination.

use std::collections::VecDeque;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use restate_storage_api::Storage;
use restate_storage_api::invocation_status_table::ReadInvocationStatusTable;
use restate_storage_api::journal_table_v2::ReadJournalTable;
use restate_storage_api::service_status_table::ReadVirtualObjectStatusTable;
use restate_types::identifiers::{ClusterSnapshotId, PartitionId};
use restate_types::partition_table::PartitionTable;
use restate_wal_protocol::Command;

use crate::partition::{
    InvokerBehavior, PartitionSimulation, PartitionSimulationConfig, SimulationError,
};

/// Scheduling strategy for picking which partition to step next.
#[derive(Debug, Clone, Default)]
pub enum StepScheduler {
    /// Step partitions in round-robin order.
    #[default]
    RoundRobin,
    /// Randomly pick a partition that has pending work.
    Random,
}

/// Configuration for a cluster simulation.
#[derive(Debug, Clone)]
pub struct ClusterSimulationConfig {
    /// Number of partitions.
    pub num_partitions: u16,
    /// Random seed for deterministic execution.
    pub seed: u64,
    /// Maximum total steps across all partitions.
    pub max_steps: usize,
    /// Scheduling strategy.
    pub scheduler: StepScheduler,
}

impl Default for ClusterSimulationConfig {
    fn default() -> Self {
        Self {
            num_partitions: 3,
            seed: 0,
            max_steps: 50_000,
            scheduler: StepScheduler::default(),
        }
    }
}

/// In-flight messages queued for delivery to a partition.
struct PartitionMailbox {
    messages: VecDeque<Command>,
}

impl PartitionMailbox {
    fn new() -> Self {
        Self {
            messages: VecDeque::new(),
        }
    }
}

/// Multi-partition cluster simulation.
///
/// Drives N partition simulations with cross-partition message routing.
/// Messages are routed via mailboxes: each partition's outbound messages
/// are placed into the target partition's mailbox and delivered before
/// the next step.
pub struct ClusterSimulation<S> {
    config: ClusterSimulationConfig,
    rng: StdRng,
    partition_table: PartitionTable,
    partitions: Vec<PartitionSimulation<S>>,
    mailboxes: Vec<PartitionMailbox>,
    /// Total steps executed across all partitions.
    total_steps: usize,
    /// Snapshot IDs completed by each partition (partition index → snapshot IDs).
    completed_snapshots: Vec<Vec<ClusterSnapshotId>>,
    /// Round-robin cursor.
    rr_cursor: usize,
}

impl<S> ClusterSimulation<S>
where
    S: Storage
        + ReadInvocationStatusTable
        + ReadVirtualObjectStatusTable
        + ReadJournalTable
        + Send
        + Clone,
{
    /// Creates a new cluster simulation.
    ///
    /// `storage_factory` is called once per partition to create isolated storage.
    /// `invoker_behavior` applies to all partitions uniformly (ImmediateSuccess
    /// is a common choice for multi-partition tests).
    pub fn new<Fs>(
        config: ClusterSimulationConfig,
        partition_table: PartitionTable,
        storage_factory: Fs,
        invoker_behavior: InvokerBehavior,
    ) -> Self
    where
        Fs: Fn(PartitionId) -> S,
    {
        let invoker_factory = |_pid: PartitionId| match &invoker_behavior {
            InvokerBehavior::ImmediateSuccess => InvokerBehavior::ImmediateSuccess,
            InvokerBehavior::ImmediateFail {
                error_code,
                message,
            } => InvokerBehavior::ImmediateFail {
                error_code: *error_code,
                message: message.clone(),
            },
            InvokerBehavior::Probabilistic {
                success_rate,
                failure_rate,
            } => InvokerBehavior::Probabilistic {
                success_rate: *success_rate,
                failure_rate: *failure_rate,
            },
            InvokerBehavior::RandomJournal {
                min_entries,
                max_entries,
            } => InvokerBehavior::RandomJournal {
                min_entries: *min_entries,
                max_entries: *max_entries,
            },
            InvokerBehavior::Custom(_) => {
                panic!(
                    "Custom InvokerBehavior is not supported in ClusterSimulation; use Probabilistic or provide a factory via new_with_invoker_factory"
                )
            }
        };
        Self::new_with_invoker_factory(config, partition_table, storage_factory, invoker_factory)
    }

    /// Creates a cluster simulation with per-partition invoker control.
    pub fn new_with_invoker_factory<Fs, Fi>(
        config: ClusterSimulationConfig,
        partition_table: PartitionTable,
        storage_factory: Fs,
        invoker_factory: Fi,
    ) -> Self
    where
        Fs: Fn(PartitionId) -> S,
        Fi: Fn(PartitionId) -> InvokerBehavior,
    {
        let rng = StdRng::seed_from_u64(config.seed);

        let mut partitions = Vec::with_capacity(config.num_partitions as usize);
        let mut mailboxes = Vec::with_capacity(config.num_partitions as usize);

        for (pid, partition) in partition_table.iter() {
            let partition_config = PartitionSimulationConfig {
                seed: config.seed.wrapping_add(u64::from(*pid)),
                max_steps: config.max_steps,
                partition_id: *pid,
                partition_key_range: partition.key_range.clone(),
                check_invariants: true,
            };
            let storage = storage_factory(*pid);
            let behavior = invoker_factory(*pid);
            let sim = PartitionSimulation::with_partition_table(
                partition_config,
                storage,
                partition_table.clone(),
                behavior,
            );
            partitions.push(sim);
            mailboxes.push(PartitionMailbox::new());
        }

        let num = config.num_partitions as usize;
        ClusterSimulation {
            config,
            rng,
            partition_table,
            partitions,
            mailboxes,
            total_steps: 0,
            completed_snapshots: vec![Vec::new(); num],
            rr_cursor: 0,
        }
    }

    /// Returns the partition table used by this cluster.
    pub fn partition_table(&self) -> &PartitionTable {
        &self.partition_table
    }

    /// Returns total steps executed across all partitions.
    pub fn total_steps(&self) -> usize {
        self.total_steps
    }

    /// Returns completed snapshot IDs per partition.
    pub fn completed_snapshots(&self) -> &[Vec<ClusterSnapshotId>] {
        &self.completed_snapshots
    }

    /// Gets a mutable reference to a specific partition.
    pub fn partition_mut(&mut self, idx: usize) -> &mut PartitionSimulation<S> {
        &mut self.partitions[idx]
    }

    /// Injects a command into a specific partition's mailbox.
    pub fn inject_command(&mut self, partition_idx: usize, command: Command) {
        self.mailboxes[partition_idx].messages.push_back(command);
    }

    /// Initiates a distributed snapshot by sending InitiateSnapshot to all partitions.
    pub fn initiate_snapshot(&mut self, snapshot_id: ClusterSnapshotId) {
        let num_partitions = self.config.num_partitions as u32;
        for mailbox in &mut self.mailboxes {
            mailbox.messages.push_back(Command::InitiateSnapshot {
                snapshot_id,
                num_partitions,
            });
        }
    }

    /// Delivers pending mailbox messages to each partition.
    fn deliver_mailboxes(&mut self) {
        for (idx, mailbox) in self.mailboxes.iter_mut().enumerate() {
            for msg in mailbox.messages.drain(..) {
                self.partitions[idx].enqueue_command(msg);
            }
        }
    }

    /// Routes outbound messages from all partitions into target mailboxes.
    fn route_outbound(&mut self) {
        for idx in 0..self.partitions.len() {
            let outbound = self.partitions[idx].drain_outbound();
            for msg in outbound {
                let target_idx = self.partition_index(msg.target_partition_id);
                self.mailboxes[target_idx].messages.push_back(msg.command);
            }

            let completions = self.partitions[idx].drain_snapshot_completions();
            self.completed_snapshots[idx].extend(completions);
        }
    }

    /// Maps a PartitionId to its index in the partitions vec.
    fn partition_index(&self, pid: PartitionId) -> usize {
        let idx = u64::from(pid) as usize;
        debug_assert!(
            idx < self.partitions.len(),
            "PartitionId {pid} out of range for {}-partition cluster",
            self.partitions.len()
        );
        idx
    }

    /// Picks the next partition to step based on the scheduling strategy.
    fn pick_partition(&mut self) -> Option<usize> {
        let n = self.partitions.len();
        match self.config.scheduler {
            StepScheduler::RoundRobin => {
                for _ in 0..n {
                    let idx = self.rr_cursor % n;
                    self.rr_cursor += 1;
                    if self.partitions[idx].has_pending_commands() {
                        return Some(idx);
                    }
                }
                None
            }
            StepScheduler::Random => {
                // Collect indices of partitions with pending work
                let candidates: Vec<usize> = (0..n)
                    .filter(|&i| self.partitions[i].has_pending_commands())
                    .collect();
                if candidates.is_empty() {
                    None
                } else {
                    let pick = self.rng.random_range(0..candidates.len());
                    Some(candidates[pick])
                }
            }
        }
    }

    /// Executes one step: deliver mailboxes, pick a partition, step it, route outbound.
    pub async fn step(&mut self) -> Result<Option<usize>, SimulationError> {
        if self.total_steps >= self.config.max_steps {
            return Ok(None);
        }

        self.deliver_mailboxes();

        // Advance timers on all partitions that have no pending commands
        // but do have pending timers
        for partition in &mut self.partitions {
            if !partition.has_pending_commands() {
                partition.advance_timers().await?;
            }
        }

        let Some(idx) = self.pick_partition() else {
            return Ok(None);
        };

        self.partitions[idx].step().await?;
        self.total_steps += 1;

        self.route_outbound();
        Ok(Some(idx))
    }

    /// Runs the cluster simulation until all partitions quiesce or max_steps is reached.
    pub async fn run(&mut self) -> Result<ClusterSimulationOutcome, SimulationError> {
        loop {
            self.deliver_mailboxes();

            // Advance timers globally
            let mut any_timer_fired = false;
            for partition in &mut self.partitions {
                if !partition.has_pending_commands() && partition.advance_timers().await? {
                    any_timer_fired = true;
                }
            }

            if self.total_steps >= self.config.max_steps {
                break;
            }

            let Some(idx) = self.pick_partition() else {
                // No partition has pending commands. If timers fired this
                // iteration, retry (they may have produced commands). Otherwise
                // there's no more work to do.
                if any_timer_fired {
                    continue;
                }
                break;
            };

            self.partitions[idx].step().await?;
            self.total_steps += 1;

            self.route_outbound();
        }

        Ok(ClusterSimulationOutcome {
            total_steps: self.total_steps,
            completed_snapshots: self.completed_snapshots.clone(),
        })
    }
}

/// Outcome of running a cluster simulation.
#[derive(Debug)]
pub struct ClusterSimulationOutcome {
    pub total_steps: usize,
    pub completed_snapshots: Vec<Vec<ClusterSnapshotId>>,
}
