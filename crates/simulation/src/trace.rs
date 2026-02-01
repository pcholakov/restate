// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Trace recording for deterministic simulation testing.
//!
//! This module provides a lightweight trace format that captures the essential
//! information from simulation steps to verify determinism across runs.
//!
//! # Usage
//!
//! ```ignore
//! let mut trace = SimulationTrace::new();
//! while sim.should_continue() {
//!     let result = sim.step().await?;
//!     trace.record(&result);
//! }
//!
//! // Compare with another trace for determinism verification
//! assert_eq!(trace1, trace2, "Non-determinism detected!");
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

use restate_types::identifiers::{InvocationId, WithInvocationId};
use restate_types::time::MillisSinceEpoch;
use restate_wal_protocol::Command;
use restate_worker::state_machine::Action;

use crate::StepResult;

/// A single trace entry capturing one simulation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEntry {
    /// Step number (0-indexed).
    pub step: usize,
    /// Simulation time when the step was executed.
    pub time: MillisSinceEpoch,
    /// The command variant that was applied.
    pub command: CommandTrace,
    /// Actions produced by the state machine.
    pub actions: Vec<ActionTrace>,
}

/// Lightweight representation of a Command for tracing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandTrace {
    Invoke {
        invocation_id: InvocationId,
    },
    InvocationResponse {
        target_invocation_id: InvocationId,
    },
    InvokerEffect {
        invocation_id: InvocationId,
        effect_kind: String,
    },
    Timer {
        invocation_id: InvocationId,
    },
    TerminateInvocation {
        invocation_id: InvocationId,
    },
    PurgeInvocation {
        invocation_id: InvocationId,
    },
    NotifySignal {
        invocation_id: InvocationId,
    },
    Other {
        variant: String,
    },
}

impl CommandTrace {
    pub fn from_command(command: &Command) -> Self {
        match command {
            Command::Invoke(inv) => CommandTrace::Invoke {
                invocation_id: inv.invocation_id,
            },
            Command::InvocationResponse(resp) => CommandTrace::InvocationResponse {
                target_invocation_id: resp.invocation_id(),
            },
            Command::InvokerEffect(effect) => CommandTrace::InvokerEffect {
                invocation_id: effect.invocation_id,
                effect_kind: format!("{:?}", std::mem::discriminant(&effect.kind)),
            },
            Command::Timer(timer) => {
                let invocation_id = timer.invocation_id();
                CommandTrace::Timer { invocation_id }
            }
            Command::TerminateInvocation(term) => CommandTrace::TerminateInvocation {
                invocation_id: term.invocation_id,
            },
            Command::PurgeInvocation(purge) => CommandTrace::PurgeInvocation {
                invocation_id: purge.invocation_id,
            },
            Command::NotifySignal(signal) => CommandTrace::NotifySignal {
                invocation_id: signal.invocation_id,
            },
            _ => CommandTrace::Other {
                variant: format!("{:?}", std::mem::discriminant(command)),
            },
        }
    }
}

/// Lightweight representation of an Action for tracing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionTrace {
    Invoke {
        invocation_id: InvocationId,
    },
    VQInvoke {
        invocation_id: InvocationId,
    },
    NewOutboxMessage {
        seq_number: u64,
    },
    RegisterTimer {
        wake_time: MillisSinceEpoch,
    },
    DeleteTimer,
    AckStoredCommand {
        invocation_id: InvocationId,
        command_index: u32,
    },
    ForwardCompletion {
        invocation_id: InvocationId,
    },
    ForwardNotification {
        invocation_id: InvocationId,
    },
    AbortInvocation {
        invocation_id: InvocationId,
    },
    IngressResponse,
    IngressSubmitNotification,
    VQEvent,
    Other {
        variant: String,
    },
}

impl ActionTrace {
    pub fn from_action(action: &Action) -> Self {
        match action {
            Action::Invoke { invocation_id, .. } => ActionTrace::Invoke {
                invocation_id: *invocation_id,
            },
            Action::VQInvoke { invocation_id, .. } => ActionTrace::VQInvoke {
                invocation_id: *invocation_id,
            },
            Action::NewOutboxMessage { seq_number, .. } => ActionTrace::NewOutboxMessage {
                seq_number: *seq_number,
            },
            Action::RegisterTimer { timer_value } => ActionTrace::RegisterTimer {
                wake_time: timer_value.wake_up_time(),
            },
            Action::DeleteTimer { .. } => ActionTrace::DeleteTimer,
            Action::AckStoredCommand {
                invocation_id,
                command_index,
            } => ActionTrace::AckStoredCommand {
                invocation_id: *invocation_id,
                command_index: *command_index,
            },
            Action::ForwardCompletion { invocation_id, .. } => ActionTrace::ForwardCompletion {
                invocation_id: *invocation_id,
            },
            Action::ForwardNotification { invocation_id, .. } => ActionTrace::ForwardNotification {
                invocation_id: *invocation_id,
            },
            Action::AbortInvocation { invocation_id } => ActionTrace::AbortInvocation {
                invocation_id: *invocation_id,
            },
            Action::IngressResponse { .. } => ActionTrace::IngressResponse,
            Action::IngressSubmitNotification { .. } => ActionTrace::IngressSubmitNotification,
            Action::VQEvent(_) => ActionTrace::VQEvent,
            _ => ActionTrace::Other {
                variant: format!("{:?}", std::mem::discriminant(action)),
            },
        }
    }
}

