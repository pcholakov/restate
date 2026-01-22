# DST Implementation Sketch: Distributed Snapshot Protocol

This document sketches the implementation needed to validate the Chandy-Lamport
distributed snapshot design (see `pitr-snapshot-design.md` and
`distributed-snapshot-walkthrough.txt`) using the deterministic simulation
testing framework in `.workspaces/dst/crates/simulation/`.

## Current State of DST

The simulation framework is **single-partition only**: one `PartitionSimulation`
drives a real `StateMachine`, converts `Action`s back to `Command`s in a
closed loop. It validates VO exclusivity, timer monotonicity, and journal
sequence invariants. See `crates/simulation/src/partition.rs`.

## What the Snapshot Protocol Requires

The CL-based snapshot protocol is inherently multi-partition. It requires:

1. Multiple partition processors, each with their own state machine and storage
2. Per-partition Bifrost logs with FIFO ordering
3. Shuffle routing: outbox messages → target partition's log
4. Acks flowing through Bifrost (not out-of-band)
5. Snapshot markers flowing through Bifrost
6. A coordinator that initiates and finalizes snapshots

None of these exist in the current DST framework.

---

## Part 1: Multi-Partition Simulation Infrastructure

### 1.1 SimulatedLog

A per-partition Bifrost log, providing FIFO append/read with LSN tracking.

```rust
/// Simulated Bifrost log for one partition. Provides FIFO ordering via
/// monotonic LSN assignment.
pub struct SimulatedLog {
    log_id: LogId,
    /// All records ever appended, indexed by LSN. Never trimmed during
    /// a simulation run (trimming is a separate concern).
    records: Vec<LogEntry>,
    /// Next LSN to assign on append.
    next_lsn: Lsn,
}

pub struct LogEntry {
    pub lsn: Lsn,
    pub envelope: Envelope,
    pub appended_at: MillisSinceEpoch,
}

impl SimulatedLog {
    pub fn append(&mut self, envelope: Envelope, now: MillisSinceEpoch) -> Lsn { ... }

    /// Read next unread record for a given reader cursor.
    pub fn read_from(&self, from_lsn: Lsn) -> Option<&LogEntry> { ... }

    /// All entries in LSN range, for invariant checking.
    pub fn entries_in_range(&self, range: RangeInclusive<Lsn>) -> &[LogEntry] { ... }

    pub fn tail_lsn(&self) -> Lsn { ... }
}
```

### 1.2 SimulatedShuffle

Routes outbox messages to the correct partition's log. This replaces the real
shuffle's Bifrost append with a direct push into `SimulatedLog`.

```rust
/// Per-partition shuffle that reads outbox actions and routes them to target logs.
pub struct SimulatedShuffle {
    source_partition_id: PartitionId,
    leader_epoch: LeaderEpoch,
    /// Next outbox sequence number to send. Tracks what has been
    /// "sent" (appended to a target log) vs. what's still pending.
    next_send_seq: MessageIndex,
}

impl SimulatedShuffle {
    /// Process a NewOutboxMessage action: wrap in Envelope with dedup info,
    /// append to the target partition's SimulatedLog.
    pub fn route_outbox_message(
        &mut self,
        seq_number: MessageIndex,
        message: OutboxMessage,
        target_partition_id: PartitionId,
        logs: &mut HashMap<PartitionId, SimulatedLog>,
        now: MillisSinceEpoch,
    ) -> Lsn { ... }

    /// Send a SnapshotMarker to a target partition's log.
    pub fn send_marker(
        &mut self,
        snapshot_id: ClusterSnapshotId,
        target: PartitionId,
        logs: &mut HashMap<PartitionId, SimulatedLog>,
        now: MillisSinceEpoch,
    ) -> Lsn { ... }

    /// Send an OutboxProcessedAck to the source partition's log.
    pub fn send_ack(
        &mut self,
        ack: OutboxProcessedAck,
        target: PartitionId,
        logs: &mut HashMap<PartitionId, SimulatedLog>,
        now: MillisSinceEpoch,
    ) -> Lsn { ... }
}
```

### 1.3 PartitionSim (Extended)

Wrap the existing `PartitionSimulation` with multi-partition awareness:

```rust
/// Extended per-partition simulation state for the multi-partition case.
pub struct PartitionSim<S> {
    pub partition_id: PartitionId,
    /// The real state machine + storage, same as current PartitionSimulation.
    pub inner: PartitionSimulation<S>,
    /// This partition's shuffle.
    pub shuffle: SimulatedShuffle,
    /// Read cursor: next LSN to consume from this partition's log.
    pub read_cursor: Lsn,
    /// Snapshot protocol state (None if not participating in a snapshot).
    pub snapshot_state: Option<SnapshotProtocolState>,
    /// Whether outbound messages are gated (queued, not sent).
    pub outbound_gate_open: bool,
    /// Queued outbound actions while gate is closed.
    pub gated_actions: Vec<OutboundAction>,
}

/// An outbound action that was gated during snapshot.
pub enum OutboundAction {
    OutboxMessage { seq_number: MessageIndex, message: OutboxMessage, target: PartitionId },
    Ack(OutboxProcessedAck),
}
```

### 1.4 ClusterSimulation

The top-level driver that orchestrates multiple partitions.

