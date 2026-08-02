// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! V0 restore is deliberately a single-node bootstrap operation. It establishes a fresh target
//! cluster from a portable backup descriptor; it does not restore a multi-node deployment.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use restate_core::MetadataWriter;
use restate_partition_store::PartitionStoreManager;
use restate_partition_store::snapshots::{SnapshotRecord, SnapshotRepository, SnapshotSource};
use restate_storage_api::Transaction;
use restate_storage_api::fsm_table::{ReadFsmTable, WriteFsmTable};
use restate_types::cluster_backup::{
    BackupArtifactSet, ClusterBackupDescriptor, PartitionBackupArtifact,
};
use restate_types::config::{CommonOptions, MetadataClientKind, SnapshotsOptions};
use restate_types::epoch::EpochMetadata;
use restate_types::logs::builder::LogsBuilder;
use restate_types::logs::metadata::{
    Chain, LogletParams, Logs, LogsConfiguration, ProviderConfiguration, ProviderKind,
    SegmentIndex, new_single_node_loglet_params,
};
use restate_types::logs::{LogId, LogletId, Lsn, SequenceNumber};
use restate_types::metadata::Precondition;
use restate_types::metadata_store::keys::partition_processor_epoch_key;
use restate_types::nodes_config::{ClusterFingerprint, Role};
use restate_types::partition_table::{
    Partition, PartitionReplication, PartitionTable, PartitionTableBuilder,
};
use restate_types::partitions::PartitionConfiguration;
use restate_types::replicated_loglet::ReplicatedLogletParams;
use restate_types::replication::{NodeSet, ReplicationProperty};
use restate_types::{GenerationalNodeId, Version};

use crate::{
    create_initial_nodes_configuration_with_fingerprint, write_initial_logs_dont_fail_if_it_exists,
    write_initial_value_dont_fail_if_it_exists,
};

#[derive(Debug, Clone, Copy)]
pub struct RestoreOptions {
    pub restore_schema: bool,
    pub preserve_cluster_fingerprint: bool,
}

pub struct RestorePlan {
    descriptor: ClusterBackupDescriptor,
    artifacts: Vec<ResolvedArtifact>,
    target_fingerprint: ClusterFingerprint,
    restore_schema: bool,
    provider: ProviderKind,
    source_repository: SnapshotRepository,
}

#[derive(Debug)]
struct ResolvedArtifact {
    partition: Partition,
    record: SnapshotRecord,
}

#[derive(Debug)]
struct ImportedPartition {
    partition: Partition,
    cached_epoch: Option<restate_storage_api::fsm_table::CachedEpochMetadata>,
    applied_lsn: Lsn,
}

impl RestorePlan {
    pub fn partition_count(&self) -> usize {
        self.artifacts.len()
    }

    pub fn schema_restored(&self) -> bool {
        self.restore_schema && self.descriptor.schema.is_some()
    }

    pub fn target_fingerprint(&self) -> ClusterFingerprint {
        self.target_fingerprint
    }
}

