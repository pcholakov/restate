// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Portable descriptions of cluster backup artifacts.

use std::collections::BTreeSet;

use enumset::EnumSet;
use serde::{Deserialize, Serialize};

use crate::identifiers::{PartitionId, SnapshotId};
use crate::logs::{LogId, Lsn};
use crate::nodes_config::{ClusterFeature, ClusterFingerprint};
use crate::partition_table::PartitionTable;
use crate::schema::Schema;
use crate::sharding::KeyRange;
use crate::time::MillisSinceEpoch;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackupConsistency {
    /// Metadata and partition artifacts were captured independently while the cluster was live.
    BestEffort,
}

/// The kind of descriptor validated by [`ClusterBackupDescriptor::validate`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BackupArtifactSet {
    /// The descriptor records topology only. Restore resolves the server-side latest artifacts.
    TopologyOnly,
    /// The descriptor names one exact artifact for every captured partition.
    Complete,
    /// The backup attempt failed after recording only a valid subset of its artifacts.
    Incomplete,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ClusterBackupDescriptorError {
    #[error("unsupported cluster backup descriptor version {0}")]
    UnsupportedVersion(u16),
    #[error("source snapshot repository must not be empty")]
    EmptySnapshotRepository,
    #[error("source cluster name must not be empty")]
    EmptyClusterName,
    #[error("captured partition table must not be empty")]
    EmptyPartitionTable,
    #[error("backup descriptor has duplicate artifact for partition {0}")]
    DuplicatePartition(PartitionId),
    #[error("backup descriptor artifact set does not match the captured partition table")]
    ArtifactSetMismatch,
    #[error(
        "backup descriptor artifact for partition {partition_id} does not match the captured topology"
    )]
    ArtifactTopologyMismatch { partition_id: PartitionId },
}

/// A versioned description of one V0 cluster backup attempt.
///
/// `artifacts` is absent for topology-only recovery of snapshots that existed before descriptor
/// capture. A failed capture may contain a valid subset plus entries in `failures`; such a
/// descriptor remains inspectable but is not restorable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterBackupDescriptor {
    pub version: u16,
    pub created_at: MillisSinceEpoch,
    pub consistency: BackupConsistency,
    /// Snapshot repository URL with query parameters removed, matching server-side normalization.
    pub source_snapshot_repository: String,
    pub source_cluster_name: String,
    pub source_cluster_fingerprint: Option<ClusterFingerprint>,
    /// Cluster-wide behavioral features that affect the interpretation and sharding of data.
    pub source_cluster_features: EnumSet<ClusterFeature>,
    pub partition_table: PartitionTable,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub schema: Option<Schema>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub artifacts: Option<Vec<PartitionBackupArtifact>>,
    /// Failures observed while creating an exact artifact set. A non-empty value makes the
    /// descriptor incomplete and therefore unsuitable for restore.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub failures: Vec<PartitionBackupFailure>,
}

impl ClusterBackupDescriptor {
    pub const VERSION: u16 = 1;

