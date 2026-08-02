// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use serde::{Deserialize, Serialize};

use crate::Version;
use crate::identifiers::{PartitionId, SnapshotId};
use crate::logs::{LogId, Lsn, SequenceNumber};
use crate::net::{ServiceTag, define_service, define_unary_message};
use crate::net::{default_wire_codec, define_rpc};

pub struct PartitionManagerService;

fn invalid_lsn() -> Lsn {
    Lsn::INVALID
}

define_service! {
    @service = PartitionManagerService,
    @tag = ServiceTag::PartitionManagerService,
}

define_unary_message! {
    @message = ControlProcessors,
    @service = PartitionManagerService,
}

default_wire_codec!(ControlProcessors);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlProcessors {
    pub min_partition_table_version: Version,
    pub min_logs_table_version: Version,
    pub commands: Vec<ControlProcessor>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ControlProcessor {
    pub partition_id: PartitionId,
    pub command: ProcessorCommand,
    // Version of the current partition configuration used for creating the command for selecting
    // the leader. Restate <= 1.3.2 does not set the current version attribute.
    #[serde(default = "Version::invalid")]
    pub current_version: Version,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, derive_more::Display)]
pub enum ProcessorCommand {
    // #[deprecated(
    //     since = "1.3.3",
    //     note = "Stopping should happen based on the PartitionReplicaSetStates"
    // )]
    Stop,
    // #[deprecated(
    //     since = "1.3.3",
    //     note = "Starting followers should happen based on the PartitionReplicaSetStates"
    // )]
    Follower,
    Leader,
}

define_rpc! {
    @request = CreateSnapshotRequest,
    @response = CreateSnapshotResponse,
    @service = PartitionManagerService,
}

default_wire_codec!(CreateSnapshotRequest);
default_wire_codec!(CreateSnapshotResponse);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSnapshotRequest {
    pub partition_id: PartitionId,
    pub min_target_lsn: Option<Lsn>,
    #[serde(default)]
    pub protect_from_retention: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSnapshotResponse {
    pub result: Result<Snapshot, SnapshotError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_id: SnapshotId,
    pub log_id: LogId,
    /// Legacy trim-safe LSN. Keep this meaning for old Admin nodes in rolling upgrades.
    pub min_applied_lsn: Lsn,
    #[serde(default = "invalid_lsn")]
    pub latest_snapshot_lsn: Lsn,
    #[serde(default)]
    pub snapshot_repository: String,
}

impl Snapshot {
    /// Exact LSN paired with `snapshot_id`, falling back to the legacy field for old workers.
    pub fn exact_min_applied_lsn(&self) -> Lsn {
        if self.latest_snapshot_lsn == Lsn::INVALID {
            self.min_applied_lsn
        } else {
            self.latest_snapshot_lsn
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SnapshotError {
    SnapshotCreationFailed(String),
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use crate::identifiers::{PartitionId, SnapshotId};
    use crate::logs::{LogId, Lsn, SequenceNumber};

    use super::{CreateSnapshotRequest, Snapshot};

    #[derive(Serialize)]
    struct OldCreateSnapshotRequest {
        partition_id: PartitionId,
        min_target_lsn: Option<Lsn>,
    }

    #[derive(Serialize, Deserialize)]
    struct OldSnapshot {
        snapshot_id: SnapshotId,
        log_id: LogId,
        min_applied_lsn: Lsn,
    }

    #[test]
    fn decodes_snapshot_request_from_old_shape() {
        let bytes = flexbuffers::to_vec(OldCreateSnapshotRequest {
            partition_id: PartitionId::MIN,
            min_target_lsn: Some(Lsn::new(42)),
        })
        .unwrap();

        let request: CreateSnapshotRequest = flexbuffers::from_slice(&bytes).unwrap();
        assert_eq!(request.partition_id, PartitionId::MIN);
        assert_eq!(request.min_target_lsn, Some(Lsn::new(42)));
        assert!(!request.protect_from_retention);
    }

    #[test]
    fn decodes_snapshot_response_from_old_shape() {
        let snapshot_id = SnapshotId::new();
        let bytes = flexbuffers::to_vec(OldSnapshot {
            snapshot_id,
            log_id: LogId::MIN,
            min_applied_lsn: Lsn::new(42),
        })
        .unwrap();

        let snapshot: Snapshot = flexbuffers::from_slice(&bytes).unwrap();
        assert_eq!(snapshot.snapshot_id, snapshot_id);
        assert_eq!(snapshot.log_id, LogId::MIN);
        assert_eq!(snapshot.min_applied_lsn, Lsn::new(42));
        assert_eq!(snapshot.latest_snapshot_lsn, Lsn::INVALID);
        assert_eq!(snapshot.exact_min_applied_lsn(), Lsn::new(42));
        assert!(snapshot.snapshot_repository.is_empty());
    }

    #[test]
    fn old_admin_decodes_new_snapshot_with_legacy_trim_safe_lsn() {
        let snapshot_id = SnapshotId::new();
        let bytes = flexbuffers::to_vec(Snapshot {
            snapshot_id,
            log_id: LogId::MIN,
            min_applied_lsn: Lsn::new(10),
            latest_snapshot_lsn: Lsn::new(42),
            snapshot_repository: "s3://bucket/snapshots".to_owned(),
        })
        .unwrap();

        let snapshot: OldSnapshot = flexbuffers::from_slice(&bytes).unwrap();
        assert_eq!(snapshot.snapshot_id, snapshot_id);
        assert_eq!(snapshot.log_id, LogId::MIN);
        assert_eq!(snapshot.min_applied_lsn, Lsn::new(10));
    }
}
