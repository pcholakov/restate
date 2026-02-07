// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::net::SocketAddr;
use std::time::Duration;

use enumset::EnumSet;
use googletest::IntoTestResult;
use http::header::CONTENT_TYPE;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing::info;
use url::Url;

use restate_core::network::net_util::{DNSResolution, create_tonic_channel};
use restate_core::protobuf::cluster_ctrl_svc::{
    ClusterStateRequest, CreateDistributedSnapshotRequest, GetClusterSnapshotStatusRequest,
    cluster_ctrl_svc_client::ClusterCtrlSvcClient, new_cluster_ctrl_client,
};
use restate_local_cluster_runner::cluster::Cluster;
use restate_local_cluster_runner::node::{BinarySource, NodeSpec};
use restate_types::config::{Configuration, LogFormat, NetworkingOptions};
use restate_types::logs::metadata::{
    NodeSetSize, ProviderConfiguration, ProviderKind, ReplicatedLogletConfig,
};
use restate_types::net::address::PeerNetAddress;
use restate_types::protobuf::cluster::RunMode;
use restate_types::protobuf::cluster::node_state::State;
use restate_types::replication::ReplicationProperty;

mod common;

/// End-to-end test: create a cluster, populate state, take a coordinated
/// snapshot, shut down, re-provision from the snapshot, and verify state.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn cluster_snapshot_restore() -> googletest::Result<()> {
    let num_partitions = 4u16;
    let replication = ReplicationProperty::new_unchecked(1);
    let replicated_loglet_config = ReplicatedLogletConfig {
        target_nodeset_size: NodeSetSize::default(),
        replication_property: replication.clone(),
    };
    let provider = ProviderConfiguration::Replicated(replicated_loglet_config);

    // Shared snapshot repository — both original and restored clusters use it.
    let snapshots_dir = TempDir::new()?;
    let snapshots_url = Url::from_file_path(snapshots_dir.path())
        .unwrap()
        .to_string();

    let mut base_config = Configuration::new_unix_sockets();
    base_config.common.default_num_partitions = num_partitions;
    base_config.bifrost.default_provider = ProviderKind::Replicated;
    base_config.common.log_filter = "restate=debug,warn".to_owned();
    base_config.common.log_format = LogFormat::Compact;
    base_config.common.log_disable_ansi_codes = true;
    base_config.worker.snapshots.destination = Some(snapshots_url.clone());

    // --- Phase 1: Create original cluster and populate state ---
    info!("Phase 1: Creating original cluster");
    let nodes = NodeSpec::new_test_nodes(
        base_config.clone(),
        BinarySource::CargoTest,
        EnumSet::all(),
        1,
        false,
    );

    let mut cluster = Cluster::builder()
        .cluster_name("snapshot-restore")
        .nodes(nodes)
        .temp_base_dir("cluster_snapshot_restore_original")
        .build()
        .start()
        .await?;

    cluster.nodes[0]
        .provision_cluster(None, replication.clone(), Some(provider.clone()))
        .await
        .into_test_result()?;

    info!("Waiting for cluster to be healthy");
    cluster.wait_healthy(Duration::from_secs(60)).await?;

    let mut ctrl_client = new_cluster_ctrl_client(
        create_tonic_channel(
            cluster.nodes[0].advertised_address().clone(),
            &NetworkingOptions::default(),
            DNSResolution::Gai,
        ),
        &base_config.networking,
    );

    wait_for_partition_leader(&mut ctrl_client).await?;

    // Deploy a mock service and invoke it to create state
    let mock_svc_port = start_mock_service().await?;

    let admin_uds = cluster.nodes[0]
        .admin_address()
        .clone()
        .unwrap()
        .into_address()
        .unwrap();
    let PeerNetAddress::Uds(admin_uds) = admin_uds else {
        panic!("admin address must be a unix domain socket");
    };
    let admin_client = reqwest::Client::builder().unix_socket(admin_uds).build()?;

    info!("Deploying mock service");
    let reg_response = admin_client
        .post("http://localhost/deployments")
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "uri": format!("http://127.0.0.1:{mock_svc_port}") }))
        .send()
        .await?;
    assert!(
        reg_response.status().is_success(),
        "Service deployment should succeed: {}",
        reg_response.status()
    );

    let ingress_uds = cluster.nodes[0]
        .ingress_address()
        .clone()
        .unwrap()
        .into_address()
        .unwrap();
    let PeerNetAddress::Uds(ingress_uds) = ingress_uds else {
        panic!("ingress address must be a unix domain socket");
    };
    let ingress_client = reqwest::Client::builder()
        .unix_socket(ingress_uds)
        .build()?;

    // Send invocations to populate state across multiple partitions.
    // Use different keys to spread across partition key range.
    info!("Sending invocations to populate state");
    for key in &["alpha", "beta", "gamma", "delta"] {
        let response = retry_until_success(&ingress_client, &format!("/Counter/{key}/get")).await?;
        assert!(
            response.status().is_success(),
            "Invocation for {key} should succeed"
        );
    }

    // --- Phase 2: Take a coordinated cluster snapshot ---
    info!("Phase 2: Taking coordinated cluster snapshot");
    let snapshot_response = ctrl_client
        .create_distributed_snapshot(CreateDistributedSnapshotRequest {})
        .await?
        .into_inner();
    let snapshot_id = snapshot_response.snapshot_id;
    info!(%snapshot_id, "Distributed snapshot initiated");
    assert!(snapshot_id > 0, "Snapshot ID should be positive");

    // Poll until the snapshot is complete. This also triggers the manifest
    // write to the snapshot repository (needed for provisioning).
    let manifest = poll_snapshot_complete(&mut ctrl_client, snapshot_id).await?;
    info!(
        snapshot_id = %manifest.snapshot_id,
        partitions = manifest.partitions.len(),
        "Cluster snapshot complete"
    );
    assert_eq!(
        manifest.partitions.len() as u32,
        manifest.num_partitions,
        "All partitions should be present in manifest"
    );

    // --- Phase 3: Shut down the original cluster ---
    info!("Phase 3: Shutting down original cluster");
    cluster.graceful_shutdown(Duration::from_secs(10)).await?;
    drop(cluster);

    // --- Phase 4: Provision a new cluster from the snapshot ---
    info!("Phase 4: Provisioning new cluster from snapshot");

    // Create fresh nodes with a different temp dir (simulating a new cluster)
    let nodes = NodeSpec::new_test_nodes(
        base_config.clone(),
        BinarySource::CargoTest,
        EnumSet::all(),
        1,
        false,
    );

    let mut restored_cluster = Cluster::builder()
        .cluster_name("snapshot-restore")
        .nodes(nodes)
        .temp_base_dir("cluster_snapshot_restore_new")
        .build()
        .start()
        .await?;

    restored_cluster.nodes[0]
        .provision_cluster_from_snapshot(replication.clone(), Some(provider.clone()), snapshot_id)
        .await
        .into_test_result()?;

    info!("Waiting for restored cluster to be healthy");
    restored_cluster
        .wait_healthy(Duration::from_secs(60))
        .await?;

    let mut restored_ctrl_client = new_cluster_ctrl_client(
        create_tonic_channel(
            restored_cluster.nodes[0].advertised_address().clone(),
            &NetworkingOptions::default(),
            DNSResolution::Gai,
        ),
        &base_config.networking,
    );

    wait_for_partition_leader(&mut restored_ctrl_client).await?;

    // --- Phase 5: Verify state is present in the restored cluster ---
    info!("Phase 5: Verifying state in restored cluster");

    // Deploy the mock service again (schema is not part of the snapshot)
    let restored_admin_uds = restored_cluster.nodes[0]
        .admin_address()
        .clone()
        .unwrap()
        .into_address()
        .unwrap();
    let PeerNetAddress::Uds(restored_admin_uds) = restored_admin_uds else {
        panic!("admin address must be a unix domain socket");
    };
    let restored_admin_client = reqwest::Client::builder()
        .unix_socket(restored_admin_uds)
        .build()?;

    let reg_response = restored_admin_client
        .post("http://localhost/deployments")
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "uri": format!("http://127.0.0.1:{mock_svc_port}") }))
        .send()
        .await?;
    assert!(
        reg_response.status().is_success(),
        "Service re-deployment should succeed: {}",
        reg_response.status()
    );

    let restored_ingress_uds = restored_cluster.nodes[0]
        .ingress_address()
        .clone()
        .unwrap()
        .into_address()
        .unwrap();
    let PeerNetAddress::Uds(restored_ingress_uds) = restored_ingress_uds else {
        panic!("ingress address must be a unix domain socket");
    };
    let restored_ingress_client = reqwest::Client::builder()
        .unix_socket(restored_ingress_uds)
        .build()?;

    // Verify invocations work on the restored cluster
    for key in &["alpha", "beta", "gamma", "delta"] {
        let response =
            retry_until_success(&restored_ingress_client, &format!("/Counter/{key}/get")).await?;
        assert!(
            response.status().is_success(),
            "Invocation for {key} on restored cluster should succeed"
        );
    }

    info!("Cluster snapshot restore test passed");
    restored_cluster
        .graceful_shutdown(Duration::from_secs(10))
        .await?;

    Ok(())
}