```rust
pub struct ClusterSimulation<S> {
    pub config: ClusterSimConfig,
    pub rng: StdRng,
    pub clock: SimulationClock,
    /// Per-partition simulation state.
    pub partitions: HashMap<PartitionId, PartitionSim<S>>,
    /// Per-partition Bifrost logs.
    pub logs: HashMap<PartitionId, SimulatedLog>,
    /// Snapshot coordinator state.
    pub coordinator: SnapshotCoordinator,
    /// Partition key → partition id routing table.
    pub routing: PartitionKeyRouter,
    /// Step counter.
    pub steps: u64,
}

pub struct ClusterSimConfig {
    pub seed: u64,
    pub num_partitions: u32,
    pub max_steps: u64,
    pub check_invariants: bool,
    /// How to schedule which partition processes next.
    pub scheduling: SchedulingPolicy,
}

/// Controls the order in which partitions consume from their logs.
/// Randomized scheduling is the most interesting for finding bugs.
pub enum SchedulingPolicy {
    /// Round-robin across partitions.
    RoundRobin,
    /// Random partition selection each step.
    Random,
    /// Weighted: partitions with more pending records are more likely to be picked.
    WeightedByBacklog,
}
```

### 1.5 Cluster Step Loop

The main simulation loop. Each "step" picks a partition and processes one
record from its log (or fires a timer, or processes an outbox action).

```rust
impl<S> ClusterSimulation<S> {
    /// One simulation step: pick a partition, process one event.
    pub async fn step(&mut self) -> Result<ClusterStepResult, SimulationError> {
        // 1. Select a partition that has pending work
        let pid = self.select_partition();

        // 2. Determine what to do: read from log, fire timer, or drain outbox
        let sim = self.partitions.get_mut(&pid).unwrap();
        let log = self.logs.get_mut(&pid).unwrap();

        if let Some(entry) = log.read_from(sim.read_cursor) {
            // 3a. Feed the log entry to the state machine
            let envelope = entry.envelope.clone();
            let lsn = entry.lsn;
            sim.read_cursor = lsn + 1;

            // Intercept snapshot protocol commands before state machine
            match &envelope.command {
                Command::InitiateSnapshot { snapshot_id } => {
                    self.handle_initiate_snapshot(pid, *snapshot_id).await?;
                }
                Command::SnapshotMarker { snapshot_id, from } => {
                    self.handle_snapshot_marker(pid, *snapshot_id, *from).await?;
                }
                Command::OutboxProcessedAck { from_partition, up_to_seq } => {
                    self.handle_ack(pid, *from_partition, *up_to_seq).await?;
                }
                _ => {
                    // Normal command: apply to state machine, collect actions
                    let actions = sim.inner.apply_command(envelope.command).await?;
                    self.process_actions(pid, actions).await?;
                }
            }
        } else if let Some(timer) = sim.inner.next_timer() {
            // 3b. Fire a timer
            sim.inner.fire_timer(timer).await?;
        }

        // 4. Check invariants
        if self.config.check_invariants {
            self.check_invariants()?;
        }

        self.steps += 1;
        Ok(ClusterStepResult { partition: pid, step: self.steps })
    }

    /// Route actions from one partition to the appropriate logs.
    async fn process_actions(
        &mut self,
        source: PartitionId,
        actions: Vec<Action>,
    ) -> Result<(), SimulationError> {
        let sim = self.partitions.get_mut(&source).unwrap();

        for action in actions {
            match action {
                Action::NewOutboxMessage { seq_number, message } => {
                    let target = self.routing.route(&message);

                    if target == source {
                        // Same-partition: feed back directly (existing behavior)
                        sim.inner.enqueue_command(message_to_command(message));
                    } else if sim.outbound_gate_open {
                        // Cross-partition: route via shuffle to target log
                        sim.shuffle.route_outbox_message(
                            seq_number, message, target,
                            &mut self.logs, self.clock.now(),
                        );
                    } else {
                        // Gated: queue for later
                        sim.gated_actions.push(OutboundAction::OutboxMessage {
                            seq_number, message, target,
                        });
                    }
                }
                // ... handle other actions (timers, invoke, etc.) same as current ...
                _ => sim.inner.process_action(action),
            }
        }
        Ok(())
    }
}
```

---

## Part 2: Snapshot Protocol State Machine

### 2.1 New WAL Protocol Commands

These need to be added to the `Command` enum:

```rust
// In crates/wal-protocol/src/lib.rs
pub enum Command {
    // ... existing variants ...

    /// Coordinator tells a partition to initiate a cluster-wide snapshot.
    InitiateSnapshot {
        snapshot_id: ClusterSnapshotId,
    },

    /// Marker sent by partition `from` to indicate it has taken its local snapshot.
    /// Travels through Bifrost to preserve FIFO ordering with application messages.
    SnapshotMarker {
        snapshot_id: ClusterSnapshotId,
        from: PartitionId,
    },

    /// Acknowledgment that partition `from_partition`'s outbox messages up to
    /// `up_to_seq` have been processed by the receiver. Travels through Bifrost
    /// to preserve FIFO ordering with markers.
    OutboxProcessedAck {
        from_partition: PartitionId,
        up_to_seq: MessageIndex,
    },
}

/// Unique identifier for a cluster-wide snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClusterSnapshotId(pub u64);
```

### 2.2 Per-Partition Snapshot Protocol State

```rust
/// Tracks CL snapshot protocol state for one partition.
pub struct SnapshotProtocolState {
    pub snapshot_id: ClusterSnapshotId,
    /// Phase of the protocol for this partition.
    pub phase: SnapshotPhase,
    /// Which partitions we've sent markers to.
    pub markers_sent_to: HashSet<PartitionId>,
    /// Which partitions we've received markers from.
    pub markers_received_from: HashSet<PartitionId>,
    /// All other partitions we expect markers from.
    pub expected_markers: HashSet<PartitionId>,
    /// The LSN at which our local snapshot was taken (set in phase TakingSnapshot).
    pub snapshot_lsn: Option<Lsn>,
    /// The target LSN we're waiting for (committed_lsn after SelfProposer flush).
    pub target_lsn: Option<Lsn>,
}

pub enum SnapshotPhase {
    /// Received InitiateSnapshot or first marker. Flushing self-proposer,
    /// waiting for applied_lsn to reach target_lsn.
    DrainingSelfLoop,
    /// Self-loop drained. Taking RocksDB checkpoint.
    TakingSnapshot,
    /// Snapshot taken, markers sent. Waiting for markers from all others.
    WaitingForMarkers,
    /// All markers received. Snapshot complete for this partition.
    Complete,
}
```

