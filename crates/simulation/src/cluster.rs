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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use restate_partition_store::PartitionStore;
use restate_partition_store::snapshots::LocalPartitionSnapshot;
use restate_storage_api::Storage;
use restate_storage_api::invocation_status_table::ReadInvocationStatusTable;
use restate_storage_api::journal_table_v2::ReadJournalTable;
use restate_storage_api::service_status_table::ReadVirtualObjectStatusTable;
use restate_types::identifiers::{ClusterSnapshotId, InvocationId, PartitionId, WithPartitionKey};
use restate_types::invocation::ServiceInvocation;
use restate_types::partition_table::{FindPartition, PartitionTable};
use restate_wal_protocol::Command;

use crate::partition::{
    InvariantViolation, InvokerBehavior, PartitionSimulation, PartitionSimulationConfig,
    SimulationError,
};

/// Fault injection mode for the cluster simulation.
///
/// Used by trip-wire tests to intentionally break the snapshot protocol
/// and verify that invariant checkers detect the violation.
#[derive(Debug, Clone, Default)]
pub enum FaultInjection {
    /// No faults — normal operation.
    #[default]
    None,
    /// Drop all `SnapshotMarker` commands during routing.
    /// This prevents partitions from receiving markers, so the snapshot
    /// protocol can never complete. The completion-agreement checker should fire.
    DropSnapshotMarkers,
    /// Drop all cross-partition messages (markers, invocations, responses, etc).
    /// The message accounting checker should fire.
    DropAllMessages,
}

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
    /// Fault injection mode for testing invariant checkers.
    pub fault_injection: FaultInjection,
}

