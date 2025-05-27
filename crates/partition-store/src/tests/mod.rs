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
use std::fmt::Debug;
use std::num::{NonZeroU16, NonZeroUsize};
use std::pin::pin;

use futures::Stream;
use restate_types::Version;
use restate_types::partitions::PartitionTable;
use tokio_stream::StreamExt;

use crate::{OpenMode, PartitionStore, PartitionStoreManager};
use restate_rocksdb::RocksDbManager;
use restate_storage_api::StorageError;
use restate_types::config::{CommonOptions, RocksDbOptionsBuilder, StorageOptionsBuilder};
use restate_types::identifiers::{
    InvocationId, PartitionId, PartitionProcessorRpcRequestId, ServiceId,
};
use restate_types::invocation::{InvocationTarget, ServiceInvocation, Source};
use restate_types::live::Constant;
use restate_types::state_mut::ExternalStateMutation;

#[allow(unused)]
mod idempotency_table_test;
#[allow(unused)]
mod inbox_table_test;
#[allow(unused)]
mod invocation_status_table_test;
#[allow(unused)]
mod journal_table_test;
#[allow(unused)]
mod journal_table_v2_test;
#[allow(unused)]
mod outbox_table_test;
#[allow(unused)]
mod promise_table_test;
mod snapshots_test;
#[allow(unused)]
mod state_table_test;
#[allow(unused)]
mod timer_table_test;
#[allow(unused)]
mod virtual_object_status_table_test;

mod persisted_lsn_tracking_test;

async fn storage_test_environment() -> PartitionStore {
    storage_test_environment_with_manager()
        .await
        .2
        .get(&PartitionId::MIN)
        .expect("at least one store available")
        .clone()
}

async fn storage_test_environment_with_manager()
-> (&'static RocksDbManager, PartitionStoreManager, HashMap<PartitionId, PartitionStore>) {
    //
    // create a rocksdb storage from options
    //
    let num_stores = 128;

    let common_opts = CommonOptions::default();

    let rocksdb = RocksDbManager::init(Constant::new(common_opts));
    let storage_options = StorageOptionsBuilder::default()
        .rocksdb_memory_budget(Some(NonZeroUsize::new(4 << 30).unwrap()))
        .num_partitions_to_share_memory_budget(Some(NonZeroU16::new(num_stores).unwrap()))
        .rocksdb(
            RocksDbOptionsBuilder::default()
                .rocksdb_log_level(Some(restate_types::config::RocksDbLogLevel::Debug))
                .rocksdb_disable_wal(Some(true))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    let manager = PartitionStoreManager::create(Constant::new(storage_options.clone()), &[])
        .await
        .expect("DB storage creation succeeds");

    let mut stores: HashMap<PartitionId, PartitionStore> = Default::default();

    let partition_table = PartitionTable::with_equally_sized_partitions(Version::MIN, num_stores);

    for (partition_id, partition) in partition_table.iter() {
        let store = manager
            .open_partition_store(
                *partition_id,
                partition.key_range.clone(),
                OpenMode::CreateIfMissing,
                &storage_options.rocksdb,
            )
            .await
            .expect("DB storage creation succeeds");
        stores.insert(*partition_id, store);
    }

    (rocksdb, manager, stores)
}

#[test_log::test(restate_core::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_read_write() {
    let (_rocksdb, manager, stores) = storage_test_environment_with_manager().await;

    //
    // run the tests
    //
    // inbox_table_test::run_tests(store.clone()).await;
    // outbox_table_test::run_tests(store.clone()).await;
    // state_table_test::run_tests(store.clone()).await;
    // virtual_object_status_table_test::run_tests(store.clone()).await;
    // timer_table_test::run_tests(store.clone()).await;

    let res = snapshots_test::run_snapshot_tests(manager.clone(), stores).await;
    // rocksdb.shutdown().await; // give rocksdb a chance to flush log output
    res.expect("no errors");
}

pub(crate) fn mock_service_invocation(service_id: ServiceId) -> ServiceInvocation {
    let invocation_target = InvocationTarget::mock_from_service_id(service_id);
    ServiceInvocation {
        invocation_id: InvocationId::mock_generate(&invocation_target),
        invocation_target,
        argument: Default::default(),
        source: Source::Ingress(PartitionProcessorRpcRequestId::new()),
        response_sink: None,
        span_context: Default::default(),
        headers: vec![],
        execution_time: None,
        completion_retention_duration: None,
        idempotency_key: None,
        submit_notification_sink: None,
    }
}

pub(crate) fn mock_random_service_invocation() -> ServiceInvocation {
    mock_service_invocation(ServiceId::mock_random())
}

pub(crate) fn mock_state_mutation(service_id: ServiceId) -> ExternalStateMutation {
    ExternalStateMutation {
        service_id,
        version: None,
        state: HashMap::default(),
    }
}

pub(crate) async fn assert_stream_eq<T: Send + Debug + PartialEq + 'static>(
    actual: impl Stream<Item = Result<T, StorageError>>,
    expected: Vec<T>,
) {
    let mut actual = pin!(actual);
    let mut items = expected.into_iter();

    while let Some(item) = actual.next().await {
        let got = item.expect("Fail result");
        let expected = items.next();

        assert_eq!(Some(got), expected);
    }

    assert_eq!(None, items.next());
}
