// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::time::Duration;

use cling::prelude::*;

use restate_cli_util::{CliContext, c_println};
use restate_core::protobuf::cluster_ctrl_svc::{
    CreateDistributedSnapshotRequest, GetClusterSnapshotStatusRequest, new_cluster_ctrl_client,
};
use restate_types::nodes_config::Role;

use crate::connection::ConnectionInfo;

#[derive(Run, Parser, Collect, Clone, Debug)]
#[clap(visible_alias = "create-cluster")]
#[cling(run = "create_cluster_snapshot")]
pub struct CreateClusterSnapshotOpts {
    /// Wait for the cluster snapshot to complete before returning
    #[arg(long, default_value = "false")]
    wait: bool,

    /// Poll interval in seconds when --wait is used
    #[arg(long, default_value = "2")]
    poll_interval: u64,
}

async fn create_cluster_snapshot(
    connection: &ConnectionInfo,
    opts: &CreateClusterSnapshotOpts,
) -> anyhow::Result<()> {
    let response = connection
        .try_each(Some(Role::Admin), |channel| async {
            new_cluster_ctrl_client(channel, &CliContext::get().network)
                .create_distributed_snapshot(CreateDistributedSnapshotRequest {})
                .await
        })
        .await?
        .into_inner();

    let snapshot_id = response.snapshot_id;
    c_println!("Cluster snapshot initiated: {snapshot_id}");

    if !opts.wait {
        c_println!(
            "Use `restatectl snapshots create-cluster-snapshot --wait` or \
             poll status to track completion."
        );
        return Ok(());
    }

    let poll_interval = Duration::from_secs(opts.poll_interval);
    loop {
        tokio::time::sleep(poll_interval).await;

        let status = connection
            .try_each(Some(Role::Admin), |channel| async {
                new_cluster_ctrl_client(channel, &CliContext::get().network)
                    .get_cluster_snapshot_status(GetClusterSnapshotStatusRequest { snapshot_id })
                    .await
            })
            .await?
            .into_inner();

        c_println!(
            "  {}/{} partitions complete",
            status.completed_partitions,
            status.total_partitions,
        );

        if status.is_complete {
            c_println!("Cluster snapshot {snapshot_id} complete!");
            return Ok(());
        }
    }
}