/// Resolves every backup artifact without touching local partition stores or metadata. For a
/// topology-only descriptor, this pins the current `latest.json` target before the caller can
/// begin downloading it.
pub async fn preflight(
    descriptor: ClusterBackupDescriptor,
    options: RestoreOptions,
    common_options: &CommonOptions,
    provider: ProviderKind,
    snapshots_options: &SnapshotsOptions,
    restore_staging_dir: PathBuf,
) -> anyhow::Result<RestorePlan> {
    validate_target_configuration(common_options, provider)?;
    let artifact_set = descriptor.validate().context("invalid backup descriptor")?;
    ensure!(
        artifact_set != BackupArtifactSet::Incomplete,
        "an incomplete backup descriptor cannot be restored"
    );
    ensure!(
        !options.restore_schema || descriptor.schema.is_some(),
        "restoring schema requires a backup descriptor containing schema metadata"
    );

    let source_destination = normalized_repository_root(
        &descriptor.source_snapshot_repository,
        "source snapshot repository",
    )?;
    ensure_distinct_repository_roots(&source_destination, snapshots_options)?;
    let source_repository = SnapshotRepository::new_for_source(
        snapshots_options,
        source_destination,
        restore_staging_dir,
    )
    .await
    .context("failed opening source snapshot repository")?;

    let target_fingerprint = target_fingerprint(
        options.preserve_cluster_fingerprint,
        descriptor.source_cluster_fingerprint,
    )?;
    let source = SnapshotSource::new(
        descriptor.source_cluster_name.clone(),
        descriptor.source_cluster_fingerprint,
    );

    let mut artifacts = Vec::with_capacity(descriptor.partition_table.len());
    for (partition_id, partition) in descriptor.partition_table.iter() {
        let record = match artifact_set {
            BackupArtifactSet::TopologyOnly => source_repository
                .resolve_latest_for_source(*partition_id, &source)
                .await
                .with_context(|| {
                    format!("failed resolving latest snapshot for partition {partition_id}")
                })?
                .ok_or_else(|| {
                    anyhow::anyhow!("no snapshot exists for partition {partition_id}")
                })?,
            BackupArtifactSet::Complete => {
                let artifact = exact_artifact(&descriptor, *partition_id)?;
                source_repository
                    .inspect_exact(
                        artifact.partition_id,
                        artifact.snapshot_id,
                        artifact.min_applied_lsn,
                        &source,
                    )
                    .await
                    .with_context(|| {
                        format!("failed inspecting snapshot for partition {partition_id}")
                    })?
            }
            BackupArtifactSet::Incomplete => unreachable!("checked above"),
        };
        validate_record(partition, &record)?;
        ensure!(
            record.min_applied_lsn != Lsn::MAX,
            "snapshot for partition {partition_id} is at the maximum LSN and cannot be resumed"
        );
        artifacts.push(ResolvedArtifact {
            partition: partition.clone(),
            record,
        });
    }

    Ok(RestorePlan {
        descriptor,
        artifacts,
        target_fingerprint,
        restore_schema: options.restore_schema,
        provider,
        source_repository,
    })
}