impl Default for ClusterSimulationConfig {
    fn default() -> Self {
        Self {
            num_partitions: 3,
            seed: 0,
            max_steps: 50_000,
            scheduler: StepScheduler::default(),
            fault_injection: FaultInjection::default(),
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

/// Per-channel (source → target) message statistics.
///
/// Models the Bifrost-backed FIFO channels between partitions. In production,
/// the shuffle reads from the source's outbox, wraps messages in envelopes
/// with dedup info, and appends them to the target's Bifrost log. This struct
/// tracks the equivalent message flow in the simulation.
#[derive(Debug, Default)]
pub struct ChannelStats {
    /// Number of application messages routed through this channel.
    pub app_messages: u64,
    /// Number of snapshot markers routed through this channel.
    pub markers: u64,
    /// Number of outbox acks routed through this channel.
    pub acks: u64,
    /// Number of messages dropped by fault injection.
    pub dropped: u64,
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
    /// Per-channel (source → target) message statistics. Key: (source_idx, target_idx).
    channel_stats: HashMap<(usize, usize), ChannelStats>,
    /// Round-robin cursor.
    rr_cursor: usize,
    /// Invocations injected via `inject_invocation`, with their target partition.
    injected_invocations: Vec<(InvocationId, PartitionId)>,
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
            channel_stats: HashMap::new(),
            rr_cursor: 0,
            injected_invocations: Vec::new(),
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

    /// Returns per-channel (source_idx → target_idx) message statistics.
    pub fn channel_stats(&self) -> &HashMap<(usize, usize), ChannelStats> {
        &self.channel_stats
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

    /// Routes an invocation to the correct partition based on its partition key.
    /// Returns the invocation ID and target partition for tracking.
    pub fn inject_invocation(
        &mut self,
        invocation: ServiceInvocation,
    ) -> (InvocationId, PartitionId) {
        let invocation_id = invocation.invocation_id;
        let partition_key = invocation.partition_key();
        let target_pid = self
            .partition_table
            .find_partition_id(partition_key)
            .expect("partition key should map to a valid partition");
        let target_idx = self.partition_index(target_pid);
        self.injected_invocations.push((invocation_id, target_pid));
        self.mailboxes[target_idx]
            .messages
            .push_back(Command::Invoke(Box::new(invocation)));
        (invocation_id, target_pid)
    }

    /// Returns all injected invocations with their target partitions.
    pub fn injected_invocations(&self) -> &[(InvocationId, PartitionId)] {
        &self.injected_invocations
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
    ///
    /// Applies fault injection if configured — dropped messages are still
    /// counted as "sent" by the source partition (via `drain_outbound`),
    /// but never delivered, so invariant checkers can detect the loss.
    fn route_outbound(&mut self) {
        for idx in 0..self.partitions.len() {
            let outbound = self.partitions[idx].drain_outbound();
            for msg in outbound {
                let target_idx = self.partition_index(msg.target_partition_id);
                let stats = self.channel_stats.entry((idx, target_idx)).or_default();

                // Apply fault injection
                let should_drop = match &self.config.fault_injection {
                    FaultInjection::None => false,
                    FaultInjection::DropSnapshotMarkers => {
                        matches!(msg.command, Command::SnapshotMarker { .. })
                    }
                    FaultInjection::DropAllMessages => true,
                };

                // Classify message for per-channel tracking
                match &msg.command {
                    Command::SnapshotMarker { .. } => stats.markers += 1,
                    Command::OutboxProcessedAck { .. } => stats.acks += 1,
                    _ => stats.app_messages += 1,
                }

                if should_drop {
                    stats.dropped += 1;
                } else {
                    self.partitions[target_idx].record_inbound_message();
                    self.mailboxes[target_idx].messages.push_back(msg.command);
                }
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

        // Run cluster-level invariant checks on the final state.
        let violations = self.check_cluster_invariants();

        Ok(ClusterSimulationOutcome {
            total_steps: self.total_steps,
            completed_snapshots: self.completed_snapshots.clone(),
            violations,
        })
    }

    /// Checks cluster-level invariants after the simulation quiesces.
    ///
    /// Verifies:
    /// 1. **Snapshot completion agreement** — all partitions completed the same set of snapshot IDs
    /// 2. **Marker delivery** — for each completed snapshot, every partition received markers from all others
    /// 3. **Message accounting** — total cross-partition messages sent equals total received
    pub fn check_cluster_invariants(&self) -> Vec<InvariantViolation> {
        let mut violations = Vec::new();

        self.check_snapshot_completion_agreement(&mut violations);
        self.check_marker_delivery(&mut violations);
        self.check_message_accounting(&mut violations);

        violations
    }

    /// Verifies that all partitions agree on which snapshots completed.
    fn check_snapshot_completion_agreement(&self, violations: &mut Vec<InvariantViolation>) {
        // Collect all snapshot IDs seen across any partition
        let mut all_snapshot_ids: HashSet<ClusterSnapshotId> = HashSet::new();
        for partition_snapshots in &self.completed_snapshots {
            for sid in partition_snapshots {
                all_snapshot_ids.insert(*sid);
            }
        }

        // For each snapshot ID, verify all partitions completed it
        for &sid in &all_snapshot_ids {
            let mut completed_by = Vec::new();
            let mut missing = Vec::new();

            for (idx, partition_snapshots) in self.completed_snapshots.iter().enumerate() {
                let pid = self.partitions[idx].partition_id();
                if partition_snapshots.contains(&sid) {
                    completed_by.push(pid);
                } else {
                    missing.push(pid);
                }
            }

            if !missing.is_empty() {
                violations.push(InvariantViolation::SnapshotCompletionDisagreement {
                    snapshot_id: sid,
                    completed_by,
                    missing,
                });
            }
        }
    }

    /// Verifies that for each completed snapshot, markers were properly exchanged.
    ///
    /// For a completed snapshot, each partition should have:
    /// - Sent markers to every other partition
    /// - Received markers from every other partition
    ///
    /// **Assumes full-mesh topology**: all partitions are connected and exchange
    /// markers directly. Self-markers are not expected (a partition does not
    /// send a marker to itself; it records its local snapshot via
    /// `LocalSnapshotTaken` instead).
    fn check_marker_delivery(&self, violations: &mut Vec<InvariantViolation>) {
        // Collect completed snapshot IDs (only those completed by ALL partitions)
        let mut globally_completed: HashMap<ClusterSnapshotId, usize> = HashMap::new();
        for partition_snapshots in &self.completed_snapshots {
            for sid in partition_snapshots {
                *globally_completed.entry(*sid).or_default() += 1;
            }
        }

        let n = self.partitions.len();
        for (&sid, &count) in &globally_completed {
            if count != n {
                continue; // Already reported by completion agreement check
            }

            // For each partition, verify it received markers from all other partitions
            for (idx, partition) in self.partitions.iter().enumerate() {
                let pid = partition.partition_id();
                let received = partition.markers_received();

                if let Some(received_from) = received.get(&sid) {
                    // Check each other partition sent a marker to this one
                    for (other_idx, other_partition) in self.partitions.iter().enumerate() {
                        if other_idx == idx {
                            continue;
                        }
                        let other_pid = other_partition.partition_id();
                        if !received_from.contains(&other_pid) {
                            violations.push(InvariantViolation::SnapshotMarkerNotDelivered {
                                snapshot_id: sid,
                                sender: other_pid,
                                expected_recipient: pid,
                            });
                        }
                    }
                } else {
                    // No markers received at all for this snapshot — report each missing sender
                    for (other_idx, other_partition) in self.partitions.iter().enumerate() {
                        if other_idx == idx {
                            continue;
                        }
                        violations.push(InvariantViolation::SnapshotMarkerNotDelivered {
                            snapshot_id: sid,
                            sender: other_partition.partition_id(),
                            expected_recipient: pid,
                        });
                    }
                }
            }
        }
    }

    /// Verifies that total cross-partition messages sent equals total received.
    ///
    /// This detects any message loss, including loss caused by fault injection.
    /// Trip wire tests rely on this to verify that the invariant checker fires
    /// when messages are intentionally dropped.
    fn check_message_accounting(&self, violations: &mut Vec<InvariantViolation>) {
        let mut total_sent = 0u64;
        let mut total_received = 0u64;

        for partition in &self.partitions {
            let (sent, received) = partition.cross_partition_message_counts();
            total_sent += sent;
            total_received += received;
        }

        if total_sent != total_received {
            violations.push(InvariantViolation::CrossPartitionMessageLoss {
                total_sent,
                total_received,
            });
        }
    }
}

/// Specialized methods for `PartitionStore`-backed cluster simulations that
/// support RocksDB checkpoint collection during snapshot runs.
impl ClusterSimulation<PartitionStore> {
    /// Runs the cluster simulation like [`run()`](ClusterSimulation::run), but
    /// also collects RocksDB checkpoints for every partition that participates
    /// in a distributed snapshot. Returns both the simulation outcome and a
    /// map from `PartitionId` to `LocalPartitionSnapshot`.
    pub async fn run_with_checkpoints(
        &mut self,
        snapshot_base_path: &Path,
    ) -> Result<
        (
            ClusterSimulationOutcome,
            HashMap<PartitionId, LocalPartitionSnapshot>,
        ),
        SimulationError,
    > {
        let mut snapshots: HashMap<PartitionId, LocalPartitionSnapshot> = HashMap::new();

        loop {
            self.deliver_mailboxes();

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
                if any_timer_fired {
                    continue;
                }
                break;
            };

            self.partitions[idx].step().await?;
            self.total_steps += 1;

            // Collect checkpoint if this partition has a pending one
            if let Some((_, local_snapshot)) = self.partitions[idx]
                .take_pending_checkpoint(snapshot_base_path)
                .await?
            {
                let pid = self.partitions[idx].partition_id();
                snapshots.insert(pid, local_snapshot);
            }

            self.route_outbound();
        }

        let violations = self.check_cluster_invariants();
        let outcome = ClusterSimulationOutcome {
            total_steps: self.total_steps,
            completed_snapshots: self.completed_snapshots.clone(),
            violations,
        };

        Ok((outcome, snapshots))
    }
}

/// Outcome of running a cluster simulation.
#[derive(Debug)]
pub struct ClusterSimulationOutcome {
    pub total_steps: usize,
    pub completed_snapshots: Vec<Vec<ClusterSnapshotId>>,
    pub violations: Vec<InvariantViolation>,
}

impl ClusterSimulationOutcome {
    /// Returns true if no invariant violations were detected.
    pub fn is_ok(&self) -> bool {
        self.violations.is_empty()
    }
}
