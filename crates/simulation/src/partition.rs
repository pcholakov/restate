// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Per-partition deterministic simulation.
//!
//! This module provides a closed-loop simulation of a single partition processor,
//! where actions emitted by the state machine (timer registrations, outbox messages)
//! are fed back as commands.
//!
//! In multi-partition mode, cross-partition messages are collected as outbound
//! messages instead of being dropped. A [`ClusterSimulation`](super::cluster::ClusterSimulation)
//! routes them between partitions.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use bytestring::ByteString;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use restate_invoker_api::{Effect, EffectKind, InvokeInputJournal};
use restate_partition_store::PartitionStore;
use restate_partition_store::snapshots::LocalPartitionSnapshot;
use restate_service_protocol_v4::entry_codec::ServiceProtocolV4Codec;
use restate_storage_api::fsm_table::WriteFsmTable;
use restate_storage_api::invocation_status_table::{InvocationStatus, ReadInvocationStatusTable};
use restate_storage_api::journal_table_v2::ReadJournalTable;
use restate_storage_api::outbox_table::OutboxMessage;
use restate_storage_api::service_status_table::{
    ReadVirtualObjectStatusTable, VirtualObjectStatus,
};
use restate_storage_api::state_table::ReadStateTable;
use restate_storage_api::timer_table::TimerKey;
use restate_storage_api::{Storage, Transaction};
use restate_types::deployment::PinnedDeployment;
use restate_types::errors::InvocationError;
use restate_types::identifiers::{
    ClusterSnapshotId, DeploymentId, InvocationId, InvocationUuid, PartitionId, PartitionKey,
    ServiceId, SnapshotId, WithPartitionKey, partitioner::HashPartitioner,
};
use restate_types::invocation::{InvocationTarget, ServiceInvocation, Source};
use restate_types::journal_v2::Entry;
use restate_types::journal_v2::command::{
    Command as JournalCommand, GetEagerStateCommand, OutputCommand, OutputResult, SetStateCommand,
};
use restate_types::journal_v2::notification::GetStateResult;
use restate_types::logs::{Lsn, SequenceNumber};
use restate_types::partition_table::{FindPartition, PartitionTable};
use restate_types::service_protocol::ServiceProtocolVersion;
use restate_types::storage::{StoredRawEntry, StoredRawEntryHeader};
use restate_types::time::MillisSinceEpoch;
use restate_types::timer::Timer;
use restate_vqueues::VQueuesMetaMut;
use restate_wal_protocol::Command;
use restate_wal_protocol::timer::TimerKeyValue;
use restate_worker::state_machine::{Action, ActionCollector, StateMachine};

use crate::clock::SimulationClock;
use crate::trace::SimulationTrace;

/// Small key space for virtual object invocations to force key collisions.
/// This is critical for testing VO exclusivity invariants.
pub const VO_TEST_KEYS: &[&str] = &["key-a", "key-b", "key-c"];

/// Default service name for test invocations.
pub const VO_TEST_SERVICE: &str = "TestService";

/// Default handler name for test invocations.
pub const VO_TEST_HANDLER: &str = "handler";

/// Configuration for a partition simulation.
#[derive(Debug, Clone)]
pub struct PartitionSimulationConfig {
    /// Random seed for deterministic execution.
    pub seed: u64,
    /// Maximum number of simulation steps before termination.
    pub max_steps: usize,
    /// Partition ID for this partition.
    pub partition_id: PartitionId,
    /// Partition key range for this partition.
    pub partition_key_range: RangeInclusive<PartitionKey>,
    /// Whether to check invariants after each step.
    pub check_invariants: bool,
}

impl Default for PartitionSimulationConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            max_steps: 10_000,
            partition_id: PartitionId::MIN,
            partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
            check_invariants: true,
        }
    }
}

/// A message destined for another partition, collected during action processing.
/// The cluster simulation routes these to the appropriate target.
#[derive(Debug)]
pub struct OutboundMessage {
    pub target_partition_id: PartitionId,
    pub command: Command,
}

/// Behavior configuration for the invoker simulator.
#[derive(Debug, Default)]
pub enum InvokerBehavior {
    /// Immediately complete invocations with success.
    #[default]
    ImmediateSuccess,
    /// Immediately fail invocations with an error.
    ImmediateFail { error_code: u16, message: String },
    /// Generate a random sequence of journal entries before completing.
    RandomJournal {
        /// Minimum number of journal entries to generate.
        min_entries: usize,
        /// Maximum number of journal entries to generate.
        max_entries: usize,
    },
    /// Probabilistic behavior: randomly choose success, failure, or timeout.
    /// Timeout probability is the remainder (1.0 - success_rate - failure_rate).
    /// Timeouts generate no effects, simulating an invoker that never responds.
    Probabilistic {
        /// Probability of success (0.0 - 1.0).
        success_rate: f64,
        /// Probability of failure (0.0 - 1.0).
        failure_rate: f64,
    },
    /// Custom behavior provided by the test.
    Custom(Box<dyn InvokerSimulator>),
}

/// Trait for simulating invoker behavior.
///
/// Implementations generate the sequence of `InvokerEffect` commands that would
/// be produced by the invoker when executing an invocation.
pub trait InvokerSimulator: std::fmt::Debug + Send + Sync {
    /// Called when an invocation should be started.
    /// Returns the sequence of commands to apply for this invocation.
    ///
    /// `eager_state` contains all user state for the target service ID, loaded
    /// from the partition store before the invocation starts (mirroring how the
    /// real invoker sends eager state in the StartMessage).
    fn on_invoke(
        &mut self,
        invocation_id: InvocationId,
        invocation_target: &InvocationTarget,
        journal: &InvokeInputJournal,
        eager_state: &[(Bytes, Bytes)],
        rng: &mut StdRng,
        clock: &SimulationClock,
    ) -> Vec<Command>;
}

/// Simple invoker simulator that immediately completes invocations.
#[derive(Debug, Default)]
pub struct ImmediateSuccessInvoker;