### 2.3 Protocol Handlers

```rust
impl<S> ClusterSimulation<S> {
    /// Handle InitiateSnapshot command arriving at a partition.
    async fn handle_initiate_snapshot(
        &mut self,
        pid: PartitionId,
        snapshot_id: ClusterSnapshotId,
    ) -> Result<(), SimulationError> {
        let sim = self.partitions.get_mut(&pid).unwrap();

        // Ignore if already in a snapshot
        if sim.snapshot_state.is_some() {
            return Ok(());
        }

        let all_others: HashSet<_> = self.partitions.keys()
            .filter(|&&p| p != pid)
            .copied()
            .collect();

        sim.snapshot_state = Some(SnapshotProtocolState {
            snapshot_id,
            phase: SnapshotPhase::DrainingSelfLoop,
            markers_sent_to: HashSet::new(),
            markers_received_from: HashSet::new(),
            expected_markers: all_others,
            snapshot_lsn: None,
            target_lsn: None,
        });

        // Gate outbound messages
        sim.outbound_gate_open = false;

        // In the real system: flush SelfProposer, set target_lsn.
        // In simulation: the self-loop is instant (no SelfProposer queue),
        // so we can take the snapshot immediately.
        self.take_local_snapshot(pid).await?;

        Ok(())
    }

    /// Handle SnapshotMarker arriving at a partition.
    async fn handle_snapshot_marker(
        &mut self,
        pid: PartitionId,
        snapshot_id: ClusterSnapshotId,
        from: PartitionId,
    ) -> Result<(), SimulationError> {
        let sim = self.partitions.get_mut(&pid).unwrap();

        match &mut sim.snapshot_state {
            None => {
                // First marker for this snapshot: initiate local snapshot
                let all_others: HashSet<_> = self.partitions.keys()
                    .filter(|&&p| p != pid)
                    .copied()
                    .collect();

                sim.snapshot_state = Some(SnapshotProtocolState {
                    snapshot_id,
                    phase: SnapshotPhase::DrainingSelfLoop,
                    markers_sent_to: HashSet::new(),
                    markers_received_from: HashSet::from([from]),
                    expected_markers: all_others,
                    snapshot_lsn: None,
                    target_lsn: None,
                });

                sim.outbound_gate_open = false;
                self.take_local_snapshot(pid).await?;
            }
            Some(state) if state.snapshot_id == snapshot_id => {
                // Subsequent marker: record channel completion
                state.markers_received_from.insert(from);

                // Check if all markers received
                if state.markers_received_from == state.expected_markers {
                    state.phase = SnapshotPhase::Complete;
                    self.coordinator.report_complete(pid, snapshot_id, state.snapshot_lsn.unwrap());
                }
            }
            Some(_) => {
                // Different snapshot ID while one is in progress: ignore
            }
        }

        Ok(())
    }

    /// Take local snapshot and send markers to all other partitions.
    async fn take_local_snapshot(
        &mut self,
        pid: PartitionId,
    ) -> Result<(), SimulationError> {
        let sim = self.partitions.get_mut(&pid).unwrap();
        let state = sim.snapshot_state.as_mut().unwrap();

        // Record snapshot LSN (current applied position)
        let snapshot_lsn = sim.read_cursor.prev(); // Last applied LSN
        state.snapshot_lsn = Some(snapshot_lsn);
        state.phase = SnapshotPhase::TakingSnapshot;

        // Capture the full partition store state at this point.
        // This is the "RocksDB checkpoint" equivalent.
        let snapshot = sim.inner.capture_store_snapshot();
        self.coordinator.store_partition_snapshot(pid, state.snapshot_id, snapshot);

        // Send markers to all other partitions
        let others: Vec<_> = state.expected_markers.iter().copied().collect();
        for target in &others {
            sim.shuffle.send_marker(
                state.snapshot_id, *target,
                &mut self.logs, self.clock.now(),
            );
            state.markers_sent_to.insert(*target);
        }

        state.phase = SnapshotPhase::WaitingForMarkers;

        // Release the outbound gate: flush gated actions
        sim.outbound_gate_open = true;
        let gated = std::mem::take(&mut sim.gated_actions);
        for action in gated {
            match action {
                OutboundAction::OutboxMessage { seq_number, message, target } => {
                    sim.shuffle.route_outbox_message(
                        seq_number, message, target,
                        &mut self.logs, self.clock.now(),
                    );
                }
                OutboundAction::Ack(ack) => {
                    sim.shuffle.send_ack(
                        ack, ack.target_partition(),
                        &mut self.logs, self.clock.now(),
                    );
                }
            }
        }

        // Check if we've already received all markers (possible if we're
        // the last partition to snapshot)
        let state = sim.snapshot_state.as_ref().unwrap();
        if state.markers_received_from == state.expected_markers {
            let state = sim.snapshot_state.as_mut().unwrap();
            state.phase = SnapshotPhase::Complete;
            self.coordinator.report_complete(pid, state.snapshot_id, snapshot_lsn);
        }

        Ok(())
    }

    /// Handle OutboxProcessedAck: truncate sender's outbox.
    async fn handle_ack(
        &mut self,
        pid: PartitionId,
        from_partition: PartitionId,
        up_to_seq: MessageIndex,
    ) -> Result<(), SimulationError> {
        let sim = self.partitions.get_mut(&pid).unwrap();

        // Apply TruncateOutbox to the state machine.
        // The ack tells us that `from_partition`'s messages up to `up_to_seq`
        // have been processed, so we can truncate our outbox.
        let cmd = Command::TruncateOutbox(up_to_seq);
        let actions = sim.inner.apply_command(cmd).await?;
        self.process_actions(pid, actions).await?;

        Ok(())
    }
}
```

