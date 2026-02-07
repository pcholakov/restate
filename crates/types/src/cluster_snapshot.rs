// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use restate_clock::time::MillisSinceEpoch;

use crate::identifiers::{ClusterSnapshotId, PartitionId, PartitionKey, SnapshotId};
use crate::logs::{LogId, Lsn};
use crate::nodes_config::ClusterFingerprint;
use crate::partition_table::PartitionTable;
use crate::{Version, Versioned, flexbuffers_storage_encode_decode};

/// Manifest tracking a coordinated cluster-wide snapshot (Chandy-Lamport).
///
/// Written to metadata store on initiation (empty partitions map), then
/// updated with per-partition records as they complete. The `partition_table`
/// captures the source cluster's layout at initiation time — this is critical
/// for recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterSnapshotManifest {
    version: Version,
    pub snapshot_id: ClusterSnapshotId,
    pub cluster_name: String,
    pub cluster_fingerprint: Option<ClusterFingerprint>,
    pub num_partitions: u32,
    pub partition_table: PartitionTable,
    pub initiated_at: MillisSinceEpoch,
    pub completed_at: Option<MillisSinceEpoch>,
    pub partitions: BTreeMap<PartitionId, PartitionSnapshotRecord>,
}

impl ClusterSnapshotManifest {
    pub fn new(
        snapshot_id: ClusterSnapshotId,
        cluster_name: String,
        cluster_fingerprint: Option<ClusterFingerprint>,
        num_partitions: u32,
        partition_table: PartitionTable,
    ) -> Self {
        Self {
            version: Version::MIN,
            snapshot_id,
            cluster_name,
            cluster_fingerprint,
            num_partitions,
            partition_table,
            initiated_at: MillisSinceEpoch::now(),
            completed_at: None,
            partitions: BTreeMap::new(),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.partitions.len() as u32 == self.num_partitions
    }

    pub fn add_partition(&mut self, partition_id: PartitionId, record: PartitionSnapshotRecord) {
        self.partitions.insert(partition_id, record);
        self.version = self.version.next();
    }
}

impl Versioned for ClusterSnapshotManifest {
    fn version(&self) -> Version {
        self.version
    }
}

flexbuffers_storage_encode_decode!(ClusterSnapshotManifest);

/// Record of a single partition's snapshot within a cluster snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionSnapshotRecord {
    version: Version,
    pub snapshot_id: SnapshotId,
    pub log_id: LogId,
    pub min_applied_lsn: Lsn,
    pub key_range: RangeInclusive<PartitionKey>,
    pub node_name: String,
    pub completed_at: MillisSinceEpoch,
}

impl PartitionSnapshotRecord {
    pub fn new(
        snapshot_id: SnapshotId,
        log_id: LogId,
        min_applied_lsn: Lsn,
        key_range: RangeInclusive<PartitionKey>,
        node_name: String,
        completed_at: MillisSinceEpoch,
    ) -> Self {
        Self {
            version: Version::MIN,
            snapshot_id,
            log_id,
            min_applied_lsn,
            key_range,
            node_name,
            completed_at,
        }
    }
}

impl Versioned for PartitionSnapshotRecord {
    fn version(&self) -> Version {
        self.version
    }
}

flexbuffers_storage_encode_decode!(PartitionSnapshotRecord);
