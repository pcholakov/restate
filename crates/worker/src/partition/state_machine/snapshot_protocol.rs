// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use ahash::HashSet;

use restate_types::identifiers::{ClusterSnapshotId, PartitionId};

/// Chandy-Lamport distributed snapshot protocol state, maintained
/// deterministically by the state machine as it processes commands from
/// the log. Only one snapshot may be active at a time.
#[derive(Debug)]
pub(crate) struct SnapshotProtocol {
    pub snapshot_id: ClusterSnapshotId,
    num_partitions: u32,
    markers_received: HashSet<PartitionId>,
    local_snapshot_taken: bool,
}

impl SnapshotProtocol {
    pub fn new(snapshot_id: ClusterSnapshotId, num_partitions: u32) -> Self {
        Self {
            snapshot_id,
            num_partitions,
            markers_received: HashSet::default(),
            local_snapshot_taken: false,
        }
    }

    pub fn record_marker(&mut self, from_partition: PartitionId) {
        self.markers_received.insert(from_partition);
    }

    pub fn mark_local_snapshot_taken(&mut self) {
        self.local_snapshot_taken = true;
    }

    /// Returns true when the local snapshot has been taken and markers
    /// have been received from every other partition.
    pub fn is_complete(&self) -> bool {
        let expected_markers = self.num_partitions.saturating_sub(1) as usize;
        self.local_snapshot_taken && self.markers_received.len() >= expected_markers
    }
}
