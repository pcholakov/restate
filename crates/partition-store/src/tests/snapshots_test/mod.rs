// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::HashMap;

use tempfile::tempdir;

use restate_storage_api::Transaction;
use restate_storage_api::fsm_table::FsmTable;
use restate_types::identifiers::{PartitionId, SnapshotId};
use restate_types::logs::Lsn;
use tokio::task::JoinSet;
use tracing::info;

use crate::{PartitionStore, PartitionStoreManager};

pub(crate) async fn run_snapshot_tests(
    manager: PartitionStoreManager,
    stores: HashMap<PartitionId, PartitionStore>,
) {
    let snapshots_dir = tempdir().unwrap().into_path();
    let mut workers = JoinSet::new();

    info!(
        "RocksDB path: {}",
        manager.rocksdb().path.as_path().display()
    );

    for (partition_id, partition_store) in stores {
        let manager = manager.clone();
        let mut partition_store = partition_store.clone();
        let snapshot_base_path = snapshots_dir.clone();

        workers.spawn(async move {
            for idx in 1..10_000 {
                update_applied_lsn(&mut partition_store, Lsn::new(idx)).await;
                manager
                    .export_partition_snapshot(
                        partition_id,
                        None,
                        SnapshotId::new(),
                        snapshot_base_path.as_path(),
                    )
                    .await
                    .unwrap();
            }
        });
    }

    workers.join_all().await;
}

async fn update_applied_lsn(partition_store: &mut PartitionStore, lsn: Lsn) {
    let mut txn = partition_store.transaction();
    txn.put_applied_lsn(lsn).await.unwrap();
    txn.commit().await.expect("commit succeeds");
}