### 2.4 Ack Generation on Message Processing

When partition Pj processes a cross-partition message from Pi, it must send an
ack back to Pi's log. This happens in the dedup check path:

```rust
impl<S> ClusterSimulation<S> {
    /// Apply a normal (non-snapshot-protocol) command from another partition.
    /// If it has dedup info, generate an ack back to the sender.
    async fn apply_cross_partition_command(
        &mut self,
        receiver: PartitionId,
        envelope: Envelope,
    ) -> Result<(), SimulationError> {
        let sim = self.partitions.get_mut(&receiver).unwrap();

        if let Some(dedup) = &envelope.header.dedup_info() {
            if let ProducerId::Partition(sender_pid) = &dedup.producer_id {
                let sender = *sender_pid;
                let seq = dedup.sequence_number.as_message_index();

                // Apply command to state machine (handles dedup internally)
                let actions = sim.inner.apply_command(envelope.command).await?;
                self.process_actions(receiver, actions).await?;

                // Generate ack — even for duplicates (critical for restore correctness)
                let ack = OutboxProcessedAck {
                    from_partition: sender,
                    up_to_seq: seq,
                };

                let sim = self.partitions.get_mut(&receiver).unwrap();
                if sim.outbound_gate_open {
                    sim.shuffle.send_ack(
                        ack, sender,
                        &mut self.logs, self.clock.now(),
                    );
                } else {
                    sim.gated_actions.push(OutboundAction::Ack(ack));
                }
            }
        } else {
            let actions = sim.inner.apply_command(envelope.command).await?;
            self.process_actions(receiver, actions).await?;
        }

        Ok(())
    }
}
```

### 2.5 Snapshot Coordinator

```rust
pub struct SnapshotCoordinator {
    /// Active snapshot being collected (at most one at a time).
    pub active: Option<ActiveSnapshot>,
    /// Completed snapshots.
    pub completed: Vec<ClusterSnapshot>,
    /// Next snapshot ID to assign.
    next_id: u64,
}

pub struct ActiveSnapshot {
    pub id: ClusterSnapshotId,
    pub initiated_at: MillisSinceEpoch,
    /// Partitions that have reported completion.
    pub completed_partitions: HashMap<PartitionId, PartitionSnapshotInfo>,
    /// All partitions participating.
    pub all_partitions: HashSet<PartitionId>,
}

pub struct PartitionSnapshotInfo {
    pub snapshot_lsn: Lsn,
    /// Captured partition store state (outbox, dedup, invocations, etc.)
    pub store_snapshot: CapturedStoreState,
}

pub struct ClusterSnapshot {
    pub id: ClusterSnapshotId,
    pub partitions: HashMap<PartitionId, PartitionSnapshotInfo>,
}

impl SnapshotCoordinator {
    /// Initiate a new cluster snapshot. Injects InitiateSnapshot into one
    /// partition's log (the "initiator").
    pub fn initiate(
        &mut self,
        initiator: PartitionId,
        all_partitions: &HashSet<PartitionId>,
        logs: &mut HashMap<PartitionId, SimulatedLog>,
        now: MillisSinceEpoch,
    ) -> ClusterSnapshotId { ... }

    /// Record that a partition has completed its local snapshot.
    pub fn report_complete(
        &mut self,
        pid: PartitionId,
        snapshot_id: ClusterSnapshotId,
        snapshot_lsn: Lsn,
    ) { ... }

    /// Check if the active snapshot is complete (all partitions reported).
    pub fn is_complete(&self) -> bool { ... }

    /// Finalize: move active snapshot to completed list.
    pub fn finalize(&mut self) -> Option<ClusterSnapshot> { ... }
}
```

---

## Part 3: Snapshot-Specific Invariant Checkers

These are the key properties the simulation should validate.

### 3.1 Message Conservation (The Core CL Invariant)

> For every message M sent from Pi to Pj, at snapshot time M exists in
> Pi's outbox OR Pj's dedup (or both).

```rust
/// Check: every cross-partition message is captured somewhere in the snapshot.
fn check_message_conservation(
    snapshot: &ClusterSnapshot,
    logs: &HashMap<PartitionId, SimulatedLog>,
) -> Result<(), InvariantViolation> {
    for (sender_pid, sender_info) in &snapshot.partitions {
        // Get all outbox messages in sender's snapshot
        let outbox_seqs: HashSet<MessageIndex> =
            sender_info.store_snapshot.outbox_entries().map(|e| e.seq).collect();

        // For each message ever sent by this partition to another:
        for (receiver_pid, receiver_info) in &snapshot.partitions {
            if sender_pid == receiver_pid { continue; }

            let receiver_dedup_watermark = receiver_info.store_snapshot
                .dedup_seq_for(&ProducerId::Partition(*sender_pid));

            // Every message seq from sender to receiver must be:
            // - In sender's outbox (seq in outbox_seqs), OR
            // - In receiver's dedup (seq <= receiver_dedup_watermark)
            for seq in sender_info.store_snapshot.all_sent_seqs_to(*receiver_pid) {
                let in_outbox = outbox_seqs.contains(&seq);
                let in_dedup = receiver_dedup_watermark
                    .map_or(false, |w| seq <= w);

                if !in_outbox && !in_dedup {
                    return Err(InvariantViolation::Custom(format!(
                        "Message seq={} from {:?} to {:?} lost: \
                         not in sender outbox, not in receiver dedup",
                        seq, sender_pid, receiver_pid,
                    )));
                }
            }
        }
    }
    Ok(())
}
```

