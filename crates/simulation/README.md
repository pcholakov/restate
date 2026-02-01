# Restate Simulation

Deterministic Simulation Testing (DST) framework for Restate.

## Overview

This crate provides infrastructure for running deterministic simulations of Restate
components, starting with single-partition simulation of the partition processor.

**Goal**: Detect correctness bugs and invariant violations that would be difficult to
find through traditional testing, by running many randomized scenarios with full
reproducibility.

## Design Philosophy

### Closed-Loop Simulation

The simulation runs in a closed loop where:
1. Commands are applied to the state machine
2. Actions are collected from the state machine
3. Relevant actions (timer registrations, outbox messages) are converted back to commands
4. The cycle repeats until termination conditions are met

This allows us to test the state machine in isolation while simulating external
components (invoker, timers, network).

### Sources of Determinism

All sources of non-determinism must be controlled:

| Source | Solution |
|--------|----------|
| Random number generation | Seeded `StdRng` passed to all components |
| Time | Deterministic `SimulationClock` + tokio `start_paused` |
| HashMap iteration order | Use `ahash` (deterministic) or `BTreeMap` |
| `tokio::select!` ordering | Use `rng_seed` in `restate_core::test` macro |
| UUID generation | Pre-generate invocations with fixed IDs |

### Invariant Checking

The simulation can verify invariants after each step:

- **VO Exclusivity**: At most one invocation is active per Virtual Object key
- **State Transitions**: Invocations follow valid state machine transitions
- **Journal Consistency**: No duplicate or missing entries

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    PartitionSimulation                       │
├─────────────────────────────────────────────────────────────┤
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ SimClock     │  │ StdRng       │  │ StateMachine     │  │
│  │ (det. time)  │  │ (seeded)     │  │ (real PP SM)     │  │
│  └──────────────┘  └──────────────┘  └──────────────────┘  │
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ Commands     │  │ Timers       │  │ InvokerSimulator │  │
│  │ (VecDeque)   │  │ (BTreeMap)   │  │ (configurable)   │  │
│  └──────────────┘  └──────────────┘  └──────────────────┘  │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │                  TraceRecorder                        │  │
│  │  Records commands + actions for determinism checking  │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## Modules

### `clock`

Deterministic simulation clock backed by tokio's paused time. The clock can only
advance forward and is shared across all simulation components.

### `partition`

Single-partition simulation that drives the state machine with simulated invoker
responses. Supports multiple invoker behaviors:

- `ImmediateSuccess` - Invocations complete immediately
- `ImmediateFail` - Invocations fail with configurable error
- `Probabilistic` - Random success/failure/timeout with configurable rates
- `Custom` - User-provided `InvokerSimulator` implementation

### `trace`

Trace recording for determinism verification. Captures:

- Step number and simulation time
- Command type and key identifiers (invocation ID, etc.)
- Actions emitted by the state machine

Traces can be serialized to JSON and compared to detect non-determinism.

## Usage

```rust
use restate_simulation::{
    PartitionSimulation, PartitionSimulationConfig, InvokerBehavior,
};

#[restate_core::test(start_paused = true, rng_seed = 42)]
async fn test_simulation() {
    let config = PartitionSimulationConfig {
        seed: 12345,
        max_steps: 1000,
        partition_key_range: PartitionKey::MIN..=PartitionKey::MAX,
        check_invariants: true,
    };

    let mut sim = PartitionSimulation::new(
        config,
        storage,
        InvokerBehavior::Probabilistic {
            success_rate: 0.8,
            failure_rate: 0.1,
        },
    );
    sim.enable_tracing();

    // Enqueue invocations
    for _ in 0..100 {
        let invocation = sim.random_vo_invocation();
        sim.enqueue_invocation(invocation);
    }

    // Run until completion
    let outcome = sim.run().await.expect("Simulation failed");

    assert!(outcome.success, "Invariant violations: {:?}", outcome.violations);

    // Optional: Get trace for comparison
    let trace = sim.take_trace();
}
```

## Future Work

- [ ] Multi-partition simulation with cross-partition messaging
- [ ] Network partition and delay injection
- [ ] State machine snapshot and restore
- [ ] Property-based test generation
- [ ] Integration with external fuzz testing frameworks

## References

- [FoundationDB Testing](https://apple.github.io/foundationdb/testing.html)
- [Jepsen](https://jepsen.io/)
- [TigerBeetle DST](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/DESIGN.md#testing)