impl ImmediateSuccessInvoker {
    fn generate_success(invocation_id: InvocationId) -> Vec<Command> {
        vec![
            // Pin deployment
            Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::PinnedDeployment(PinnedDeployment {
                    deployment_id: DeploymentId::default(),
                    service_protocol_version: ServiceProtocolVersion::V5,
                }),
            })),
            // Output entry
            Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::JournalEntryV2 {
                    entry: StoredRawEntry::new(
                        StoredRawEntryHeader::new(MillisSinceEpoch::UNIX_EPOCH),
                        Entry::Command(JournalCommand::Output(OutputCommand {
                            result: OutputResult::Success(Bytes::new()),
                            name: ByteString::new(),
                        }))
                        .encode::<ServiceProtocolV4Codec>(),
                    ),
                    command_index_to_ack: None,
                },
            })),
            // End
            Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::End,
            })),
        ]
    }

    fn generate_failure(invocation_id: InvocationId, error: InvocationError) -> Vec<Command> {
        vec![
            // Pin deployment first
            Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::PinnedDeployment(PinnedDeployment {
                    deployment_id: DeploymentId::default(),
                    service_protocol_version: ServiceProtocolVersion::V5,
                }),
            })),
            // Failed
            Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::Failed(error),
            })),
        ]
    }
}

impl InvokerSimulator for ImmediateSuccessInvoker {
    fn on_invoke(
        &mut self,
        invocation_id: InvocationId,
        _invocation_target: &InvocationTarget,
        _journal: &InvokeInputJournal,
        _eager_state: &[(Bytes, Bytes)],
        _rng: &mut StdRng,
        _clock: &SimulationClock,
    ) -> Vec<Command> {
        Self::generate_success(invocation_id)
    }
}

/// Invoker simulator that immediately fails invocations.
#[derive(Debug)]
pub struct ImmediateFailInvoker {
    error_code: u16,
    message: String,
}

impl ImmediateFailInvoker {
    pub fn new(error_code: u16, message: String) -> Self {
        Self {
            error_code,
            message,
        }
    }
}

impl InvokerSimulator for ImmediateFailInvoker {
    fn on_invoke(
        &mut self,
        invocation_id: InvocationId,
        _invocation_target: &InvocationTarget,
        _journal: &InvokeInputJournal,
        _eager_state: &[(Bytes, Bytes)],
        _rng: &mut StdRng,
        _clock: &SimulationClock,
    ) -> Vec<Command> {
        let error = InvocationError::new(self.error_code, self.message.clone());
        ImmediateSuccessInvoker::generate_failure(invocation_id, error)
    }
}

/// Invoker simulator that randomly chooses between success, failure, and timeout.
#[derive(Debug)]
pub struct ProbabilisticInvoker {
    success_rate: f64,
    failure_rate: f64,
}

impl ProbabilisticInvoker {
    pub fn new(success_rate: f64, failure_rate: f64) -> Self {
        debug_assert!(
            success_rate + failure_rate <= 1.0,
            "success_rate + failure_rate must be <= 1.0"
        );
        Self {
            success_rate,
            failure_rate,
        }
    }
}

impl InvokerSimulator for ProbabilisticInvoker {
    fn on_invoke(
        &mut self,
        invocation_id: InvocationId,
        _invocation_target: &InvocationTarget,
        _journal: &InvokeInputJournal,
        _eager_state: &[(Bytes, Bytes)],
        rng: &mut StdRng,
        _clock: &SimulationClock,
    ) -> Vec<Command> {
        let roll: f64 = rng.random();

        if roll < self.success_rate {
            // Success
            ImmediateSuccessInvoker::generate_success(invocation_id)
        } else if roll < self.success_rate + self.failure_rate {
            // Failure
            let error = InvocationError::new(500u16, "Simulated invoker failure");
            ImmediateSuccessInvoker::generate_failure(invocation_id, error)
        } else {
            // Timeout - return no commands, simulating invoker that never responds.
            // The invocation will remain active until explicitly aborted.
            vec![]
        }
    }
}

/// Shared state for [`SetServiceInvoker`] instances across partitions.
///
/// Records which invocation produced which element for post-restore verification.
pub struct SetServiceState {
    elements: Arc<parking_lot::Mutex<HashMap<InvocationId, u64>>>,
}

impl Default for SetServiceState {
    fn default() -> Self {
        Self {
            elements: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }
}

impl SetServiceState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns (InvocationId → element) for all invocations processed so far.
    pub fn elements(&self) -> HashMap<InvocationId, u64> {
        self.elements.lock().clone()
    }

    /// Creates a new invoker sharing this state.
    pub fn invoker(&self) -> SetServiceInvoker {
        SetServiceInvoker {
            elements: self.elements.clone(),
        }
    }
}

/// State key used by the set service to store the element array.
const SET_STATE_KEY: &str = "value";

/// Invoker simulator that models a "Set" virtual object service.
///
/// Each invocation performs a read-modify-write on a single "value" state key
/// that contains a serialized `Vec<u64>`. The handler reads current state via
/// eager state (passed by the simulation like the real invoker's StartMessage),
/// appends a unique element derived from the invocation ID if not already
/// present, and writes the updated array back.
///
/// This mirrors the real Restate SDK pattern:
/// ```js
/// let stored = (await ctx.get("value") ?? []) as number[];
/// let set = new Set(stored);
/// if (!set.has(value)) { stored.push(value); ctx.set("value", stored); }
/// ```
#[derive(Debug)]
pub struct SetServiceInvoker {
    elements: Arc<parking_lot::Mutex<HashMap<InvocationId, u64>>>,
}

impl SetServiceInvoker {
    /// Derives a stable, unique element value from an invocation ID.
    fn element_for(invocation_id: &InvocationId) -> u64 {
        // Use the lower 64 bits of the invocation UUID. These are generated
        // from a seeded RNG in the simulation, so they are unique and stable
        // across snapshot restore.
        let bytes = invocation_id.invocation_uuid().to_bytes();
        u64::from_le_bytes(bytes[8..16].try_into().unwrap())
    }

