// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#![allow(dead_code)]

use ahash::HashMap;

use crate::identifiers::{LeaderEpoch, PartitionId};
use crate::replication::{NodeSet, ReplicationProperty};
use crate::time::MillisSinceEpoch;
use crate::{GenerationalNodeId, Version, Versioned, flexbuffers_storage_encode_decode};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EpochMetadata {
    version: Version,
    leader_metadata: LeaderMetadata,
    // Optional fields for forward compatibility with newer versions
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<PartitionConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<PartitionConfiguration>,
    // Optional epoch field for forward compatibility
    #[serde(skip_serializing_if = "Option::is_none")]
    next_epoch: Option<LeaderEpoch>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LeaderMetadata {
    partition_id: PartitionId,
    node_id: GenerationalNodeId,
}

/// The Partition configuration contains information about which nodes run partition processors for
/// the given partition.
#[serde_with::serde_as]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PartitionConfiguration {
    pub(crate) version: Version,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    replication: ReplicationProperty,
    replica_set: NodeSet,
    modified_at: MillisSinceEpoch,
    context: HashMap<String, String>,
}

impl Versioned for EpochMetadata {
    fn version(&self) -> Version {
        self.version
    }
}

impl EpochMetadata {
    pub fn new(node_id: GenerationalNodeId, partition_id: PartitionId) -> Self {
        Self {
            version: Version::MIN,
            leader_metadata: LeaderMetadata {
                node_id,
                partition_id,
            },
            current: None,
            next: None,
            next_epoch: None,
        }
    }

    pub fn epoch(&self) -> LeaderEpoch {
        // todo think about aligning Version and LeaderEpoch types
        let version: u32 = self.version.into();
        LeaderEpoch::from(u64::from(version))
    }

    pub fn partition_id(&self) -> PartitionId {
        self.leader_metadata.partition_id
    }

    pub fn node_id(&self) -> GenerationalNodeId {
        self.leader_metadata.node_id
    }

    pub fn claim_leadership(self, node_id: GenerationalNodeId, partition_id: PartitionId) -> Self {
        Self {
            version: self.version.next(),
            leader_metadata: LeaderMetadata {
                node_id,
                partition_id,
            },
            // Preserve optional fields for forward compatibility
            current: self.current,
            next: self.next,
            next_epoch: self.next_epoch,
        }
    }
}

flexbuffers_storage_encode_decode!(EpochMetadata);

#[cfg(test)]
mod tests {
    use crate::GenerationalNodeId;
    use crate::epoch::EpochMetadata;
    use crate::identifiers::{LeaderEpoch, PartitionId};

    #[test]
    fn basic_operations() {
        let node_id = GenerationalNodeId::new(1, 1);
        let other_node_id = GenerationalNodeId::new(2, 1);

        let epoch = EpochMetadata::new(node_id, PartitionId::from(0));

        assert_eq!(epoch.epoch(), LeaderEpoch::INITIAL);
        assert_eq!(epoch.partition_id(), PartitionId::from(0));
        assert_eq!(epoch.node_id(), node_id);

        let next_epoch = epoch.claim_leadership(other_node_id, PartitionId::from(1));

        assert_eq!(next_epoch.epoch(), LeaderEpoch::from(2));
        assert_eq!(next_epoch.partition_id(), PartitionId::from(1));
        assert_eq!(next_epoch.node_id(), other_node_id);
    }
}