/// Applies a preflighted V0 restore. Each already-validated artifact is downloaded and imported
/// before moving to the next one, bounding restore staging space to one partition snapshot. No
/// cluster metadata is published until every partition is imported. `PartitionTable` is published
/// last, so node initialization and worker roles remain blocked until imported stores and their
/// epoch floors are in place.
pub async fn apply(
    plan: RestorePlan,
    metadata_writer: &MetadataWriter,
    common_options: &CommonOptions,
    partition_store_manager: &PartitionStoreManager,
) -> anyhow::Result<bool> {
    let provider = plan.provider;
    let source_cluster_features = plan.descriptor.source_cluster_features;
    let mut imported_partitions = Vec::with_capacity(plan.artifacts.len());
    for artifact in plan.artifacts {
        let minimum_applied_lsn = artifact.record.min_applied_lsn;
        let snapshot = plan
            .source_repository
            .download_record(artifact.record)
            .await
            .with_context(|| {
                format!(
                    "failed downloading snapshot for partition {}",
                    artifact.partition.partition_id
                )
            })?;
        let partition = artifact.partition;
        let mut store = partition_store_manager
            .open_from_snapshot(&partition, snapshot)
            .await
            .with_context(|| {
                format!(
                    "failed importing snapshot for partition {}",
                    partition.partition_id
                )
            })?;
        let applied_lsn = store
            .get_applied_lsn()
            .await
            .with_context(|| {
                format!(
                    "failed reading imported applied LSN for partition {}",
                    partition.partition_id
                )
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "imported snapshot for partition {} has no applied LSN",
                    partition.partition_id
                )
            })?;
        ensure!(
            applied_lsn >= minimum_applied_lsn,
            "imported applied LSN {applied_lsn} is below snapshot lower bound {minimum_applied_lsn} for partition {}",
            partition.partition_id
        );
        ensure!(
            applied_lsn != Lsn::MAX,
            "imported snapshot for partition {} is at the maximum LSN and cannot be resumed",
            partition.partition_id
        );
        let cached_epoch = store.get_partition_config_state().await.with_context(|| {
            format!(
                "failed reading imported epoch state for partition {}",
                partition.partition_id
            )
        })?;
        if !plan.restore_schema {
            let mut transaction = store.transaction();
            transaction.delete_schema().with_context(|| {
                format!(
                    "failed removing imported schema for partition {}",
                    partition.partition_id
                )
            })?;
            transaction.commit().await.with_context(|| {
                format!(
                    "failed committing schema removal for partition {}",
                    partition.partition_id
                )
            })?;
        }
        imported_partitions.push(ImportedPartition {
            partition,
            cached_epoch,
            applied_lsn,
        });
    }

    // Build chains from the state actually imported from the SST, not the repository metadata's
    // conservative lower bound. Reusing already-applied sequence numbers can make the restored
    // state machine silently discard new commands.
    let logs = bootstrap_target_logs(&imported_partitions, provider)?;

    let nodes_configuration = create_initial_nodes_configuration_with_fingerprint(
        common_options,
        source_cluster_features,
        plan.target_fingerprint,
    );
    let newly_provisioned = metadata_writer
        .raw_metadata_store_client()
        .provision(&nodes_configuration)
        .await
        .context("failed reserving the unprovisioned target cluster")?;
    if !newly_provisioned {
        return Ok(false);
    }

    write_initial_logs_dont_fail_if_it_exists(metadata_writer, logs)
        .await
        .context("failed writing restored log metadata")?;

    for imported in imported_partitions {
        let partition = imported.partition;
        let (version_floor, epoch_floor) = imported
            .cached_epoch
            .map(|cached| (cached.version, cached.leader_epoch))
            .unwrap_or_else(|| (Version::INVALID, Default::default()));
        let epoch = EpochMetadata::bootstrap_after(
            PartitionConfiguration::new(
                ReplicationProperty::new_unchecked(1),
                NodeSet::from_single(GenerationalNodeId::INITIAL_NODE_ID.as_plain()),
                Default::default(),
            ),
            version_floor,
            epoch_floor,
        );
        metadata_writer
            .raw_metadata_store_client()
            .put(
                partition_processor_epoch_key(partition.partition_id),
                &epoch,
                Precondition::DoesNotExist,
            )
            .await
            .with_context(|| {
                format!(
                    "failed seeding epoch metadata for partition {}",
                    partition.partition_id
                )
            })?;
    }

    if plan.restore_schema
        && let Some(schema) = plan.descriptor.schema
    {
        write_initial_value_dont_fail_if_it_exists(metadata_writer, Arc::new(schema))
            .await
            .context("failed writing restored schema")?;
    }

    let partition_table = single_node_partition_table(plan.descriptor.partition_table);
    write_initial_value_dont_fail_if_it_exists(metadata_writer, Arc::new(partition_table))
        .await
        .context("failed writing restored partition table")?;

    Ok(true)
}

pub(crate) fn validate_target_configuration(
    common_options: &CommonOptions,
    provider: ProviderKind,
) -> anyhow::Result<()> {
    ensure!(
        !common_options.auto_provision,
        "cluster restore requires auto-provision to be disabled"
    );
    ensure!(
        common_options
            .force_node_id
            .is_none_or(|node_id| { node_id == GenerationalNodeId::INITIAL_NODE_ID.as_plain() }),
        "cluster restore requires the bootstrap node ID to be {} when force-node-id is configured",
        GenerationalNodeId::INITIAL_NODE_ID.as_plain()
    );
    ensure!(
        common_options.roles.contains(Role::Worker),
        "cluster restore requires the bootstrap node to have the worker role"
    );
    ensure!(
        common_options.roles.contains(Role::MetadataServer)
            && matches!(
                &common_options.metadata_client.kind,
                MetadataClientKind::Replicated { .. }
            ),
        "V0 cluster restore requires the bootstrap node to host the built-in metadata server; external metadata backends need a durable pre-import reservation protocol"
    );
    if provider == ProviderKind::Replicated {
        ensure!(
            common_options.roles.contains(Role::LogServer),
            "the replicated log provider requires the bootstrap node to have the log-server role"
        );
    }
    ensure!(
        provider != ProviderKind::InMemory,
        "cluster restore cannot use the non-durable in-memory log provider"
    );
    Ok(())
}

