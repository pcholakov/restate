// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::path::{Path, PathBuf};

use cling::prelude::*;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use restate_cli_util::{c_println, c_warn};
use restate_clock::WallClock;
use restate_core::protobuf::cluster_ctrl_svc::{
    CreatePartitionSnapshotRequest, new_cluster_ctrl_client,
};
use restate_types::cluster_backup::{
    BackupConsistency, ClusterBackupDescriptor, PartitionBackupArtifact, PartitionBackupFailure,
};
use restate_types::identifiers::PartitionId;
use restate_types::nodes_config::Role;

use crate::connection::ConnectionInfo;

#[derive(Run, Parser, Collect, Clone, Debug)]
#[cling(run = "backup")]
pub struct BackupOpts {
    /// Path at which to write the backup descriptor JSON.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    output: PathBuf,

    /// Snapshot repository configured on the source workers. Query parameters are not recorded.
    #[arg(long, value_hint = clap::ValueHint::Url)]
    snapshot_repository: String,

    /// Include the whole global Schema metadata in the descriptor.
    #[arg(long)]
    include_schema: bool,

    /// Maximum number of partition snapshots to create concurrently.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Record topology and optional schema only; restore resolves each partition's latest snapshot.
    #[arg(long)]
    use_latest: bool,

    /// Replace an existing descriptor at the output path.
    #[arg(long)]
    overwrite: bool,
}

async fn backup(connection: &ConnectionInfo, opts: &BackupOpts) -> anyhow::Result<()> {
    if opts.concurrency == 0 {
        anyhow::bail!("--concurrency must be greater than zero");
    }
    if !opts.overwrite && tokio::fs::try_exists(&opts.output).await? {
        anyhow::bail!(
            "output descriptor '{}' already exists; pass --overwrite to replace it",
            opts.output.display()
        );
    }

    c_warn!(
        "This creates a best-effort backup. Partition snapshots are not a consistent cluster-wide cut."
    );

    let source_snapshot_repository = normalize_snapshot_repository(&opts.snapshot_repository)?;
    let nodes_config = connection.get_nodes_configuration().await?;
    let partition_table = connection.get_partition_table().await?;
    let schema = if opts.include_schema {
        Some(connection.get_schema().await?)
    } else {
        None
    };

    let mut descriptor = ClusterBackupDescriptor {
        version: ClusterBackupDescriptor::VERSION,
        created_at: WallClock::recent_ms(),
        consistency: BackupConsistency::BestEffort,
        source_snapshot_repository: source_snapshot_repository.clone(),
        source_cluster_name: nodes_config.cluster_name().to_owned(),
        source_cluster_fingerprint: nodes_config.cluster_fingerprint(),
        source_cluster_features: nodes_config.features(),
        partition_table: partition_table.clone(),
        schema,
        artifacts: None,
        failures: vec![],
    };

    if !opts.use_latest {
        c_warn!(
            "Exact backup artifacts are protected from automatic snapshot retention and accumulate until repository metadata and objects are cleaned up together."
        );
        let partitions = partition_table
            .iter()
            .map(|(partition_id, partition)| (*partition_id, partition.key_range))
            .collect::<Vec<_>>();
        let mut artifacts = Vec::with_capacity(partitions.len());
        let mut failures = vec![];
        let mut snapshots = futures::stream::iter(partitions)
            .map(|(partition_id, key_range)| {
                create_snapshot(
                    connection.clone(),
                    partition_id,
                    key_range,
                    source_snapshot_repository.clone(),
                )
            })
            .buffer_unordered(opts.concurrency);

        while let Some(result) = snapshots.next().await {
            match result {
                Ok(artifact) => artifacts.push(artifact),
                Err((partition_id, error)) => failures.push(PartitionBackupFailure {
                    partition_id,
                    message: error.to_string(),
                }),
            }
        }
        artifacts.sort_by_key(|artifact| artifact.partition_id);
        failures.sort_by_key(|failure| failure.partition_id);
        descriptor.artifacts = Some(artifacts);
        descriptor.failures = failures;
    }

    write_descriptor(&opts.output, &descriptor).await?;
    c_println!("{}", opts.output.display());

    match descriptor.validate()? {
        restate_types::cluster_backup::BackupArtifactSet::TopologyOnly if opts.use_latest => Ok(()),
        restate_types::cluster_backup::BackupArtifactSet::Complete => Ok(()),
        restate_types::cluster_backup::BackupArtifactSet::TopologyOnly => {
            unreachable!("snapshot capture always records an artifact set")
        }
        restate_types::cluster_backup::BackupArtifactSet::Incomplete => {
            c_warn!(
                "Backup descriptor was written but is incomplete; inspect {} before retrying.",
                opts.output.display()
            );
            anyhow::bail!("failed to create snapshots for one or more partitions")
        }
    }
}