**Implementation note**: To check this, `CapturedStoreState` must track
which messages have ever been sent cross-partition. The simplest approach:
maintain a per-partition `sent_messages: Vec<(PartitionId, MessageIndex)>`
log in the simulation (not in the real store — this is test instrumentation).

### 3.2 Consistent Cut (No Orphaned Receives)

> If a message M is in receiver Pj's dedup (i.e., Pj "received" M), then
> the corresponding send must also be captured: either M is still in Pi's
> outbox, or the send happened before Pi's snapshot point.

This is the contrapositive of message conservation and is checked by the
same invariant above. The simulation should also verify:

```rust
/// Check: no dedup entry references a message that was never sent.
fn check_no_orphaned_receives(
    snapshot: &ClusterSnapshot,
    message_log: &MessageLog,  // test instrumentation tracking all sends
) -> Result<(), InvariantViolation> {
    for (pid, info) in &snapshot.partitions {
        for (producer_id, dedup_seq) in info.store_snapshot.dedup_entries() {
            if let ProducerId::Partition(sender) = producer_id {
                // Verify sender actually sent messages up to this sequence
                if !message_log.was_sent(*sender, *pid, dedup_seq) {
                    return Err(InvariantViolation::Custom(format!(
                        "Partition {:?} has dedup entry for {:?} seq={}, \
                         but no such message was ever sent",
                        pid, sender, dedup_seq,
                    )));
                }
            }
        }
    }
    Ok(())
}
```

### 3.3 Marker Ordering (FIFO Guarantee)

> For each channel Pi→Pj, Pi's marker must appear in Log-Pj AFTER all
> application messages that Pi sent before its snapshot.

```rust
/// Check: markers appear after all pre-snapshot messages in each target log.
fn check_marker_ordering(
    snapshot: &ClusterSnapshot,
    logs: &HashMap<PartitionId, SimulatedLog>,
    marker_lsns: &HashMap<(PartitionId, PartitionId), Lsn>,  // (from, to) → LSN of marker
    message_send_log: &MessageSendLog,
) -> Result<(), InvariantViolation> {
    for (&(from, to), &marker_lsn) in marker_lsns {
        let sender_snapshot_lsn = snapshot.partitions[&from].snapshot_lsn;

        // All messages from `from` that were sent before `from`'s snapshot
        // must appear in Log-to at LSN < marker_lsn
        for msg in message_send_log.messages_from_before_snapshot(from, to, sender_snapshot_lsn) {
            if msg.target_log_lsn >= marker_lsn {
                return Err(InvariantViolation::Custom(format!(
                    "Message seq={} from {:?} appears at LSN {} in {:?}'s log, \
                     but marker from {:?} is at LSN {} (message should be before marker)",
                    msg.seq, from, msg.target_log_lsn, to, from, marker_lsn,
                )));
            }
        }
    }
    Ok(())
}
```

### 3.4 Ack-Before-Truncation (No Premature Outbox Truncation)

> A partition must not truncate an outbox entry until it receives an
> OutboxProcessedAck for that entry through its own Bifrost log.

```rust
/// Track all truncation events and verify each was preceded by an ack.
fn check_ack_before_truncation(
    truncation_log: &[(PartitionId, MessageIndex, Lsn)],  // (pid, seq, lsn_of_truncation)
    ack_log: &[(PartitionId, MessageIndex, Lsn)],          // (pid, seq, lsn_of_ack)
) -> Result<(), InvariantViolation> {
    for &(pid, seq, trunc_lsn) in truncation_log {
        let ack_exists = ack_log.iter().any(|&(p, s, ack_lsn)| {
            p == pid && s >= seq && ack_lsn <= trunc_lsn
        });
        if !ack_exists {
            return Err(InvariantViolation::Custom(format!(
                "Partition {:?} truncated outbox seq={} at LSN {} \
                 without receiving ack first",
                pid, seq, trunc_lsn,
            )));
        }
    }
    Ok(())
}
```

### 3.5 Gate Invariant (No Messages Between Snapshot and Marker)

> After taking a local snapshot and before sending markers, a partition
> must not send any application messages on outgoing channels.

```rust
/// Check: no application messages were sent between snapshot and marker send.
fn check_gate_invariant(
    logs: &HashMap<PartitionId, SimulatedLog>,
    snapshot_events: &HashMap<PartitionId, SnapshotTiming>,
) -> Result<(), InvariantViolation> {
    for (pid, timing) in snapshot_events {
        let snapshot_time = timing.snapshot_taken_at;
        let markers_sent_at = timing.markers_sent_at;  // clock time when markers were sent

        // Check each target log for messages from `pid` between snapshot and marker
        for (target, log) in logs {
            if target == pid { continue; }

            for entry in log.entries_from_sender(*pid) {
                if entry.appended_at > snapshot_time && entry.appended_at < markers_sent_at {
                    if !entry.is_control_message() {
                        return Err(InvariantViolation::Custom(format!(
                            "Partition {:?} sent application message to {:?} \
                             between snapshot and marker send",
                            pid, target,
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}
```

### 3.6 Restore Correctness (End-to-End)

This is the most important test: restore from snapshot and verify the system
reaches a correct state.

