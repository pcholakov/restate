// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

pub mod keys {
    //! Keys of values stored in the metadata store

    use crate::identifiers::{ClusterSnapshotId, PartitionId};
    use bytestring::ByteString;

    // todo: remove
    pub static NODES_CONFIG_KEY: ByteString = ByteString::from_static("nodes_config");
    pub static BIFROST_CONFIG_KEY: ByteString = ByteString::from_static("bifrost_config");
    pub static PARTITION_TABLE_KEY: ByteString = ByteString::from_static("partition_table");
    pub static SCHEMA_INFORMATION_KEY: ByteString = ByteString::from_static("schema_registry");
    // end todo

    pub static PARTITION_PROCESSOR_EPOCH_PREFIX: &str = "pp_epoch";
    pub fn partition_processor_epoch_key(partition_id: PartitionId) -> ByteString {
        ByteString::from(format!("{PARTITION_PROCESSOR_EPOCH_PREFIX}_{partition_id}"))
    }

    static CLUSTER_SNAPSHOT_MANIFEST_PREFIX: &str = "cluster_snapshot";
    static CLUSTER_SNAPSHOT_PARTITION_PREFIX: &str = "cluster_snapshot_part";

    /// Key for the cluster snapshot manifest in metadata store.
    pub fn cluster_snapshot_manifest_key(id: ClusterSnapshotId) -> ByteString {
        ByteString::from(format!(
            "{}_{}",
            CLUSTER_SNAPSHOT_MANIFEST_PREFIX,
            id.as_u64()
        ))
    }

    /// Per-partition completion key within a cluster snapshot.
    pub fn cluster_snapshot_partition_key(id: ClusterSnapshotId, pid: PartitionId) -> ByteString {
        ByteString::from(format!(
            "{}_{}_{}",
            CLUSTER_SNAPSHOT_PARTITION_PREFIX,
            id.as_u64(),
            pid
        ))
    }
}