async fn create_snapshot(
    connection: ConnectionInfo,
    partition_id: PartitionId,
    key_range: restate_types::sharding::KeyRange,
    expected_repository: String,
) -> Result<PartitionBackupArtifact, (PartitionId, anyhow::Error)> {
    let response = connection
        .try_each(Some(Role::Admin), |channel| async {
            new_cluster_ctrl_client(channel, &restate_cli_util::CliContext::get().network)
                .create_partition_snapshot(CreatePartitionSnapshotRequest {
                    partition_id: partition_id.into(),
                    min_target_lsn: None,
                    trim_log: false,
                    protect_from_retention: true,
                })
                .await
        })
        .await
        .map_err(|error| (partition_id, error.into()))?
        .into_inner();

    validate_worker_repository(&response.snapshot_repository, &expected_repository)
        .map_err(|error| (partition_id, error))?;

    Ok(PartitionBackupArtifact {
        partition_id,
        snapshot_id: response
            .snapshot_id
            .parse()
            .map_err(|error| (partition_id, anyhow::Error::from(error)))?,
        log_id: response.log_id.into(),
        min_applied_lsn: response.min_applied_lsn.into(),
        key_range,
    })
}

fn normalize_snapshot_repository(value: &str) -> anyhow::Result<String> {
    let mut repository = url::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid --snapshot-repository URL: {error}"))?;
    repository.set_query(None);
    if repository.path() != "/" {
        let path = repository.path().trim_end_matches('/').to_owned();
        repository.set_path(&path);
    }
    Ok(repository.to_string())
}

fn validate_worker_repository(reported: &str, expected: &str) -> anyhow::Result<()> {
    let reported = normalize_snapshot_repository(reported).map_err(|error| {
        anyhow::anyhow!(
            "worker did not report a valid snapshot repository; it may not support protected backups: {error}"
        )
    })?;
    if reported != expected {
        anyhow::bail!(
            "worker wrote snapshot to '{reported}', but --snapshot-repository resolves to '{expected}'"
        );
    }
    Ok(())
}

async fn write_descriptor(path: &Path, descriptor: &ClusterBackupDescriptor) -> anyhow::Result<()> {
    let content = serde_json::to_vec_pretty(descriptor)?;
    let temporary_path = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)
        .await?;
    file.write_all(&content).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary_path, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{normalize_snapshot_repository, validate_worker_repository};

    #[test]
    fn normalizes_snapshot_repository_like_the_server() {
        assert_eq!(
            normalize_snapshot_repository("s3://bucket/backups/?region=ignored").unwrap(),
            "s3://bucket/backups"
        );
        assert_eq!(
            normalize_snapshot_repository("file:///").unwrap(),
            "file:///"
        );
        assert!(normalize_snapshot_repository("not a URL").is_err());
    }

    #[test]
    fn rejects_old_or_mismatched_worker_repository() {
        assert!(validate_worker_repository("", "s3://bucket/backups").is_err());
        assert!(validate_worker_repository("s3://bucket/other", "s3://bucket/backups").is_err());
        validate_worker_repository("s3://bucket/backups/?ignored=true", "s3://bucket/backups")
            .unwrap();
    }
}