```rust
/// Restore from a cluster snapshot, replay, and verify:
/// 1. All messages are eventually delivered exactly once
/// 2. No invariant violations during replay
/// 3. System reaches quiescence with correct state
async fn check_restore_correctness<S>(
    snapshot: &ClusterSnapshot,
    original_outcome: &ClusterOutcome,
) -> Result<(), InvariantViolation> {
    // 1. Create fresh ClusterSimulation with same config but fresh logs
    let mut restored = ClusterSimulation::restore_from(snapshot);

    // 2. Each partition starts from its snapshot state
    //    - Outbox has un-acked messages
    //    - Dedup has processed message records
    //    - Logs are empty (fresh Bifrost)

    // 3. Each partition's shuffle rescans outbox and resends
    for (pid, sim) in &mut restored.partitions {
        sim.shuffle.rescan_outbox(&sim.inner.storage());
    }

    // 4. Run until quiescence (no pending work across all partitions)
    let outcome = restored.run_until_quiescent().await?;

    // 5. Verify: every message that was in an outbox at snapshot time
    //    has now been processed (dedup'd or applied) by its target
    for (pid, info) in &snapshot.partitions {
        for outbox_entry in info.store_snapshot.outbox_entries() {
            let target = routing.route(&outbox_entry.message);
            let target_sim = restored.partitions.get(&target).unwrap();

            // Message must be in target's dedup after restore
            let dedup_seq = target_sim.inner.storage()
                .get_dedup_sequence_number(&ProducerId::Partition(*pid))
                .await?;

            assert!(
                dedup_seq.map_or(false, |d| d >= outbox_entry.seq),
                "After restore, message seq={} from {:?} not in {:?}'s dedup",
                outbox_entry.seq, pid, target,
            );
        }
    }

    // 6. Verify: all existing VO exclusivity and other invariants still hold
    for (pid, sim) in &restored.partitions {
        sim.inner.check_invariants()?;
    }

    Ok(())
}
```

---

## Part 4: CapturedStoreState (Snapshot Representation)

The simulation needs to capture the full partition store state at snapshot time
for later inspection by invariant checkers and for restore.

```rust
/// Captured state of a partition's store at snapshot time.
/// This is the simulation equivalent of a RocksDB checkpoint.
#[derive(Clone, Debug)]
pub struct CapturedStoreState {
    /// Outbox entries: (seq_number, target_partition, message).
    pub outbox: Vec<OutboxEntry>,
    /// Dedup table: producer_id → highest processed sequence number.
    pub dedup: HashMap<ProducerId, DedupSequenceNumber>,
    /// Invocation states (for verifying restore correctness).
    pub invocations: HashMap<InvocationId, InvocationStatus>,
    /// The applied LSN at snapshot time.
    pub applied_lsn: Lsn,
}

#[derive(Clone, Debug)]
pub struct OutboxEntry {
    pub seq: MessageIndex,
    pub message: OutboxMessage,
    pub target: PartitionId,
}

impl CapturedStoreState {
    /// Capture current state from a PartitionSimulation's storage.
    pub async fn capture_from<S: Storage>(storage: &mut S) -> Self { ... }

    /// Restore into a fresh storage backend.
    pub async fn restore_into<S: Storage>(&self, storage: &mut S) { ... }
}
```

---

## Part 5: Test Scenarios

### 5.1 Basic Two-Partition Snapshot

The happy path from the walkthrough document.

```rust
#[test]
async fn test_basic_two_partition_snapshot() {
    let mut cluster = ClusterSimulation::new(ClusterSimConfig {
        seed: 42,
        num_partitions: 2,
        max_steps: 500,
        check_invariants: true,
        scheduling: SchedulingPolicy::RoundRobin,
    });

    // Enqueue some cross-partition invocations
    let inv = cluster.random_cross_partition_invocation();
    cluster.enqueue_invocation(inv);

    // Run until invocations are processed
    cluster.run_steps(50).await.unwrap();

    // Initiate snapshot
    let snap_id = cluster.coordinator.initiate(
        PartitionId(0),
        &cluster.partition_ids(),
        &mut cluster.logs,
        cluster.clock.now(),
    );

    // Run until snapshot completes
    cluster.run_until_snapshot_complete(snap_id).await.unwrap();

    // Verify snapshot invariants
    let snapshot = cluster.coordinator.completed.last().unwrap();
    check_message_conservation(snapshot, &cluster.logs).unwrap();
}
```

### 5.2 In-Flight Messages at Snapshot Time

Messages in various stages: outbox-only, in-log, dedup'd.

```rust
#[test]
async fn test_inflight_messages_at_snapshot() {
    let mut cluster = ClusterSimulation::new(config(2, 1000));

    // Create a burst of cross-partition traffic
    for _ in 0..20 {
        let inv = cluster.random_cross_partition_invocation();
        cluster.enqueue_invocation(inv);
    }

    // Process just enough to get messages in various stages
    cluster.run_steps(30).await.unwrap();

    // Snapshot while messages are in flight
    let snap_id = cluster.initiate_snapshot(PartitionId(0));
    cluster.run_until_snapshot_complete(snap_id).await.unwrap();

    let snapshot = cluster.coordinator.completed.last().unwrap();
    check_message_conservation(snapshot, &cluster.logs).unwrap();

    // Restore and verify
    check_restore_correctness(snapshot, &cluster.outcome()).await.unwrap();
}
```

### 5.3 Concurrent Activity During Snapshot

New invocations arrive while snapshot is in progress.

