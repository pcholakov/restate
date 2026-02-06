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
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing::info;

use restate_core::network::net_util::{DNSResolution, create_tonic_channel};
use restate_core::protobuf::cluster_ctrl_svc::{
    ClusterStateRequest, CreateDistributedSnapshotRequest,
    cluster_ctrl_svc_client::ClusterCtrlSvcClient, new_cluster_ctrl_client,
};
use restate_local_cluster_runner::{
    cluster::Cluster,
    node::{BinarySource, NodeSpec},
};
use restate_types::config::{Configuration, LogFormat, NetworkingOptions};
use restate_types::logs::metadata::{
    NodeSetSize, ProviderConfiguration, ProviderKind, ReplicatedLogletConfig,
};
use restate_types::net::address::PeerNetAddress;
use restate_types::protobuf::cluster::RunMode;
use restate_types::protobuf::cluster::node_state::State;
use restate_types::replication::ReplicationProperty;
use restate_types::retries::RetryPolicy;

mod common;

/// Validates that the distributed snapshot protocol runs in a multi-partition
/// cluster without causing crashes or disruption. This is a smoke test that
/// exercises the full InitiateSnapshot → marker exchange → completion path
/// in the real partition processor, complementing the DST invariant checks.
#[test_log::test(tokio::test)]
async fn distributed_snapshot_multi_partition() -> googletest::Result<()> {
    let num_partitions = 4u16;
    let mut base_config = Configuration::new_unix_sockets();
    base_config.common.default_num_partitions = num_partitions.try_into()?;
    base_config.bifrost.default_provider = ProviderKind::Replicated;
    base_config.common.log_filter = "restate=debug,warn".to_owned();
    base_config.common.log_format = LogFormat::Compact;
    base_config.common.log_disable_ansi_codes = true;

    let nodes = NodeSpec::new_test_nodes(
        base_config.clone(),
        BinarySource::CargoTest,
        EnumSet::all(),
        1,
        false,
    );

    let mut cluster = Cluster::builder()
        .cluster_name("distributed-snapshot")
        .nodes(nodes)
        .temp_base_dir("distributed_snapshot_multi_partition")
        .build()
        .start()
        .await?;

    let replicated_loglet_config = ReplicatedLogletConfig {
        target_nodeset_size: NodeSetSize::default(),
        replication_property: ReplicationProperty::new_unchecked(1),
    };

    info!("Provisioning the cluster with {num_partitions} partitions");
    cluster.nodes[0]
        .provision_cluster(
            None,
            ReplicationProperty::new_unchecked(1),
            Some(ProviderConfiguration::Replicated(replicated_loglet_config)),
        )
        .await
        .into_test_result()?;

    info!("Waiting until the cluster is healthy");
    cluster.wait_healthy(Duration::from_secs(60)).await?;

    let mut client = new_cluster_ctrl_client(
        create_tonic_channel(
            cluster.nodes[0].advertised_address().clone(),
            &NetworkingOptions::default(),
            DNSResolution::Gai,
        ),
        &base_config.networking,
    );

    info!("Waiting until at least one partition processor is leading");
    wait_for_partition_leader(&mut client).await?;

    // Deploy a mock service and send an invocation to create some state
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
    let admin_client = reqwest::Client::builder()
        .unix_socket(admin_uds)
        .build()?;

    let registration_response = admin_client
        .post("http://localhost/deployments")
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "uri": format!("http://127.0.0.1:{mock_svc_port}") }))
        .send()
        .await?;
    assert!(
        registration_response.status().is_success(),
        "Service deployment should succeed"
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

    // Send a few invocations to create some WAL activity across partitions
    info!("Sending initial invocations to generate WAL activity");
    let mut retry = RetryPolicy::fixed_delay(Duration::from_millis(500), None).into_iter();
    loop {
        let response = ingress_client
            .post("http://localhost/Counter/0/get")
            .send()
            .await?;
        if response.status().is_success() {
            break;
        }
        if let Some(delay) = retry.next() {
            tokio::time::sleep(delay).await;
        } else {
            panic!("Failed to invoke service after retries");
        }
    }

    // Trigger a distributed snapshot
    info!("Triggering distributed snapshot");
    let snapshot_response = client
        .create_distributed_snapshot(CreateDistributedSnapshotRequest {})
        .await?
        .into_inner();
    info!(
        snapshot_id = snapshot_response.snapshot_id,
        "Distributed snapshot initiated"
    );
    assert!(
        snapshot_response.snapshot_id > 0,
        "Snapshot ID should be a positive timestamp"
    );

    // Give time for the snapshot protocol to propagate markers
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify the cluster is still healthy — the protocol didn't crash anything
    info!("Verifying cluster health after snapshot protocol");
    wait_for_partition_leader(&mut client).await?;

    // Verify invocations still work after the snapshot protocol ran
    info!("Sending post-snapshot invocation to verify cluster health");
    let response = ingress_client
        .post("http://localhost/Counter/0/get")
        .send()
        .await?;
    assert!(
        response.status().is_success(),
        "Invocation after snapshot should succeed"
    );

    // Trigger a second snapshot to verify the protocol can run again
    info!("Triggering second distributed snapshot");
    let snapshot_response_2 = client
        .create_distributed_snapshot(CreateDistributedSnapshotRequest {})
        .await?
        .into_inner();
    assert!(
        snapshot_response_2.snapshot_id > snapshot_response.snapshot_id,
        "Second snapshot ID should be greater than first"
    );

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Final health check
    let response = ingress_client
        .post("http://localhost/Counter/0/get")
        .send()
        .await?;
    assert!(
        response.status().is_success(),
        "Invocation after second snapshot should succeed"
    );

    info!("Distributed snapshot test passed");
    cluster.graceful_shutdown(Duration::from_secs(10)).await?;

    Ok(())
}

async fn wait_for_partition_leader(
    client: &mut ClusterCtrlSvcClient<tonic::transport::Channel>,
) -> googletest::Result<()> {
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
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
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