    /// Deserialize the "value" state key into a Vec<u64>.
    fn deserialize_set(data: &[u8]) -> Vec<u64> {
        // Stored as consecutive little-endian u64s.
        data.chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    /// Serialize a Vec<u64> into bytes for the "value" state key.
    fn serialize_set(elements: &[u64]) -> Bytes {
        let mut buf = Vec::with_capacity(elements.len() * 8);
        for &e in elements {
            buf.extend_from_slice(&e.to_le_bytes());
        }
        Bytes::from(buf)
    }
}

impl InvokerSimulator for SetServiceInvoker {
    fn on_invoke(
        &mut self,
        invocation_id: InvocationId,
        _invocation_target: &InvocationTarget,
        _journal: &InvokeInputJournal,
        eager_state: &[(Bytes, Bytes)],
        _rng: &mut StdRng,
        _clock: &SimulationClock,
    ) -> Vec<Command> {
        let element = Self::element_for(&invocation_id);
        self.elements.lock().insert(invocation_id, element);

        // Read current state from eager state (like StartMessage.state_map)
        let current_value = eager_state
            .iter()
            .find(|(k, _)| k.as_ref() == SET_STATE_KEY.as_bytes())
            .map(|(_, v)| v.clone());

        let mut stored = match current_value {
            Some(ref data) => Self::deserialize_set(data),
            None => Vec::new(),
        };

        // Build GetEagerState result matching what the real invoker would send
        let get_result = match &current_value {
            Some(data) => GetStateResult::Success(data.clone()),
            None => GetStateResult::Void,
        };

        // Append element if not already present (set semantics)
        let already_present = stored.contains(&element);
        if !already_present {
            stored.push(element);
        }

        let new_value = Self::serialize_set(&stored);

        let mut commands = vec![
            // Pin deployment
            Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::PinnedDeployment(PinnedDeployment {
                    deployment_id: DeploymentId::default(),
                    service_protocol_version: ServiceProtocolVersion::V5,
                }),
            })),
            // GetEagerState: record the read (no completion needed, result is inline)
            Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::JournalEntryV2 {
                    entry: StoredRawEntry::new(
                        StoredRawEntryHeader::new(MillisSinceEpoch::UNIX_EPOCH),
                        Entry::Command(JournalCommand::GetEagerState(GetEagerStateCommand {
                            key: ByteString::from(SET_STATE_KEY),
                            result: get_result,
                            name: ByteString::new(),
                        }))
                        .encode::<ServiceProtocolV4Codec>(),
                    ),
                    command_index_to_ack: None,
                },
            })),
        ];

        // SetState: write the updated array (only if we actually modified it)
        if !already_present {
            commands.push(Command::InvokerEffect(Box::new(Effect {
                invocation_id,
                kind: EffectKind::JournalEntryV2 {
                    entry: StoredRawEntry::new(
                        StoredRawEntryHeader::new(MillisSinceEpoch::UNIX_EPOCH),
                        Entry::Command(JournalCommand::SetState(SetStateCommand {
                            key: ByteString::from(SET_STATE_KEY),
                            value: new_value.clone(),
                            name: ByteString::new(),
                        }))
                        .encode::<ServiceProtocolV4Codec>(),
                    ),
                    command_index_to_ack: None,
                },
            })));
        }

        // Output
        commands.push(Command::InvokerEffect(Box::new(Effect {
            invocation_id,
            kind: EffectKind::JournalEntryV2 {
                entry: StoredRawEntry::new(
                    StoredRawEntryHeader::new(MillisSinceEpoch::UNIX_EPOCH),
                    Entry::Command(JournalCommand::Output(OutputCommand {
                        result: OutputResult::Success(Bytes::from(element.to_le_bytes().to_vec())),
                        name: ByteString::new(),
                    }))
                    .encode::<ServiceProtocolV4Codec>(),
                ),
                command_index_to_ack: None,
            },
        })));

        // End
        commands.push(Command::InvokerEffect(Box::new(Effect {
            invocation_id,
            kind: EffectKind::End,
        })));

        commands
    }
}

/// Default service name for Set service invocations.
pub const SET_SERVICE_NAME: &str = "SetService";

/// Default handler name for Set service invocations.
pub const SET_SERVICE_HANDLER: &str = "add";

/// Result of a single simulation step.
#[derive(Debug)]
pub struct StepResult {
    /// The command that was applied.
    pub command: Command,
    /// The time at which the command was applied.
    pub time: MillisSinceEpoch,
    /// Actions emitted by the state machine.
    pub actions: Vec<Action>,
}

/// Represents a scheduled event in the simulation.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum ScheduledEvent {
    /// A timer that should fire at a specific time.
    Timer(TimerKeyValue),
    /// Commands generated by the invoker simulator.
    InvokerCommands(VecDeque<Command>),
}

/// A single-partition deterministic simulation.
///
/// This simulation runs a partition processor state machine in a closed loop,
/// where actions (timer registrations, outbox messages) are converted back
/// into commands that feed back into the state machine.
#[allow(dead_code)]
pub struct PartitionSimulation<S> {
    /// Configuration for this simulation.
    config: PartitionSimulationConfig,
    /// Deterministic random number generator.
    rng: StdRng,
    /// Deterministic clock.
    clock: SimulationClock,
    /// The state machine being simulated.
    state_machine: StateMachine,
    /// Storage backend.
    storage: S,
    /// Partition table for routing cross-partition messages.
    partition_table: PartitionTable,
    /// Invoker simulator for generating invoker effects.
    invoker: Box<dyn InvokerSimulator>,
    /// Queue of pending commands to apply.
    pending_commands: VecDeque<Command>,
    /// Messages destined for other partitions, drained by the cluster simulation.
    outbound_messages: Vec<OutboundMessage>,
    /// Snapshot IDs that this partition has completed.
    snapshot_completions: Vec<ClusterSnapshotId>,
    /// Registered timers, indexed by wake time.
    timers: BTreeMap<MillisSinceEpoch, Vec<TimerKeyValue>>,
    /// Set of timer keys that have been deleted (to handle races).
    deleted_timers: HashSet<TimerKey>,
    /// Invocations currently being "executed" by the invoker simulator.
    active_invocations: HashSet<InvocationId>,
    /// Number of steps executed so far.
    steps_executed: usize,
    /// Track VO keys we've touched for invariant checking.
    /// This allows checking invariants for dynamically generated keys,
    /// not just the hardcoded test keys.
    vo_keys_touched: HashSet<ServiceId>,
    /// Track invocations we've seen for journal integrity checking.
    invocations_seen: HashSet<InvocationId>,
    /// Last timer fire time for monotonicity checking.
    last_timer_fire_time: Option<MillisSinceEpoch>,
    /// Optional trace recorder for determinism verification.
    trace: Option<SimulationTrace>,
    /// Per-snapshot-id tracking of markers sent by this partition.
    markers_sent: HashMap<ClusterSnapshotId, HashSet<PartitionId>>,
    /// Per-snapshot-id tracking of markers received by this partition.
    markers_received: HashMap<ClusterSnapshotId, HashSet<PartitionId>>,
    /// Number of cross-partition messages sent (outbound to other partitions).
    cross_partition_messages_sent: u64,
    /// Number of cross-partition messages received (inbound from other partitions).
    cross_partition_messages_received: u64,
    /// Monotonically increasing applied LSN, written to storage on each step.
    /// Required for `PartitionStore::create_local_snapshot()` which fails if
    /// no applied LSN is present.
    applied_lsn: Lsn,
    /// Snapshot ID awaiting a RocksDB checkpoint. Set by `begin_local_snapshot()`
    /// when the `BeginLocalSnapshot` action fires. Consumed by
    /// `take_pending_checkpoint()` on `PartitionStore`-backed simulations.
    pending_snapshot_checkpoint: Option<ClusterSnapshotId>,
}