```rust
#[test]
async fn test_concurrent_activity_during_snapshot() {
    let mut cluster = ClusterSimulation::new(config(3, 2000));

    // Seed initial work
    for _ in 0..10 {
        cluster.enqueue_random_invocation();
    }
    cluster.run_steps(20).await.unwrap();

    // Initiate snapshot
    let snap_id = cluster.initiate_snapshot(PartitionId(0));

    // Inject more work while snapshot is collecting
    for _ in 0..10 {
        cluster.enqueue_random_invocation();
        cluster.run_steps(5).await.unwrap();
    }

    cluster.run_until_snapshot_complete(snap_id).await.unwrap();

    let snapshot = cluster.coordinator.completed.last().unwrap();
    check_message_conservation(snapshot, &cluster.logs).unwrap();
    check_gate_invariant(&cluster.logs, &cluster.snapshot_timing()).unwrap();
}
```

### 5.4 N-Partition with Full Mesh Traffic

Three or more partitions with messages flowing in all directions.

```rust
#[test]
async fn test_n_partition_full_mesh() {
    let mut cluster = ClusterSimulation::new(config(4, 5000));

    // Generate cross-partition invocations targeting all partition combinations
    for _ in 0..50 {
        let inv = cluster.random_cross_partition_invocation();
        cluster.enqueue_invocation(inv);
    }

    // Run with interleaved processing
    cluster.run_steps(200).await.unwrap();

    let snap_id = cluster.initiate_snapshot(PartitionId(0));
    cluster.run_until_snapshot_complete(snap_id).await.unwrap();

    let snapshot = cluster.coordinator.completed.last().unwrap();
    check_message_conservation(snapshot, &cluster.logs).unwrap();
    check_restore_correctness(snapshot, &cluster.outcome()).await.unwrap();
}
```

### 5.5 Ack Timing: Before vs. After Marker

Force the ack to arrive before/after the marker in the sender's log.

```rust
#[test]
async fn test_ack_arrives_before_marker() {
    let mut cluster = ClusterSimulation::new(ClusterSimConfig {
        scheduling: SchedulingPolicy::Manual,  // Full control over ordering
        ..config(2, 500)
    });

    // P0 sends message M to P1
    let inv = cluster.invocation_from_to(PartitionId(0), PartitionId(1));
    cluster.enqueue_invocation(inv);

    // Process P0: M goes to outbox, shuffle appends to Log-P1
    cluster.step_partition(PartitionId(0)).await.unwrap();

    // Process P1: receives M, generates ack to Log-P0
    cluster.step_partition(PartitionId(1)).await.unwrap();

    // Now initiate snapshot on P0 — ack is already in Log-P0
    let snap_id = cluster.initiate_snapshot(PartitionId(0));

    // P0 processes ack (truncates outbox), then processes marker from P1
    cluster.run_until_snapshot_complete(snap_id).await.unwrap();

    // M should be in P1's dedup (P0 truncated before snapshot)
    let snapshot = cluster.coordinator.completed.last().unwrap();
    check_message_conservation(snapshot, &cluster.logs).unwrap();
}

#[test]
async fn test_ack_arrives_after_marker() {
    let mut cluster = ClusterSimulation::new(ClusterSimConfig {
        scheduling: SchedulingPolicy::Manual,
        ..config(2, 500)
    });

    // P0 sends message M to P1
    let inv = cluster.invocation_from_to(PartitionId(0), PartitionId(1));
    cluster.enqueue_invocation(inv);

    // Process P0: M goes to outbox, shuffle appends to Log-P1
    cluster.step_partition(PartitionId(0)).await.unwrap();

    // Initiate snapshot BEFORE P1 processes M
    let snap_id = cluster.initiate_snapshot(PartitionId(0));

    // P0 snapshots (M in outbox), sends marker to P1
    cluster.step_partition(PartitionId(0)).await.unwrap();

    // P1 processes M (generates ack), then sees marker, takes snapshot
    cluster.step_partition(PartitionId(1)).await.unwrap();
    cluster.step_partition(PartitionId(1)).await.unwrap();

    // P0 receives ack AFTER its snapshot → M in P0's snapshot outbox
    cluster.run_until_snapshot_complete(snap_id).await.unwrap();

    let snapshot = cluster.coordinator.completed.last().unwrap();
    check_message_conservation(snapshot, &cluster.logs).unwrap();
}
```

### 5.6 Stress Test: Randomized Multi-Partition

The most important test — randomized scheduling finds edge cases.

```rust
#[test]
async fn test_snapshot_stress() {
    for seed in 0..1000 {
        let mut cluster = ClusterSimulation::new(ClusterSimConfig {
            seed,
            num_partitions: 3,
            max_steps: 3000,
            check_invariants: true,
            scheduling: SchedulingPolicy::Random,
        });

        // Generate random cross-partition workload
        for _ in 0..30 {
            cluster.enqueue_random_invocation();
        }

        // Run some steps, then snapshot
        let steps_before = cluster.rng.gen_range(10..100);
        cluster.run_steps(steps_before).await.unwrap();

        let initiator = PartitionId(cluster.rng.gen_range(0..3));
        let snap_id = cluster.initiate_snapshot(initiator);

        // Keep running (more invocations may arrive during snapshot)
        if cluster.rng.gen_bool(0.5) {
            for _ in 0..5 {
                cluster.enqueue_random_invocation();
            }
        }

        cluster.run_until_snapshot_complete(snap_id).await.unwrap();

        let snapshot = cluster.coordinator.completed.last().unwrap();
        check_message_conservation(snapshot, &cluster.logs).unwrap();
        check_marker_ordering(snapshot, &cluster.logs, &cluster.marker_lsns()).unwrap();
        check_gate_invariant(&cluster.logs, &cluster.snapshot_timing()).unwrap();

        // Restore test (more expensive, run for subset of seeds)
        if seed % 10 == 0 {
            check_restore_correctness(snapshot, &cluster.outcome()).await.unwrap();
        }
    }
}
```