/// A trace of simulation execution.
///
/// This captures the sequence of commands and actions executed during a simulation,
/// allowing comparison between runs to verify determinism.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationTrace {
    /// Trace entries in order of execution.
    entries: Vec<TraceEntry>,
}

impl SimulationTrace {
    /// Creates a new empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a step result into the trace.
    pub fn record(&mut self, step_result: &StepResult) {
        let entry = TraceEntry {
            step: self.entries.len(),
            time: step_result.time,
            command: CommandTrace::from_command(&step_result.command),
            actions: step_result
                .actions
                .iter()
                .map(ActionTrace::from_action)
                .collect(),
        };
        self.entries.push(entry);
    }

    /// Returns the number of entries in the trace.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the trace is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the entries in the trace.
    pub fn entries(&self) -> &[TraceEntry] {
        &self.entries
    }

    /// Compares this trace with another and returns a diff report.
    ///
    /// Returns `Ok(())` if traces are identical, otherwise returns an error
    /// describing the first point of divergence.
    pub fn compare(&self, other: &SimulationTrace) -> Result<(), TraceDiff> {
        if self.entries.len() != other.entries.len() {
            return Err(TraceDiff::LengthMismatch {
                expected: self.entries.len(),
                actual: other.entries.len(),
            });
        }

        for (i, (a, b)) in self.entries.iter().zip(other.entries.iter()).enumerate() {
            if a != b {
                return Err(TraceDiff::EntryMismatch {
                    step: i,
                    expected: Box::new(a.clone()),
                    actual: Box::new(b.clone()),
                });
            }
        }

        Ok(())
    }

    /// Serializes the trace to JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserializes a trace from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Returns a fingerprint hash of the trace for quick comparison.
    ///
    /// This is useful for quickly detecting non-determinism across runs without
    /// comparing full traces. If two traces have the same fingerprint, they are
    /// very likely identical (but should be verified with `compare()` to be certain).
    ///
    /// The fingerprint captures:
    /// - Total number of entries
    /// - First entry's step number and time
    /// - Last entry's step number and time
    /// - Total action count across all entries
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        self.entries.len().hash(&mut hasher);

        if let Some(first) = self.entries.first() {
            first.step.hash(&mut hasher);
            first.time.as_u64().hash(&mut hasher);
            std::mem::discriminant(&first.command).hash(&mut hasher);
        }

        if let Some(last) = self.entries.last() {
            last.step.hash(&mut hasher);
            last.time.as_u64().hash(&mut hasher);
            std::mem::discriminant(&last.command).hash(&mut hasher);
        }

        // Hash total action count
        let total_actions: usize = self.entries.iter().map(|e| e.actions.len()).sum();
        total_actions.hash(&mut hasher);

        hasher.finish()
    }
}

/// Describes a difference between two traces.
#[derive(Debug, Clone)]
pub enum TraceDiff {
    /// Traces have different lengths.
    LengthMismatch { expected: usize, actual: usize },
    /// Traces diverge at a specific step.
    EntryMismatch {
        step: usize,
        expected: Box<TraceEntry>,
        actual: Box<TraceEntry>,
    },
}

impl fmt::Display for TraceDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceDiff::LengthMismatch { expected, actual } => {
                write!(
                    f,
                    "Trace length mismatch: expected {} steps, got {}",
                    expected, actual
                )
            }
            TraceDiff::EntryMismatch {
                step,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "Trace diverged at step {}:\n  Expected: {:?}\n  Actual:   {:?}",
                    step, expected.command, actual.command
                )?;
                if expected.actions != actual.actions {
                    write!(
                        f,
                        "\n  Expected actions: {:?}\n  Actual actions:   {:?}",
                        expected.actions, actual.actions
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for TraceDiff {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_traces_equal() {
        let t1 = SimulationTrace::new();
        let t2 = SimulationTrace::new();
        assert!(t1.compare(&t2).is_ok());
    }

    #[test]
    fn test_trace_serialization_roundtrip() {
        let trace = SimulationTrace::new();
        let json = trace.to_json().unwrap();
        let restored = SimulationTrace::from_json(&json).unwrap();
        assert_eq!(trace, restored);
    }
}