#[allow(dead_code)]
impl<S> PartitionSimulation<S>
where
    S: Storage
        + ReadInvocationStatusTable
        + ReadVirtualObjectStatusTable
        + ReadJournalTable
        + ReadStateTable
        + Send,
{
    /// Creates a single-partition simulation (convenience constructor).
    ///
    /// Equivalent to calling `new` with a 1-partition table spanning the full
    /// key range. This preserves backward compatibility with existing tests.
    pub fn new(
        config: PartitionSimulationConfig,
        storage: S,
        invoker_behavior: InvokerBehavior,
    ) -> Self {
        let partition_table =
            PartitionTable::with_equally_sized_partitions(restate_types::Version::MIN, 1);
        Self::with_partition_table(config, storage, partition_table, invoker_behavior)
    }

    /// Creates a partition simulation with an explicit partition table.
    ///
    /// Used by `ClusterSimulation` to create multi-partition setups where
    /// cross-partition messages are routed between partitions.
    pub fn with_partition_table(
        config: PartitionSimulationConfig,
        storage: S,
        partition_table: PartitionTable,
        invoker_behavior: InvokerBehavior,
    ) -> Self {
        let rng = StdRng::seed_from_u64(config.seed);
        let clock = SimulationClock::new_at_base_time();

        let state_machine = StateMachine::new(
            0,    // inbox_seq_number
            0,    // outbox_seq_number
            None, // outbox_head_seq_number
            config.partition_key_range.clone(),
            restate_types::SemanticRestateVersion::unknown(),
            None,
        );

        let invoker: Box<dyn InvokerSimulator> = match invoker_behavior {
            InvokerBehavior::ImmediateSuccess => Box::new(ImmediateSuccessInvoker),
            InvokerBehavior::ImmediateFail {
                error_code,
                message,
            } => Box::new(ImmediateFailInvoker::new(error_code, message)),
            InvokerBehavior::RandomJournal { .. } => {
                unimplemented!(
                    "RandomJournal invoker not yet implemented. \
                     Use ImmediateSuccess, ImmediateFail, Probabilistic, or Custom instead."
                )
            }
            InvokerBehavior::Probabilistic {
                success_rate,
                failure_rate,
            } => Box::new(ProbabilisticInvoker::new(success_rate, failure_rate)),
            InvokerBehavior::Custom(invoker) => invoker,
        };

        Self {
            config,
            rng,
            clock,
            state_machine,
            storage,
            partition_table,
            invoker,
            pending_commands: VecDeque::new(),
            outbound_messages: Vec::new(),
            snapshot_completions: Vec::new(),
            timers: BTreeMap::new(),
            deleted_timers: HashSet::new(),
            active_invocations: HashSet::new(),
            steps_executed: 0,
            vo_keys_touched: HashSet::new(),
            invocations_seen: HashSet::new(),
            last_timer_fire_time: None,
            trace: None,
            markers_sent: HashMap::new(),
            markers_received: HashMap::new(),
            cross_partition_messages_sent: 0,
            cross_partition_messages_received: 0,
            applied_lsn: Lsn::OLDEST,
            pending_snapshot_checkpoint: None,
        }
    }

    /// Returns a reference to the simulation clock.
    pub fn clock(&self) -> &SimulationClock {
        &self.clock
    }

    /// Returns a mutable reference to the storage.
    pub fn storage(&mut self) -> &mut S {
        &mut self.storage
    }

    /// Returns the number of steps executed.
    pub fn steps_executed(&self) -> usize {
        self.steps_executed
    }

    /// Enables trace recording for determinism verification.
    pub fn enable_tracing(&mut self) {
        self.trace = Some(SimulationTrace::new());
    }

    /// Disables trace recording.
    pub fn disable_tracing(&mut self) {
        self.trace = None;
    }

    /// Returns a reference to the current trace, if tracing is enabled.
    pub fn trace(&self) -> Option<&SimulationTrace> {
        self.trace.as_ref()
    }

    /// Takes the trace, leaving tracing disabled.
    pub fn take_trace(&mut self) -> Option<SimulationTrace> {
        self.trace.take()
    }

    /// Returns this partition's ID.
    pub fn partition_id(&self) -> PartitionId {
        self.config.partition_id
    }

    /// Returns the current applied LSN.
    pub fn applied_lsn(&self) -> Lsn {
        self.applied_lsn
    }

    /// Drains outbound messages destined for other partitions.
    /// Called by `ClusterSimulation` after each step to route messages.
    pub fn drain_outbound(&mut self) -> Vec<OutboundMessage> {
        let msgs = std::mem::take(&mut self.outbound_messages);
        self.cross_partition_messages_sent += msgs.len() as u64;
        msgs
    }

    /// Drains completed snapshot IDs.
    pub fn drain_snapshot_completions(&mut self) -> Vec<ClusterSnapshotId> {
        std::mem::take(&mut self.snapshot_completions)
    }

    /// Returns markers sent per snapshot ID (snapshot_id → set of target partition IDs).
    pub fn markers_sent(&self) -> &HashMap<ClusterSnapshotId, HashSet<PartitionId>> {
        &self.markers_sent
    }

    /// Returns markers received per snapshot ID (snapshot_id → set of source partition IDs).
    pub fn markers_received(&self) -> &HashMap<ClusterSnapshotId, HashSet<PartitionId>> {
        &self.markers_received
    }

    /// Returns cross-partition message counts (sent, received).
    pub fn cross_partition_message_counts(&self) -> (u64, u64) {
        (
            self.cross_partition_messages_sent,
            self.cross_partition_messages_received,
        )
    }

    /// Returns true if this partition has pending commands.
    pub fn has_pending_commands(&self) -> bool {
        !self.pending_commands.is_empty()
    }

    /// Enqueues an external command to be processed.
    pub fn enqueue_command(&mut self, command: Command) {
        // Track snapshot markers received and cross-partition message counts
        if let Command::SnapshotMarker {
            snapshot_id,
            from_partition,
            ..
        } = &command
        {
            self.markers_received
                .entry(*snapshot_id)
                .or_default()
                .insert(*from_partition);
        }
        self.pending_commands.push_back(command);
    }

    /// Increments the cross-partition message received counter.
    /// Called by ClusterSimulation when delivering routed messages.
    pub fn record_inbound_message(&mut self) {
        self.cross_partition_messages_received += 1;
    }

    /// Enqueues a new invocation.
    pub fn enqueue_invocation(&mut self, invocation: ServiceInvocation) {
        self.pending_commands
            .push_back(Command::Invoke(Box::new(invocation)));
    }

    /// Creates a random invocation targeting this partition with a random key.
    ///
    /// Uses deterministic ID generation based on the simulation's seeded RNG.
    pub fn random_invocation(&mut self) -> ServiceInvocation {
        let target = InvocationTarget::virtual_object(
            VO_TEST_SERVICE,
            format!("key-{}", self.rng.random::<u32>()),
            VO_TEST_HANDLER,
            restate_types::invocation::VirtualObjectHandlerType::Exclusive,
        );
        self.create_invocation(target)
    }

    /// Creates a random invocation from a small key space to force collisions.
    /// This is useful for testing VO exclusivity invariants.
    ///
    /// Uses deterministic ID generation based on the simulation's seeded RNG.
    pub fn random_vo_invocation(&mut self) -> ServiceInvocation {
        let key_idx = self.rng.random_range(0..VO_TEST_KEYS.len());
        let key = VO_TEST_KEYS[key_idx];
        let target = InvocationTarget::virtual_object(
            VO_TEST_SERVICE,
            key,
            VO_TEST_HANDLER,
            restate_types::invocation::VirtualObjectHandlerType::Exclusive,
        );
        self.create_invocation(target)
    }

    /// Creates an invocation for a specific VO key.
    ///
    /// Uses deterministic ID generation based on the simulation's seeded RNG.
    pub fn invocation_for_key(&mut self, key: &str) -> ServiceInvocation {
        let target = InvocationTarget::virtual_object(
            VO_TEST_SERVICE,
            key,
            VO_TEST_HANDLER,
            restate_types::invocation::VirtualObjectHandlerType::Exclusive,
        );
        self.create_invocation(target)
    }

    /// Creates an invocation targeting a specific set VO key.
    ///
    /// Used by the linearizability test: each set key (e.g., `set-0`, `set-1`)
    /// is a virtual object whose state keys represent set elements.
    pub fn set_invocation(&mut self, set_key: &str) -> ServiceInvocation {
        let target = InvocationTarget::virtual_object(
            SET_SERVICE_NAME,
            set_key,
            SET_SERVICE_HANDLER,
            restate_types::invocation::VirtualObjectHandlerType::Exclusive,
        );
        self.create_invocation(target)
    }

    /// Creates an invocation with a deterministic ID based on the simulation's seeded RNG.
    ///
    /// Uses deterministic partition key from the target's key when available (for virtual
    /// objects and workflows), otherwise falls back to RNG. The invocation UUID is always
    /// generated from RNG to ensure deterministic replay.
    fn create_invocation(&mut self, target: InvocationTarget) -> ServiceInvocation {
        // Use deterministic partition key from target's key if available, otherwise RNG
        let partition_key = target
            .key()
            .map(|key| HashPartitioner::compute_partition_key(&**key))
            .unwrap_or_else(|| self.rng.random());

        // Generate deterministic invocation UUID from RNG (as u128)
        let uuid_high: u64 = self.rng.random();
        let uuid_low: u64 = self.rng.random();
        let uuid = ((uuid_high as u128) << 64) | (uuid_low as u128);
        // Ensure non-zero (required by InvocationUuid)
        let uuid = if uuid == 0 { 1 } else { uuid };

        let invocation_id =
            InvocationId::from_parts(partition_key, InvocationUuid::from_u128(uuid));
        ServiceInvocation::initialize(invocation_id, target, Source::Ingress(Default::default()))
    }

    /// Checks if the simulation should continue.
    pub fn should_continue(&self) -> bool {
        self.steps_executed < self.config.max_steps
            && (!self.pending_commands.is_empty()
                || !self.timers.is_empty()
                || !self.active_invocations.is_empty())
    }

    /// Advances time to fire the next scheduled timer (if any).
    /// Returns true if a timer was fired.
    ///
    /// This method is synchronous and only updates the simulation clock.
    /// For tests that need tokio time integration, use [`advance_to_next_timer_async`].
    fn advance_to_next_timer(&mut self) -> bool {
        if let Some((&wake_time, _)) = self.timers.first_key_value() {
            self.clock.advance_to(wake_time);
            if let Some(timers) = self.timers.remove(&wake_time) {
                for timer in timers {
                    let timer_key = timer.timer_key();
                    if !self.deleted_timers.remove(timer_key) {
                        self.pending_commands.push_back(Command::Timer(timer));
                    }
                }
            }
            true
        } else {
            false
        }
    }

    /// Advances time to fire the next scheduled timer (if any), synchronizing with tokio's paused time.
    /// Returns Ok(true) if a timer was fired, Ok(false) if no timers available.
    /// Returns Err if timer monotonicity was violated.
    ///
    /// This method should be used when running with `start_paused = true` to ensure
    /// that `tokio::time::sleep` and other timer-based operations complete correctly.
    async fn advance_to_next_timer_async(&mut self) -> Result<bool, InvariantViolation> {
        if let Some((&wake_time, _)) = self.timers.first_key_value() {
            // Check timer monotonicity before firing
            if let Some(last_time) = self.last_timer_fire_time
                && wake_time < last_time
            {
                return Err(InvariantViolation::TimerFiredOutOfOrder {
                    current_time: wake_time,
                    last_fire_time: last_time,
                });
            }
            self.last_timer_fire_time = Some(wake_time);

            self.clock.advance_to_async(wake_time).await;
            if let Some(timers) = self.timers.remove(&wake_time) {
                for timer in timers {
                    let timer_key = timer.timer_key();
                    if !self.deleted_timers.remove(timer_key) {
                        self.pending_commands.push_back(Command::Timer(timer));
                    }
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Public entry point for the cluster to advance timers.
    /// Fires the next scheduled timer if available.
    pub async fn advance_timers(&mut self) -> Result<bool, SimulationError> {
        Ok(self.advance_to_next_timer_async().await?)
    }

    /// Loads eager state for a virtual object invocation target.
    async fn load_eager_state(
        &mut self,
        invocation_target: &InvocationTarget,
    ) -> Vec<(Bytes, Bytes)> {
        if let Some(key) = invocation_target.key() {
            let service_id = ServiceId::new(invocation_target.service_name().clone(), key.clone());
            match self.storage.get_all_user_states_for_service(&service_id) {
                Ok(stream) => {
                    use futures::StreamExt;
                    stream
                        .filter_map(|r| async { r.ok() })
                        .collect::<Vec<_>>()
                        .await
                }
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        }
    }

    /// Processes actions emitted by the state machine.
    async fn process_actions(&mut self, actions: &[Action]) {
        for action in actions {
            match action {
                Action::Invoke {
                    invocation_id,
                    invocation_target,
                    invoke_input_journal,
                } => {
                    self.active_invocations.insert(*invocation_id);
                    self.invocations_seen.insert(*invocation_id);
                    // Track VO keys for invariant checking (only small test key space)
                    if let Some(key) = invocation_target.key()
                        && VO_TEST_KEYS.contains(&key.as_ref())
                    {
                        let service_id =
                            ServiceId::new(invocation_target.service_name().clone(), key.clone());
                        self.vo_keys_touched.insert(service_id);
                    }
                    let eager_state = self.load_eager_state(invocation_target).await;
                    let commands = self.invoker.on_invoke(
                        *invocation_id,
                        invocation_target,
                        invoke_input_journal,
                        &eager_state,
                        &mut self.rng,
                        &self.clock,
                    );
                    for cmd in commands {
                        self.pending_commands.push_back(cmd);
                    }
                }
                Action::VQInvoke {
                    invocation_id,
                    invocation_target,
                    invoke_input_journal,
                    ..
                } => {
                    self.active_invocations.insert(*invocation_id);
                    self.invocations_seen.insert(*invocation_id);
                    // Track VO keys for invariant checking (only small test key space)
                    if let Some(key) = invocation_target.key()
                        && VO_TEST_KEYS.contains(&key.as_ref())
                    {
                        let service_id =
                            ServiceId::new(invocation_target.service_name().clone(), key.clone());
                        self.vo_keys_touched.insert(service_id);
                    }
                    let eager_state = self.load_eager_state(invocation_target).await;
                    let commands = self.invoker.on_invoke(
                        *invocation_id,
                        invocation_target,
                        invoke_input_journal,
                        &eager_state,
                        &mut self.rng,
                        &self.clock,
                    );
                    for cmd in commands {
                        self.pending_commands.push_back(cmd);
                    }
                }
                Action::RegisterTimer { timer_value } => {
                    let wake_time = timer_value.wake_up_time();
                    self.timers
                        .entry(wake_time)
                        .or_default()
                        .push(timer_value.clone());
                }
                Action::DeleteTimer { timer_key } => {
                    self.deleted_timers.insert(timer_key.clone());
                }
                Action::NewOutboxMessage { message, .. } => {
                    self.process_outbox_message(message);
                }
                Action::AbortInvocation { invocation_id } => {
                    self.active_invocations.remove(invocation_id);
                }
                Action::BeginLocalSnapshot {
                    snapshot_id,
                    num_partitions,
                } => {
                    self.begin_local_snapshot(*snapshot_id, *num_partitions);
                }
                Action::SnapshotComplete { snapshot_id } => {
                    self.snapshot_completions.push(*snapshot_id);
                }
                Action::SendOutboxAck {
                    to_partition,
                    from_partition,
                    seq,
                } => {
                    self.outbound_messages.push(OutboundMessage {
                        target_partition_id: *to_partition,
                        command: Command::OutboxProcessedAck {
                            from_partition: *from_partition,
                            seq: *seq,
                        },
                    });
                }
                // These actions don't generate feedback commands in simulation
                Action::VQEvent(_)
                | Action::AckStoredCommand { .. }
                | Action::ForwardCompletion { .. }
                | Action::ForwardNotification { .. }
                | Action::IngressResponse { .. }
                | Action::IngressSubmitNotification { .. }
                | Action::ForwardKillResponse { .. }
                | Action::ForwardCancelResponse { .. }
                | Action::ForwardPurgeInvocationResponse { .. }
                | Action::ForwardPurgeJournalResponse { .. }
                | Action::ForwardResumeInvocationResponse { .. }
                | Action::ForwardRestartAsNewInvocationResponse { .. } => {}
            }
        }
    }

    /// Simulates the local snapshot sequence: take a checkpoint, send markers
    /// to all other partitions, then self-enqueue `LocalSnapshotTaken`.
    fn begin_local_snapshot(&mut self, snapshot_id: ClusterSnapshotId, num_partitions: u32) {
        // Record that a checkpoint should be taken. The cluster simulation
        // calls `take_pending_checkpoint()` after each step to create the
        // actual RocksDB checkpoint when running with PartitionStore.
        self.pending_snapshot_checkpoint = Some(snapshot_id);

        // Send SnapshotMarker to every other partition.
        let sent_to = self.markers_sent.entry(snapshot_id).or_default();
        for (target_pid, _) in self.partition_table.iter() {
            if *target_pid == self.config.partition_id {
                continue;
            }
            sent_to.insert(*target_pid);
            self.outbound_messages.push(OutboundMessage {
                target_partition_id: *target_pid,
                command: Command::SnapshotMarker {
                    snapshot_id,
                    from_partition: self.config.partition_id,
                    num_partitions,
                },
            });
        }
        // Self-enqueue LocalSnapshotTaken so the state machine records it.
        self.pending_commands
            .push_back(Command::LocalSnapshotTaken { snapshot_id });
    }

    /// Routes an outbox message: local messages are enqueued directly,
    /// cross-partition messages are collected as outbound.
    fn process_outbox_message(&mut self, message: &OutboxMessage) {
        match message {
            OutboxMessage::ServiceInvocation(invocation) => {
                let target_key = invocation.partition_key();
                if self.config.partition_key_range.contains(&target_key) {
                    self.pending_commands
                        .push_back(Command::Invoke(invocation.clone()));
                } else {
                    let target_pid = self
                        .partition_table
                        .find_partition_id(target_key)
                        .expect("partition key must map to a valid partition");
                    self.outbound_messages.push(OutboundMessage {
                        target_partition_id: target_pid,
                        command: Command::Invoke(invocation.clone()),
                    });
                }
            }
            OutboxMessage::ServiceResponse(response) => {
                let target_key = response.partition_key();
                if self.config.partition_key_range.contains(&target_key) {
                    self.pending_commands
                        .push_back(Command::InvocationResponse(response.clone()));
                } else {
                    let target_pid = self
                        .partition_table
                        .find_partition_id(target_key)
                        .expect("partition key must map to a valid partition");
                    self.outbound_messages.push(OutboundMessage {
                        target_partition_id: target_pid,
                        command: Command::InvocationResponse(response.clone()),
                    });
                }
            }
            OutboxMessage::InvocationTermination(termination) => {
                let target_key = termination.invocation_id.partition_key();
                if self.config.partition_key_range.contains(&target_key) {
                    self.pending_commands
                        .push_back(Command::TerminateInvocation(termination.clone()));
                } else {
                    let target_pid = self
                        .partition_table
                        .find_partition_id(target_key)
                        .expect("partition key must map to a valid partition");
                    self.outbound_messages.push(OutboundMessage {
                        target_partition_id: target_pid,
                        command: Command::TerminateInvocation(termination.clone()),
                    });
                }
            }
            OutboxMessage::AttachInvocation(_) | OutboxMessage::NotifySignal(_) => {
                // Not yet simulated — panic if encountered to surface missing support.
                unimplemented!(
                    "AttachInvocation and NotifySignal outbox messages not yet simulated"
                );
            }
        }
    }

    /// Checks state machine invariants.
    async fn check_invariants(&mut self) -> Result<(), InvariantViolation> {
        self.check_vo_exclusivity().await?;
        // TODO: Re-enable journal sequence check once we handle both journal table v1 and v2.
        // The state machine may write to v1 journal table while we read from v2, causing
        // false positive violations.
        // self.check_journal_sequence().await?;
        Ok(())
    }

    /// Checks the virtual object exclusivity invariant.
    ///
    /// For each VO key we've touched, verifies that:
    /// 1. If locked, the lock holder is in an active state (Invoked/Suspended/Paused)
    /// 2. No two invocations are simultaneously active for the same key
    /// 3. No orphaned locks (locked by invocation that no longer exists)
    async fn check_vo_exclusivity(&mut self) -> Result<(), InvariantViolation> {
        // Check each VO key we've touched during this simulation
        for service_id in &self.vo_keys_touched {
            // Get the lock status from storage
            let lock_status = self
                .storage
                .get_virtual_object_status(service_id)
                .await
                .map_err(|e| InvariantViolation::Custom(format!("Storage error: {}", e)))?;

            if let VirtualObjectStatus::Locked(locked_by) = lock_status {
                // Verify the lock holder is in an active state
                let invocation_status = self
                    .storage
                    .get_invocation_status(&locked_by)
                    .await
                    .map_err(|e| InvariantViolation::Custom(format!("Storage error: {}", e)))?;

                // Check for orphaned lock (invocation completely gone)
                if matches!(invocation_status, InvocationStatus::Free) {
                    return Err(InvariantViolation::OrphanedVoLock {
                        service_id: service_id.clone(),
                        locked_by,
                    });
                }

                let is_active = matches!(
                    invocation_status,
                    InvocationStatus::Invoked(_)
                        | InvocationStatus::Suspended { .. }
                        | InvocationStatus::Paused(_)
                );

                if !is_active {
                    let status_name = match invocation_status {
                        InvocationStatus::Scheduled(_) => "Scheduled",
                        InvocationStatus::Inboxed(_) => "Inboxed",
                        InvocationStatus::Invoked(_) => "Invoked",
                        InvocationStatus::Suspended { .. } => "Suspended",
                        InvocationStatus::Paused(_) => "Paused",
                        InvocationStatus::Completed(_) => "Completed",
                        InvocationStatus::Free => "Free",
                    };
                    return Err(InvariantViolation::VoLockHeldByNonActiveInvocation {
                        service_id: service_id.clone(),
                        locked_by,
                        status: status_name.to_string(),
                    });
                }
            }
        }

        Ok(())
    }

    /// Checks journal entry sequence continuity for active invocations.
    ///
    /// For each invocation that's in an active state (Invoked/Suspended/Paused),
    /// verifies that journal entries are sequential without gaps (0, 1, 2, ..., length-1).
    ///
    /// Note: Completed invocations are skipped because their journals may have been
    /// cleaned up based on retention policy.
    async fn check_journal_sequence(&mut self) -> Result<(), InvariantViolation> {
        use futures::StreamExt;
        use std::pin::pin;

        for invocation_id in &self.invocations_seen {
            let invocation_status = self
                .storage
                .get_invocation_status(invocation_id)
                .await
                .map_err(|e| InvariantViolation::Custom(format!("Storage error: {}", e)))?;

            // Only check active invocations (not completed or free) that have journal metadata
            let is_active = matches!(
                invocation_status,
                InvocationStatus::Invoked(_)
                    | InvocationStatus::Suspended { .. }
                    | InvocationStatus::Paused(_)
            );
            if !is_active {
                continue;
            }

            let Some(journal_meta) = invocation_status.get_journal_metadata() else {
                continue;
            };

            let expected_length = journal_meta.length;
            if expected_length == 0 {
                continue;
            }

            // Read all journal entries and verify sequence continuity
            let journal_stream = self
                .storage
                .get_journal(*invocation_id, expected_length)
                .map_err(|e| InvariantViolation::Custom(format!("Storage error: {}", e)))?;

            let mut pinned_stream = pin!(journal_stream);
            let mut seen_indices = vec![false; expected_length as usize];
            let mut actual_count = 0u32;

            while let Some(result) = pinned_stream.next().await {
                let (index, _entry) = result
                    .map_err(|e| InvariantViolation::Custom(format!("Storage error: {}", e)))?;

                if (index as usize) < seen_indices.len() {
                    seen_indices[index as usize] = true;
                    actual_count += 1;
                }
            }

            // Find missing indices
            let missing_indices: Vec<u32> = seen_indices
                .iter()
                .enumerate()
                .filter_map(|(i, &seen)| if !seen { Some(i as u32) } else { None })
                .collect();

            if !missing_indices.is_empty() {
                return Err(InvariantViolation::JournalSequenceGap {
                    invocation_id: *invocation_id,
                    expected_length,
                    actual_entries: actual_count,
                    missing_indices,
                });
            }
        }

        Ok(())
    }

    /// Executes a single simulation step.
    ///
    /// This takes the next command from the queue (or advances time to fire a timer),
    /// applies it to the state machine, and processes the resulting actions.
    ///
    /// # Requirements
    ///
    /// This method **requires** tokio time to be paused (`start_paused = true`).
    /// It calls `tokio::time::advance()` to synchronize the simulation clock with
    /// tokio's paused time when firing timers.
    ///
    /// Use `#[restate_core::test(start_paused = true)]` or
    /// `#[tokio::test(start_paused = true)]` for tests.
    pub async fn step(&mut self) -> Result<StepResult, SimulationError> {
        // If no pending commands, try to advance to the next timer
        // Use async version to synchronize with tokio's paused time
        if self.pending_commands.is_empty() && !self.advance_to_next_timer_async().await? {
            return Err(SimulationError::NoPendingWork);
        }

        // Get the next command
        let command = self
            .pending_commands
            .pop_front()
            .ok_or(SimulationError::NoPendingWork)?;

        let time = self.clock.now();

        // Create a transaction and apply the command
        let mut transaction = self.storage.transaction();
        let mut action_collector = ActionCollector::default();
        let mut vqueues = VQueuesMetaMut::default();

        // Advance applied LSN before applying the command (mirrors real PP behavior)
        self.applied_lsn = self.applied_lsn.next();

        self.state_machine
            .apply(
                command.clone(),
                time,
                self.applied_lsn,
                &mut transaction,
                &mut action_collector,
                &mut vqueues,
                true, // is_leader
            )
            .await?;

        // Write applied LSN so create_local_snapshot() can find it
        transaction.put_applied_lsn(self.applied_lsn)?;

        // Commit the transaction
        transaction.commit().await?;

        // Process actions to generate feedback commands
        self.process_actions(&action_collector).await;

        // Check invariants if enabled
        if self.config.check_invariants {
            self.check_invariants().await?;
        }

        self.steps_executed += 1;

        let step_result = StepResult {
            command,
            time,
            actions: action_collector,
        };

        // Record the step in the trace if enabled
        if let Some(ref mut trace) = self.trace {
            trace.record(&step_result);
        }

        Ok(step_result)
    }

    /// Runs the simulation until completion or max steps reached.
    ///
    /// Returns the simulation outcome including total steps and any violations.
    pub async fn run(&mut self) -> Result<SimulationOutcome, SimulationError> {
        let mut violations = Vec::new();

        while self.should_continue() {
            match self.step().await {
                Ok(_) => {}
                Err(SimulationError::NoPendingWork) => break,
                Err(SimulationError::Invariant(violation)) => {
                    violations.push(violation);
                    if self.config.check_invariants {
                        // Stop on first invariant violation if checking is enabled
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Ok(SimulationOutcome {
            steps_executed: self.steps_executed,
            final_time: self.clock.now(),
            success: violations.is_empty(),
            violations,
        })
    }
}

/// Specialized methods for `PartitionStore`-backed simulations that support
/// actual RocksDB checkpoints.
impl PartitionSimulation<PartitionStore> {
    /// If a snapshot checkpoint is pending, creates an actual RocksDB checkpoint
    /// in `snapshot_base_path` and returns the snapshot metadata. Returns `None`
    /// if no checkpoint is pending.
    pub async fn take_pending_checkpoint(
        &mut self,
        snapshot_base_path: &Path,
    ) -> Result<Option<(ClusterSnapshotId, LocalPartitionSnapshot)>, SimulationError> {
        let snapshot_id = match self.pending_snapshot_checkpoint.take() {
            Some(id) => id,
            None => return Ok(None),
        };

        let local_snapshot_id = SnapshotId::new();
        let local_snapshot = self
            .storage
            .create_local_snapshot(
                snapshot_base_path,
                Some(self.applied_lsn),
                local_snapshot_id,
            )
            .await?;

        Ok(Some((snapshot_id, local_snapshot)))
    }
}

/// Errors that can occur during simulation.
#[derive(Debug, thiserror::Error)]
pub enum SimulationError {
    #[error("State machine error: {0}")]
    StateMachine(#[from] restate_worker::state_machine::Error),
    #[error("Storage error: {0}")]
    Storage(#[from] restate_storage_api::StorageError),
    #[error("Invariant violation: {0}")]
    Invariant(#[from] InvariantViolation),
    #[error("No pending commands and no timers to fire")]
    NoPendingWork,
}

/// An invariant violation detected during simulation.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum InvariantViolation {
    #[error("Duplicate journal entry for invocation {invocation_id} at index {entry_index}")]
    DuplicateJournalEntry {
        invocation_id: InvocationId,
        entry_index: u32,
    },
    #[error("Invalid state transition for invocation {invocation_id}: {details}")]
    InvalidStateTransition {
        invocation_id: InvocationId,
        details: String,
    },
    #[error(
        "VO lock held by non-active invocation: service={service_id:?}, \
         locked_by={locked_by}, status={status}"
    )]
    VoLockHeldByNonActiveInvocation {
        service_id: ServiceId,
        locked_by: InvocationId,
        status: String,
    },
    #[error(
        "Multiple active invocations for same VO key: service={service_id:?}, \
         invocations={invocations:?}"
    )]
    MultipleActiveInvocationsForVoKey {
        service_id: ServiceId,
        invocations: Vec<InvocationId>,
    },
    #[error(
        "Journal sequence gap: invocation={invocation_id}, expected_length={expected_length}, \
         actual_entries={actual_entries}, missing_indices={missing_indices:?}"
    )]
    JournalSequenceGap {
        invocation_id: InvocationId,
        expected_length: u32,
        actual_entries: u32,
        missing_indices: Vec<u32>,
    },
    #[error(
        "Timer fired out of order: current_time={current_time}, last_fire_time={last_fire_time}"
    )]
    TimerFiredOutOfOrder {
        current_time: MillisSinceEpoch,
        last_fire_time: MillisSinceEpoch,
    },
    #[error(
        "Orphaned VO lock: service={service_id:?} locked by {locked_by} but invocation status is Free"
    )]
    OrphanedVoLock {
        service_id: ServiceId,
        locked_by: InvocationId,
    },
    #[error(
        "Snapshot completion disagreement: snapshot {snapshot_id}, \
         completed_by={completed_by:?}, missing={missing:?}"
    )]
    SnapshotCompletionDisagreement {
        snapshot_id: ClusterSnapshotId,
        completed_by: Vec<PartitionId>,
        missing: Vec<PartitionId>,
    },
    #[error(
        "Snapshot marker not delivered: snapshot {snapshot_id}, \
         sender={sender}, expected_recipient={expected_recipient}"
    )]
    SnapshotMarkerNotDelivered {
        snapshot_id: ClusterSnapshotId,
        sender: PartitionId,
        expected_recipient: PartitionId,
    },
    #[error(
        "Cross-partition message accounting mismatch: total_sent={total_sent}, \
         total_received={total_received}"
    )]
    CrossPartitionMessageLoss {
        total_sent: u64,
        total_received: u64,
    },
    #[error("Invariant check failed: {0}")]
    Custom(String),
}

/// The outcome of running a simulation.
#[derive(Debug)]
#[must_use = "simulation outcome contains success/failure status that should be checked"]
pub struct SimulationOutcome {
    /// Total number of steps executed.
    pub steps_executed: usize,
    /// Final simulation time.
    pub final_time: MillisSinceEpoch,
    /// Whether the simulation completed successfully.
    pub success: bool,
    /// Any invariant violations detected.
    pub violations: Vec<InvariantViolation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulation_config_default() {
        let config = PartitionSimulationConfig::default();
        assert_eq!(config.seed, 0);
        assert_eq!(config.max_steps, 10_000);
        assert!(config.check_invariants);
    }
}