fn normalized_repository_root(repository: &str, description: &str) -> anyhow::Result<url::Url> {
    let mut repository =
        url::Url::parse(repository).with_context(|| format!("failed parsing {description} URL"))?;
    // Keep this aligned with SnapshotRepository construction: query parameters configure the
    // object-store client and are not part of the repository object prefix.
    repository.set_query(None);
    // object_store::path treats a non-root trailing slash as the same prefix. Normalize it here so
    // source/target alias detection uses object-namespace semantics rather than URL spelling.
    if repository.path() != "/" {
        let path = repository.path().trim_end_matches('/').to_owned();
        repository.set_path(&path);
    }
    Ok(repository)
}

fn ensure_distinct_repository_roots(
    source: &url::Url,
    snapshots_options: &SnapshotsOptions,
) -> anyhow::Result<()> {
    let Some(target) = snapshots_options.destination.as_deref() else {
        return Ok(());
    };
    let target = normalized_repository_root(target, "target snapshot repository")?;
    ensure!(
        source != &target,
        "the restored cluster's active snapshot repository must differ from the backup source repository"
    );
    Ok(())
}

fn target_fingerprint(
    preserve: bool,
    source: Option<ClusterFingerprint>,
) -> anyhow::Result<ClusterFingerprint> {
    match (preserve, source) {
        (true, Some(fingerprint)) => Ok(fingerprint),
        (true, None) => bail!(
            "preserving the cluster fingerprint requires a source fingerprint in the backup descriptor"
        ),
        (false, _) => Ok(ClusterFingerprint::generate()),
    }
}

fn exact_artifact(
    descriptor: &ClusterBackupDescriptor,
    partition_id: restate_types::identifiers::PartitionId,
) -> anyhow::Result<&PartitionBackupArtifact> {
    descriptor
        .artifacts
        .as_ref()
        .and_then(|artifacts| {
            artifacts
                .iter()
                .find(|artifact| artifact.partition_id == partition_id)
        })
        .ok_or_else(|| anyhow::anyhow!("missing exact artifact for partition {partition_id}"))
}

fn validate_record(partition: &Partition, record: &SnapshotRecord) -> anyhow::Result<()> {
    ensure!(
        record.partition_id == partition.partition_id
            && record.metadata.partition_id == partition.partition_id,
        "snapshot record does not match partition {}",
        partition.partition_id
    );
    ensure!(
        record.metadata.key_range == partition.key_range,
        "snapshot key range does not match captured topology for partition {}",
        partition.partition_id
    );
    ensure!(
        record.metadata.log_id == partition.log_id(),
        "snapshot log id does not match captured topology for partition {}",
        partition.partition_id
    );
    ensure!(
        record.metadata.min_applied_lsn == record.min_applied_lsn,
        "snapshot minimum applied LSN is inconsistent for partition {}",
        partition.partition_id
    );
    Ok(())
}

fn single_node_partition_table(partition_table: PartitionTable) -> PartitionTable {
    let mut builder = PartitionTableBuilder::from(partition_table);
    builder.set_partition_replication(PartitionReplication::Limit(
        ReplicationProperty::new_unchecked(1),
    ));
    builder.build()
}

fn bootstrap_target_logs(
    imported_partitions: &[ImportedPartition],
    provider: ProviderKind,
) -> anyhow::Result<Logs> {
    let mut builder = LogsBuilder::default();
    builder.set_configuration(LogsConfiguration::from(ProviderConfiguration::from((
        provider,
        ReplicationProperty::new_unchecked(1),
        restate_types::logs::metadata::NodeSetSize::new(1).expect("one is a valid nodeset size"),
    ))));

    for imported in imported_partitions {
        let log_id = imported.partition.log_id();
        let params = match provider {
            ProviderKind::Local | ProviderKind::InMemory => new_single_node_loglet_params(provider),
            ProviderKind::Replicated => LogletParams::from(
                ReplicatedLogletParams {
                    loglet_id: LogletId::new(log_id, SegmentIndex::OLDEST),
                    sequencer: GenerationalNodeId::INITIAL_NODE_ID,
                    replication: ReplicationProperty::new_unchecked(1),
                    nodeset: NodeSet::from_single(GenerationalNodeId::INITIAL_NODE_ID.as_plain()),
                }
                .serialize()?,
            ),
        };
        builder.add_log(
            log_id,
            Chain::with_base_lsn(resume_lsn(log_id, imported.applied_lsn)?, provider, params),
        )?;
    }
    Ok(builder.build())
}

