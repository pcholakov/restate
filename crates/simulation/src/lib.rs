// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Deterministic Simulation Testing (DST) framework for Restate.
//!
//! This crate provides infrastructure for running deterministic simulations of Restate
//! components, starting with single-partition simulation of the partition processor.
//!
//! # Design Overview
//!
//! The simulation runs a closed-loop where:
//! 1. Commands are applied to the state machine
//! 2. Actions are collected from the state machine
//! 3. Relevant actions (timer registrations, outbox messages) are converted back to commands
//! 4. The cycle repeats until termination conditions are met
//!
//! All sources of non-determinism are controlled:
//! - Time is controlled via a deterministic clock
//! - Random number generation uses a seeded RNG
//! - Command ordering is deterministic based on the seed
//!
//! # Trace Recording
//!
//! The simulation supports trace recording for verifying determinism:
//!
//! ```ignore
//! let mut sim = PartitionSimulation::new(config, storage, InvokerBehavior::ImmediateSuccess);
//! sim.enable_tracing();
//!
//! // Run simulation
//! sim.enqueue_invocation(invocation);
//! let outcome = sim.run().await?;
//!
//! // Get trace for comparison
//! let trace = sim.take_trace();
//! ```

mod clock;
pub mod cluster;
mod partition;
mod trace;

pub use clock::{SimulationClock, TokioClock};
pub use cluster::{
    ClusterSimulation, ClusterSimulationConfig, ClusterSimulationOutcome, FaultInjection,
};
pub use partition::{
    InvariantViolation, InvokerBehavior, InvokerSimulator, OutboundMessage, PartitionSimulation,
    PartitionSimulationConfig, SimulationError, SimulationOutcome, StepResult, VO_TEST_HANDLER,
    VO_TEST_KEYS, VO_TEST_SERVICE,
};
pub use trace::{ActionTrace, CommandTrace, SimulationTrace, TraceDiff, TraceEntry};
