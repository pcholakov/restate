// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, bail};
use tokio::sync::oneshot::Sender;
use tokio::sync::Mutex;
use tracing::{info, warn};

use restate_core::TaskKind;
use restate_partition_store::snapshots::{PartitionSnapshotMetadata, SnapshotFormatVersion};
use restate_partition_store::PartitionStore;
use restate_types::config::Configuration;
use restate_types::identifiers::{PartitionId, SnapshotId};
use restate_types::live::Live;

/// Exports asynchronous snapshots of partition store state. Runs as a background task and takes overall responsibility
/// for enacting the configured snapshot policy, including remote uploads, metadata updates, and cleanup.
pub struct SnapshotProducer {
    pub partition_id: PartitionId,
    pub config: Live<Configuration>,
    snapshot_in_progress: Arc<Mutex<()>>,
}

impl SnapshotProducer {
    pub fn new(partition_id: PartitionId, config: Live<Configuration>) -> Self {
        Self {
            partition_id,
            config,
            snapshot_in_progress: Arc::new(Mutex::new(())),
        }
    }

    /// Creates a snapshot in a background task, optionally sending a completion notification.
    pub(crate) fn create_snapshot(
        &mut self,
        partition_store: &mut PartitionStore,
        response_tx: Option<Sender<anyhow::Result<SnapshotId>>>,
    ) -> anyhow::Result<()> {
        let partition_id = self.partition_id;

        let ongoing_snapshot = self
            .snapshot_in_progress
            .clone()
            .try_lock_owned()
            .expect("Snapshot already in progress");

        let partition_store = partition_store.clone();
        let base_dir = self
            .config
            .live_load()
            .worker
            .snapshots
            .snapshots_base_dir()
            .to_path_buf();
        let config = self.config.live_load();
        let cluster_name = config.common.cluster_name().to_string();
        let node_name = config.common.node_name().to_string();

        restate_core::task_center().spawn_child(
            TaskKind::PartitionSnapshotProducer,
            "create-snapshot",
            Some(self.partition_id),
            async move {
                let _guard = ongoing_snapshot;
                let result = Self::write_snapshot(
                    cluster_name,
                    node_name,
                    partition_store,
                    partition_id,
                    base_dir,
                )
                .await;

                if let Some(sender) = response_tx {
                    sender
                        .send(result.map(|metadata| metadata.snapshot_id))
                        .ok();
                }
                Ok(())
            },
        )?;

        Ok(())
    }

    async fn write_snapshot(
        cluster_name: String,
        node_name: String,
        mut partition_store: PartitionStore,
        partition_id: PartitionId,
        snapshot_base_path: PathBuf,
    ) -> anyhow::Result<PartitionSnapshotMetadata> {
        let partition_snapshots_path = snapshot_base_path.join(partition_id.to_string());
        if let Err(e) = std::fs::create_dir_all(&partition_snapshots_path) {
            warn!(
                %partition_id,
                path = ?partition_snapshots_path,
                error = ?e,
                "Failed to create partition snapshot directory"
            );
            bail!(anyhow!(
                "Failed to create partition snapshot directory: {:?}",
                e
            ));
        }

        let snapshot_id = SnapshotId::new();
        let snapshot_path = partition_snapshots_path.join(snapshot_id.to_string());
        let snapshot = partition_store
            .create_snapshot(snapshot_path.clone())
            .await?;

        let snapshot_meta = PartitionSnapshotMetadata {
            version: SnapshotFormatVersion::V1,
            cluster_name,
            partition_id,
            node_name,
            created_at: humantime::Timestamp::from(SystemTime::now()),
            snapshot_id,
            key_range: partition_store.partition_key_range().clone(),
            min_applied_lsn: snapshot.min_applied_lsn,
            db_comparator_name: snapshot.db_comparator_name.clone(),
            files: snapshot.files.clone(),
        };
        let metadata_json = serde_json::to_string_pretty(&snapshot_meta)?;

        let metadata_path = snapshot_path.join("metadata.json");
        std::fs::write(metadata_path.clone(), metadata_json)?;
        info!(
            %partition_id,
            lsn = %snapshot.min_applied_lsn,
            metadata = ?metadata_path,
            "Partition snapshot written"
        );

        Ok(snapshot_meta)
    }
}
