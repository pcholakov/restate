// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use crate::mock_random_service_invocation;
use restate_partition_store::PartitionStore;
use restate_storage_api::outbox_table::{OutboxMessage, OutboxTable};
use restate_storage_api::Transaction;
use restate_types::identifiers::PartitionId;

fn mock_outbox_message() -> OutboxMessage {
    OutboxMessage::ServiceInvocation(mock_random_service_invocation())
}

pub(crate) async fn populate_data<T: OutboxTable>(txn: &mut T) {
    let partition1337 = PartitionId::from(1337);
    txn.add_message(partition1337, 0, mock_outbox_message())
        .await;
    txn.add_message(partition1337, 1, mock_outbox_message())
        .await;
    txn.add_message(partition1337, 2, mock_outbox_message())
        .await;
    txn.add_message(partition1337, 3, mock_outbox_message())
        .await;

    // add a successor and a predecessor partitions
    txn.add_message(PartitionId::from(1336), 0, mock_outbox_message())
        .await;
    txn.add_message(PartitionId::from(1338), 0, mock_outbox_message())
        .await;
}

pub(crate) async fn consume_message_and_truncate<T: OutboxTable>(txn: &mut T) {
    let partition1337 = PartitionId::from(1337);
    let mut sequence = 0;
    while let Ok(Some((seq, _))) = txn.get_next_outbox_message(partition1337, sequence).await {
        sequence = seq + 1;
    }
    assert_eq!(sequence, 4);

    txn.truncate_outbox(partition1337, 0..=sequence).await;
}

pub(crate) async fn verify_outbox_is_empty_after_truncation<T: OutboxTable>(txn: &mut T) {
    let partition1337 = PartitionId::from(1337);
    let result = txn
        .get_next_outbox_message(partition1337, 0)
        .await
        .expect("should not fail");

    assert_eq!(result, None);
}

pub(crate) async fn verify_other_partitions_are_unchanged_after_truncation<T: OutboxTable>(
    txn: &mut T,
) {
    let partition1336 = PartitionId::from(1336);
    let predecessor_partition = txn
        .get_next_outbox_message(partition1336, 0)
        .await
        .expect("should not fail");

    let partition1338 = PartitionId::from(1338);
    let successor_partition = txn
        .get_next_outbox_message(partition1338, 0)
        .await
        .expect("should not fail");

    assert!(successor_partition.is_some());
    assert!(predecessor_partition.is_some());
}

pub(crate) async fn run_tests(mut rocksdb: PartitionStore) {
    let mut txn = rocksdb.transaction();

    populate_data(&mut txn).await;
    consume_message_and_truncate(&mut txn).await;

    txn.commit().await.expect("should not fail");

    let mut txn = rocksdb.transaction();
    verify_outbox_is_empty_after_truncation(&mut txn).await;
    verify_other_partitions_are_unchanged_after_truncation(&mut txn).await;
}
