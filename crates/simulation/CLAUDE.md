# Deterministic Simulation Testing (DST)

Branch: `claude/deterministic-simulation-testing`

## Project Goal

Build a deterministic simulation testing framework for Restate, inspired by FoundationDB/TigerBeetle.
Find correctness bugs and invariant violations by running many randomized scenarios with full reproducibility.

## What's Built

### Core (`crates/simulation/`)

- **`partition.rs`** — `PartitionSimulation<S>`: closed-loop single-partition simulation driving the real `StateMachine`. Commands in → actions out → actions converted back to commands (timers, outbox) → repeat.
- **`cluster.rs`** — `ClusterSimulation`: multi-partition environment with cross-partition message routing, distributed snapshot protocol, fault injection, and per-channel message tracking.
- **`clock.rs`** — `SimulationClock`: deterministic time integrated with tokio paused time and `WallClock::set_recent()`.
- **`trace.rs`** — Trace recording for determinism verification across process runs.
- **`bin/stress.rs`** — Multi-threaded stress binary. Each worker: own tokio runtime (paused), own `PartitionId` for RocksDB isolation. `--seed` for reproduction. Supports both single-partition and cluster modes.

### Distributed Snapshot Protocol (Chandy-Lamport)

Adapted Chandy-Lamport for Restate's Bifrost-based messaging:
- **WAL commands**: `SnapshotStarted`, `SnapshotMarker`, `SnapshotMarkerAck` for coordinating snapshots
- **Coordinator**: `SnapshotCoordinator` in cluster controller initiates snapshots via `SnapshotStarted` to all partitions
- **Protocol phases**: `NeedFlush` → `DrainingSelfLoop { target_lsn }` → `Done`
- **Shuffle gate**: `tokio::sync::watch<bool>` pauses shuffle during marker flush to prevent message loss
- **RocksDB checkpoint**: `PartitionStore::create_local_snapshot()` creates column family export at cut point
- **Ack-based outbox truncation**: replaced legacy shuffle-driven truncation with explicit `SnapshotMarkerAck`
- **Snapshot superseding**: newer snapshot can supersede a stuck older snapshot (A5)

### Invoker Behaviors

`InvokerBehavior` enum: `ImmediateSuccess`, `ImmediateFail`, `Probabilistic` (configurable success/fail/timeout rates), `Custom` (trait object).

### Invariant Checkers (run after each step when enabled)

- **VO exclusivity**: locked VOs must have active lock holder (Invoked/Suspended/Paused), no orphaned locks
- **Timer monotonicity**: timers fire in non-decreasing order
- **Journal sequence**: continuity check exists but disabled pending v1/v2 journal table alignment (`partition.rs:710`)
- **Snapshot completeness**: all partitions complete each initiated snapshot
- **Marker delivery**: all expected snapshot markers are delivered across channels

### Trip Wires

Meta-tests in `tests/integration.rs` that inject faults to verify invariant checkers catch them:
- `z_test_trip_wire_detection`: VO unlock skip → detects VO exclusivity violation
- `z_test_snapshot_marker_drop_trip_wire`: drop snapshot markers → detects incomplete snapshots
- `z_test_message_drop_trip_wire`: drop outbox messages → detects message loss

### Changes to Existing Crates

- `restate-clock`: `WallClock::set_recent()` for simulation time
- `restate-core/derive`: `rng_seed` param in `#[restate_core::test]` for deterministic `tokio::select!`
- `restate-worker`: trip wire mechanism, `StateMachine` + `ActionCollector` exposed for simulation; snapshot protocol in leadership; shuffle gate
- `restate-partition-store`: `create_local_snapshot()` made public for snapshot protocol
- `restate-storage-api`: minor access adjustments for simulation reads

## Determinism Sources Controlled

| Source | Solution |
|--------|----------|
| RNG | Seeded `StdRng` |
| Time | `SimulationClock` + tokio `start_paused` |
| HashMap iteration | State machine uses sets for membership only |
| `tokio::select!` | `rng_seed` in test macro |
| UUID generation | Pre-generated from seeded RNG |

## Key Design Decisions

- Small VO key space (`key-a`, `key-b`, `key-c`) forces collisions to stress exclusivity invariants
- RocksDB singleton constraint means true determinism verification requires comparing traces across separate process runs
- Stress binary uses separate `PartitionId` per worker thread for RocksDB isolation (not separate DB instances)
- Cluster simulation drives StateMachine directly (not through PartitionProcessor), so shuffle gate and RocksDB checkpoint only affect the real partition processor path
- Full-mesh inter-partition channel topology assumed (every partition can send to every other)

## Running

```bash
# Integration tests
cargo nextest run -p restate-simulation

# Stress test (60s default)
cargo run -p restate-simulation --bin simulation-stress --features stress-bin

# Reproduce a failure
cargo run -p restate-simulation --bin simulation-stress --features stress-bin -- --seed <SEED>
```

## Immediate Next Steps

- Report snapshot completion to coordinator (currently TODO — needs metadata store or dedicated log entry)
- Implement SDK service execution in simulator (richer invoker that models real SDK behavior)
- Re-enable journal sequence invariant once v1/v2 table alignment is resolved
