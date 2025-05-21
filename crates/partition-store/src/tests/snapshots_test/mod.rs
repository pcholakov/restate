// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use rocksdb::ExportImportFilesMetaData;
use tempfile::tempdir;

use restate_storage_api::Transaction;
use restate_storage_api::fsm_table::FsmTable;
use restate_types::identifiers::SnapshotId;
use restate_types::logs::Lsn;

use crate::{PartitionStore, PartitionStoreManager};

pub(crate) async fn run_tests(manager: PartitionStoreManager, mut partition_store: PartitionStore) {
    for i in 1..100_000u64 {
        eprintln!("LSN {}", i);
        create_snapshot(&manager, &mut partition_store, Lsn::new(i)).await;
    }
}

async fn create_snapshot(
    _manager: &PartitionStoreManager,
    mut partition_store: &mut PartitionStore,
    lsn: Lsn,
) {
    insert_test_data(&mut partition_store, lsn).await;

    let snapshots_dir = tempdir().unwrap();

    // let partition_id = partition_store.partition_id();

    let snapshot = partition_store
        .create_snapshot(snapshots_dir.path(), None, SnapshotId::new())
        .await
        .unwrap();

    // let key_range = partition_store.partition_key_range().clone();
    // let snapshot_meta = PartitionSnapshotMetadata {
    //     version: SnapshotFormatVersion::V1,
    //     cluster_name: "cluster_name".to_string(),
    //     partition_id,
    //     node_name: "node".to_string(),
    //     created_at: humantime::Timestamp::from(SystemTime::from(MillisSinceEpoch::new(0))),
    //     snapshot_id: SnapshotId::new(),
    //     key_range: key_range.clone(),
    //     log_id: Some(LogId::from(partition_id)),
    //     min_applied_lsn: snapshot.min_applied_lsn,
    //     db_comparator_name: snapshot.db_comparator_name.clone(),
    //     files: snapshot.files.clone(),
    // };

    // We're not re-importing the snapshots now! PartitionStore::create_snapshot above checks the output
    // manager.drop_partition(partition_id).await;

    let mut import_metadata = ExportImportFilesMetaData::default();
    import_metadata.set_db_comparator_name(snapshot.db_comparator_name.as_str());
    import_metadata.set_files(&snapshot.files);

    // verify_restored_data(&mut new_partition_store, lsn).await;
}

async fn insert_test_data(partition_store: &mut PartitionStore, lsn: Lsn) {
    let mut txn = partition_store.transaction();
    txn.put_applied_lsn(lsn).await.unwrap();
    txn.commit().await.expect("commit succeeds");
}

// async fn verify_restored_data(partition_store: &mut PartitionStore, lsn: Lsn) {
//     assert_eq!(
//         lsn,
//         partition_store.get_applied_lsn().await.unwrap().unwrap()
//     );
// }