    pub fn validate(&self) -> Result<BackupArtifactSet, ClusterBackupDescriptorError> {
        if self.version != Self::VERSION {
            return Err(ClusterBackupDescriptorError::UnsupportedVersion(
                self.version,
            ));
        }
        if self.source_snapshot_repository.is_empty() {
            return Err(ClusterBackupDescriptorError::EmptySnapshotRepository);
        }
        if self.source_cluster_name.is_empty() {
            return Err(ClusterBackupDescriptorError::EmptyClusterName);
        }
        if self.partition_table.is_empty() {
            return Err(ClusterBackupDescriptorError::EmptyPartitionTable);
        }

        let Some(artifacts) = &self.artifacts else {
            return Ok(if self.failures.is_empty() {
                BackupArtifactSet::TopologyOnly
            } else {
                BackupArtifactSet::Incomplete
            });
        };

        let expected = self
            .partition_table
            .iter_ids()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        for artifact in artifacts {
            if !actual.insert(artifact.partition_id) {
                return Err(ClusterBackupDescriptorError::DuplicatePartition(
                    artifact.partition_id,
                ));
            }
            let Some(partition) = self.partition_table.get(&artifact.partition_id) else {
                return Err(ClusterBackupDescriptorError::ArtifactSetMismatch);
            };
            if partition.log_id() != artifact.log_id || partition.key_range != artifact.key_range {
                return Err(ClusterBackupDescriptorError::ArtifactTopologyMismatch {
                    partition_id: artifact.partition_id,
                });
            }
        }
        if actual == expected && self.failures.is_empty() {
            return Ok(BackupArtifactSet::Complete);
        }
        Ok(BackupArtifactSet::Incomplete)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PartitionBackupArtifact {
    pub partition_id: PartitionId,
    pub snapshot_id: SnapshotId,
    pub log_id: LogId,
    pub min_applied_lsn: Lsn,
    pub key_range: KeyRange,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PartitionBackupFailure {
    pub partition_id: PartitionId,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use enumset::EnumSet;

    use crate::Version;
    use crate::identifiers::{PartitionId, SnapshotId};
    use crate::logs::{LogId, Lsn};
    use crate::partition_table::PartitionTable;
    use crate::time::MillisSinceEpoch;

    use super::{
        BackupArtifactSet, BackupConsistency, ClusterBackupDescriptor,
        ClusterBackupDescriptorError, PartitionBackupArtifact,
    };

    fn descriptor(artifacts: Option<Vec<PartitionBackupArtifact>>) -> ClusterBackupDescriptor {
        let partition_table = PartitionTable::with_equally_sized_partitions(Version::MIN, 2);
        ClusterBackupDescriptor {
            version: ClusterBackupDescriptor::VERSION,
            created_at: MillisSinceEpoch::new(1),
            consistency: BackupConsistency::BestEffort,
            source_snapshot_repository: "s3://bucket/backups".to_owned(),
            source_cluster_name: "source".to_owned(),
            source_cluster_fingerprint: None,
            source_cluster_features: EnumSet::empty(),
            partition_table,
            schema: None,
            artifacts,
            failures: vec![],
        }
    }

    fn artifact(partition_id: u16) -> PartitionBackupArtifact {
        let partition_id = PartitionId::new_unchecked(partition_id);
        PartitionBackupArtifact {
            partition_id,
            snapshot_id: SnapshotId::new(),
            log_id: LogId::default_for_partition(partition_id),
            min_applied_lsn: Lsn::new(1),
            key_range: crate::sharding::KeyRange::new(0, 0),
        }
    }

    #[test]
    fn topology_only_roundtrips() {
        let descriptor = descriptor(None);
        let json = serde_json::to_string(&descriptor).unwrap();
        let decoded: ClusterBackupDescriptor = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.validate(), Ok(BackupArtifactSet::TopologyOnly));
        assert!(decoded.artifacts.is_none());
    }

    #[test]
    fn cluster_features_are_required_in_v1() {
        let mut value = serde_json::to_value(descriptor(None)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("source_cluster_features");

        assert!(serde_json::from_value::<ClusterBackupDescriptor>(value).is_err());
    }

    #[test]
    fn optional_schema_roundtrips() {
        let mut descriptor = descriptor(None);
        descriptor.schema = Some(Default::default());

        let json = serde_json::to_string(&descriptor).unwrap();
        let decoded: ClusterBackupDescriptor = serde_json::from_str(&json).unwrap();

        assert!(decoded.schema.is_some());
        assert_eq!(decoded.validate(), Ok(BackupArtifactSet::TopologyOnly));
    }

    #[test]
    fn exact_artifacts_must_cover_the_captured_topology() {
        let mut descriptor = descriptor(Some(vec![artifact(0)]));
        assert_eq!(
            descriptor.validate(),
            Err(ClusterBackupDescriptorError::ArtifactTopologyMismatch {
                partition_id: PartitionId::new_unchecked(0),
            })
        );

        let artifacts = descriptor
            .partition_table
            .iter()
            .map(|(id, partition)| PartitionBackupArtifact {
                partition_id: *id,
                snapshot_id: SnapshotId::new(),
                log_id: partition.log_id(),
                min_applied_lsn: Lsn::new(1),
                key_range: partition.key_range,
            })
            .collect();
        descriptor.artifacts = Some(artifacts);
        assert_eq!(descriptor.validate(), Ok(BackupArtifactSet::Complete));
    }

    #[test]
    fn duplicate_artifacts_are_rejected() {
        let mut descriptor = descriptor(Some(vec![artifact(0), artifact(0)]));
        let partition = descriptor
            .partition_table
            .get(&PartitionId::new_unchecked(0))
            .unwrap();
        for artifact in descriptor.artifacts.as_mut().unwrap() {
            artifact.log_id = partition.log_id();
            artifact.key_range = partition.key_range;
        }

        assert_eq!(
            descriptor.validate(),
            Err(ClusterBackupDescriptorError::DuplicatePartition(
                PartitionId::new_unchecked(0)
            ))
        );
    }

    #[test]
    fn partial_artifacts_are_inspectable_but_incomplete() {
        let mut descriptor = descriptor(Some(vec![artifact(0)]));
        let partition = descriptor
            .partition_table
            .get(&PartitionId::new_unchecked(0))
            .unwrap();
        let artifact = descriptor.artifacts.as_mut().unwrap().first_mut().unwrap();
        artifact.log_id = partition.log_id();
        artifact.key_range = partition.key_range;

        assert_eq!(descriptor.validate(), Ok(BackupArtifactSet::Incomplete));
    }
}
