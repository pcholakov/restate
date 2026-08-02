// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use restate_storage_api::Transaction;
use restate_storage_api::fsm_table::{ReadFsmTable, WriteFsmTable};
use restate_types::schema::Schema;

use super::storage_test_environment;

#[restate_core::test]
async fn deleting_cached_schema_is_durable() -> googletest::Result<()> {
    let mut store = storage_test_environment().await;

    let mut transaction = store.transaction();
    transaction.put_schema(&Schema::default())?;
    transaction.commit().await?;
    drop(transaction);
    assert!(store.get_schema().await?.is_some());

    let mut transaction = store.transaction();
    transaction.delete_schema()?;
    transaction.commit().await?;
    drop(transaction);
    assert!(store.get_schema().await?.is_none());

    Ok(())
}