fn resume_lsn(log_id: LogId, applied_lsn: Lsn) -> anyhow::Result<Lsn> {
    ensure!(
        applied_lsn != Lsn::MAX,
        "log {log_id} is at the maximum LSN and cannot be resumed"
    );
    Ok(applied_lsn.next())
}

#[cfg(test)]
mod tests {
    use restate_types::partition_table::PartitionTable;

    use super::*;

    #[test]
    fn single_node_partition_table_keeps_topology_but_sets_replication_to_one() {
        let table = PartitionTable::with_equally_sized_partitions(Version::MIN, 2);
        let restored = single_node_partition_table(table.clone());

        assert_eq!(restored.len(), table.len());
        assert_eq!(
            restored.replication(),
            &PartitionReplication::Limit(ReplicationProperty::new_unchecked(1))
        );
        for (partition_id, partition) in table.iter() {
            assert_eq!(restored.get(partition_id), Some(partition));
        }
    }

    #[test]
    fn target_fingerprint_is_fresh_unless_preserved() {
        let source = ClusterFingerprint::generate();

        assert_eq!(target_fingerprint(true, Some(source)).unwrap(), source);
        assert!(target_fingerprint(true, None).is_err());
        assert_ne!(target_fingerprint(false, Some(source)).unwrap(), source);
    }

    #[test]
    fn resume_lsn_rejects_overflow() {
        assert_eq!(
            resume_lsn(LogId::new(1), Lsn::new(41)).unwrap(),
            Lsn::new(42)
        );
        assert!(resume_lsn(LogId::new(1), Lsn::MAX).is_err());
    }

    #[test]
    fn target_configuration_requires_durable_single_node_roles() {
        assert!(
            validate_target_configuration(&CommonOptions::default(), ProviderKind::Replicated)
                .is_err()
        );

        let mut common = CommonOptions::default();
        common.auto_provision = false;
        assert!(validate_target_configuration(&common, ProviderKind::Replicated).is_ok());

        common.force_node_id = Some(restate_types::PlainNodeId::new(2));
        assert!(validate_target_configuration(&common, ProviderKind::Replicated).is_err());
        common.force_node_id = Some(GenerationalNodeId::INITIAL_NODE_ID.as_plain());
        assert!(validate_target_configuration(&common, ProviderKind::Replicated).is_ok());

        common.roles.remove(Role::Worker);
        assert!(validate_target_configuration(&common, ProviderKind::Replicated).is_err());
        common.roles.insert(Role::Worker);

        common.roles.remove(Role::LogServer);
        assert!(validate_target_configuration(&common, ProviderKind::Replicated).is_err());
        assert!(validate_target_configuration(&common, ProviderKind::Local).is_ok());
        assert!(validate_target_configuration(&common, ProviderKind::InMemory).is_err());

        common.roles.insert(Role::LogServer);
        common.metadata_client.kind = MetadataClientKind::Etcd {
            addresses: Vec::new(),
        };
        assert!(validate_target_configuration(&common, ProviderKind::Replicated).is_err());
    }

    #[test]
    fn active_repository_must_differ_from_restore_source() {
        let source = normalized_repository_root(
            "s3://bucket/source?region=source",
            "source snapshot repository",
        )
        .unwrap();
        let mut snapshots = SnapshotsOptions::default();

        assert!(ensure_distinct_repository_roots(&source, &snapshots).is_ok());
        snapshots.destination = Some("s3://bucket/target?region=target".to_owned());
        assert!(ensure_distinct_repository_roots(&source, &snapshots).is_ok());

        snapshots.destination = Some("s3://bucket/source?region=target".to_owned());
        assert!(ensure_distinct_repository_roots(&source, &snapshots).is_err());

        snapshots.destination = Some("s3://bucket/source/?region=target".to_owned());
        assert!(ensure_distinct_repository_roots(&source, &snapshots).is_err());

        snapshots.destination = Some("s3://bucket/source/restored".to_owned());
        assert!(ensure_distinct_repository_roots(&source, &snapshots).is_ok());
    }
}