async fn wait_for_partition_leader(
    client: &mut ClusterCtrlSvcClient<tonic::transport::Channel>,
) -> googletest::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let cluster_state = client
            .get_cluster_state(ClusterStateRequest {})
            .await?
            .into_inner()
            .cluster_state
            .unwrap();

        if cluster_state.nodes.values().any(|n| {
            n.state.as_ref().is_some_and(|s| match s {
                State::Alive(s) => s.partitions.values().any(|p| {
                    RunMode::try_from(p.effective_mode).is_ok_and(|m| m == RunMode::Leader)
                }),
                _ => false,
            })
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timed out waiting for a partition leader"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

async fn poll_snapshot_complete(
    client: &mut ClusterCtrlSvcClient<tonic::transport::Channel>,
    snapshot_id: u64,
) -> googletest::Result<restate_types::cluster_snapshot::ClusterSnapshotManifest> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let response = client
            .get_cluster_snapshot_status(GetClusterSnapshotStatusRequest { snapshot_id })
            .await?
            .into_inner();

        if response.is_complete {
            let manifest = serde_json::from_slice(&response.manifest_json)?;
            return Ok(manifest);
        }

        info!(
            completed = response.completed_partitions,
            total = response.total_partitions,
            "Snapshot in progress..."
        );

        assert!(
            tokio::time::Instant::now() < deadline,
            "Timed out waiting for cluster snapshot to complete ({}/{} partitions)",
            response.completed_partitions,
            response.total_partitions,
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn retry_until_success(
    client: &reqwest::Client,
    path: &str,
) -> googletest::Result<reqwest::Response> {
    let url = format!("http://localhost{path}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match client.post(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                info!(status = %response.status(), %url, "Retrying...");
            }
            Err(err) => {
                info!(%err, %url, "Retrying...");
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timed out waiting for successful response from {url}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn start_mock_service() -> googletest::Result<u16> {
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();

    let (running_tx, running_rx) = oneshot::channel();

    tokio::spawn(async move {
        if let Err(e) = mock_service_endpoint::listener::run_listener(listener, || {
            let _ = running_tx.send(());
        })
        .await
        {
            panic!("Error running mock service: {e:?}");
        }
    });

    running_rx.await?;
    Ok(port)
}