### 5.7 Self-Loop Drain Under Load

Verify the deferred snapshot point works when many self-proposed commands
are in flight.

```rust
#[test]
async fn test_self_loop_drain_under_load() {
    let mut cluster = ClusterSimulation::new(config(2, 2000));

    // Generate invocations that produce many self-proposed commands
    // (e.g., timer registrations, journal writes)
    for _ in 0..20 {
        cluster.enqueue_random_invocation();
    }

    // Run just enough to create lots of in-flight self-proposed commands
    cluster.run_steps(15).await.unwrap();

    // Snapshot — must wait for self-loop drain
    let snap_id = cluster.initiate_snapshot(PartitionId(0));
    cluster.run_until_snapshot_complete(snap_id).await.unwrap();

    // Verify the snapshot LSN reflects a fully-drained self-loop:
    // all commands generated before the snapshot decision are included
    let snapshot = cluster.coordinator.completed.last().unwrap();
    let p0_info = &snapshot.partitions[&PartitionId(0)];
    // In the simulation, the self-loop is synchronous, so this
    // is trivially true. The real test value is in the actual
    // PP integration where SelfProposer has a queue.
}
```

---

## Part 6: Modeling Choices & Simplifications

### What the simulation models faithfully

1. **FIFO ordering per log** — SimulatedLog assigns monotonic LSNs
2. **Per-sender channel accounting** — Dedup keyed by ProducerId::Partition
3. **Outbox/dedup duality** — Messages in exactly one place at any time
4. **Acks through Bifrost** — Acks appended to sender's log, not out-of-band
5. **Marker propagation rule** — Gate enforces no messages before markers
6. **One snapshot at a time** — Subsequent InitiateSnapshot ignored

### What the simulation simplifies

1. **Self-loop channel** — In the real system, there's a SelfProposer queue and
   a gap between `committed_lsn` and `applied_lsn`. In the simulation, command
   application is synchronous, so the self-loop is effectively instant. This is
   acceptable because:
   - The self-loop drain logic is straightforward (wait for catch-up)
   - The interesting bugs are in cross-partition message accounting
   - A separate unit test can validate self-loop drain timing

2. **RocksDB checkpoints** — Replaced with `CapturedStoreState` clones. The
   simulation doesn't need real disk I/O.

3. **Leader epochs** — The simulation assumes stable leadership. Epoch changes
   during snapshots are a separate concern (snapshot should be aborted on
   leadership change).

4. **Bifrost trimming** — Logs are never trimmed in the simulation. Trimming
   interacts with snapshot retention but doesn't affect protocol correctness.

5. **Object store upload** — The coordinator stores snapshots in memory, not
   in an object store.

### What to add later

1. **Leadership changes during snapshot** — Inject epoch bumps mid-snapshot
   and verify the protocol aborts cleanly.
2. **Partition restarts** — Kill and restart a partition during snapshot
   collection.
3. **Multiple sequential snapshots** — Take snapshot, continue, take another.
4. **Snapshot with Bifrost trim** — Trim logs below snapshot LSN, then restore.

---

## Part 7: Integration with Existing DST Framework

### Required changes to `PartitionSimulation`

The existing `PartitionSimulation` needs these additions:

```rust
impl<S> PartitionSimulation<S> {
    /// Apply a single command (envelope) and return collected actions.
    /// This is the primitive the ClusterSimulation calls.
    pub async fn apply_command(&mut self, cmd: Command) -> Result<Vec<Action>, SimulationError>;

    /// Process a single action (invoke, timer, etc.) — the non-outbox path.
    pub fn process_action(&mut self, action: Action);

    /// Capture the current store state for snapshot.
    pub async fn capture_store_snapshot(&mut self) -> CapturedStoreState;

    /// Restore from a captured store state.
    pub async fn restore_from_snapshot(&mut self, state: CapturedStoreState);
}
```

These factor out the internals that are currently wired together in `step()`.

### New module structure

```
crates/simulation/src/
├── lib.rs                    # Re-exports
├── clock.rs                  # Existing
├── trace.rs                  # Existing
├── partition.rs              # Existing (single-partition sim)
├── cluster.rs                # NEW: ClusterSimulation
├── cluster/
│   ├── mod.rs                # ClusterSimulation, step loop
│   ├── log.rs                # SimulatedLog
│   ├── shuffle.rs            # SimulatedShuffle
│   ├── routing.rs            # PartitionKeyRouter
│   ├── coordinator.rs        # SnapshotCoordinator
│   ├── snapshot_state.rs     # SnapshotProtocolState, SnapshotPhase
│   ├── captured_state.rs     # CapturedStoreState
│   └── invariants.rs         # Snapshot-specific invariant checkers
└── bin/
    ├── stress.rs             # Existing (single-partition stress)
    └── cluster_stress.rs     # NEW: Multi-partition snapshot stress
```

### Dependency on WAL protocol changes

The simulation needs the new `Command` variants (`InitiateSnapshot`,
`SnapshotMarker`, `OutboxProcessedAck`). These should be added to the
real `Command` enum — the simulation uses the real types, not mocks.

If that's not ready yet, the simulation can use a wrapper enum:

```rust
/// Simulation-only command wrapper until WAL protocol is updated.
pub enum SimCommand {
    Real(Command),
    InitiateSnapshot { snapshot_id: ClusterSnapshotId },
    SnapshotMarker { snapshot_id: ClusterSnapshotId, from: PartitionId },
    OutboxProcessedAck { from_partition: PartitionId, up_to_seq: MessageIndex },
}
```

This lets the snapshot protocol logic be developed and tested in the
simulation before the real WAL protocol changes land.
