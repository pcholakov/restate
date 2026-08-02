// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#![allow(clippy::large_futures)]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::num::NonZeroU8;
use std::path::Path;
use std::time::Duration;

use cling::prelude::*;
use enumset::EnumSet;
use http::header::CONTENT_TYPE;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::Code;
use tracing::info;
use url::Url;

use restate_core::network::net_util::{DNSResolution, create_tonic_channel};
use restate_core::protobuf::cluster_ctrl_svc::{
    CreatePartitionSnapshotRequest, new_cluster_ctrl_client,
};
use restate_core::protobuf::node_ctl_svc::{
    RestoreClusterRequest, RestoreClusterResponse, new_node_ctl_client,
    node_ctl_svc_client::NodeCtlSvcClient,
};
use restate_local_cluster_runner::{
    cluster::{Cluster, StartedCluster},
    node::{BinarySource, NodeSpec},
};
use restate_types::cluster_backup::{BackupArtifactSet, ClusterBackupDescriptor};
use restate_types::config::Configuration;
use restate_types::logs::metadata::ProviderKind::Replicated;
use restate_types::net::address::{ListenerPort, PeerNetAddress};
use restate_types::replication::ReplicationProperty;

#[test_log::test(tokio::test)]
async fn restores_partition_data_and_schema_from_a_v0_backup() -> anyhow::Result<()> {
    let source_snapshots_dir = TempDir::new()?;
    let mut config = Configuration::new_unix_sockets();
    config.common.default_num_partitions = 2.try_into()?;
    config.bifrost.default_provider = Replicated;
    config.common.log_disable_ansi_codes = true;
    config.worker.snapshots.num_retained = NonZeroU8::new(2).unwrap();
    config.worker.snapshots.destination = Some(
        Url::from_file_path(source_snapshots_dir.path())
            .expect("temporary snapshot directory is a valid file URL")
            .to_string(),
    );

    let source_nodes = NodeSpec::new_test_nodes(
        config.clone(),
        BinarySource::CargoTest,
        EnumSet::all(),
        1,
        false,
    );
    let mut source = Cluster::builder()
        .cluster_name("v0-backup-source")
        .nodes(source_nodes)
        .temp_base_dir("v0_backup_source")
        .build()
        .start()
        .await?;

    info!("Provisioning the source cluster");
    source.nodes[0]
        .provision_cluster(
            None,
            ReplicationProperty::new_unchecked(1),
            None,
            EnumSet::empty(),
        )
        .await?;
    source.wait_healthy(Duration::from_secs(60)).await?;

    let mock_service_url = start_mock_service().await?;
    let source_admin = unix_client(
        source.nodes[0]
            .admin_address()
            .as_ref()
            .expect("all-role source node has an admin address"),
    )?;
    let deployment_response = source_admin
        .post("http://localhost/deployments")
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "uri": mock_service_url }))
        .send()
        .await?;
    assert!(
        deployment_response.status().is_success(),
        "source deployment registration failed: {}",
        deployment_response.status()
    );

    let source_ingress = unix_client(
        source.nodes[0]
            .ingress_address()
            .as_ref()
            .expect("all-role source node has an ingress address"),
    )?;
    let source_value: i32 = serde_json::from_slice(
        invoke_until_success(&source_ingress, "http://localhost/Counter/0/add", Some("3"))
            .await?
            .bytes()
            .await?
            .as_ref(),
    )?;

    // Populate a first retained generation before restatectl captures the exact backup. This
    // catches accidental pairing of a new snapshot ID with the oldest retention-safe LSN.
    create_snapshot_round(&source, &config, 2).await?;

    let descriptors_dir = TempDir::new()?;
    let exact_descriptor_path = descriptors_dir.path().join("exact.json");
    let topology_descriptor_path = descriptors_dir.path().join("topology.json");
    let source_address = source.nodes[0].advertised_address().to_string();
    let source_repository = config
        .worker
        .snapshots
        .destination
        .as_deref()
        .expect("the source snapshot repository is configured");

    run_backup_command(
        &source_address,
        source_repository,
        &exact_descriptor_path,
        false,
    )
    .await?;
    // Exact backup artifacts are protected independently of rolling retention. Advance three
    // ordinary generations beyond num_retained=2 before attempting the exact restore below.
    for _ in 0..3 {
        create_snapshot_round(&source, &config, 2).await?;
    }
    run_backup_command(
        &source_address,
        source_repository,
        &topology_descriptor_path,
        true,
    )
    .await?;

    let exact_descriptor = read_descriptor(&exact_descriptor_path).await?;
    assert_eq!(exact_descriptor.validate()?, BackupArtifactSet::Complete);
    assert!(exact_descriptor.schema.is_some());
    let topology_descriptor = read_descriptor(&topology_descriptor_path).await?;
    assert_eq!(
        topology_descriptor.validate()?,
        BackupArtifactSet::TopologyOnly
    );
    assert!(topology_descriptor.schema.is_some());

    info!("Stopping the source cluster before restoring onto a fresh target");
    source.graceful_shutdown(Duration::from_secs(30)).await?;

    // First perform a real restore while deliberately omitting schema. Partition snapshots also
    // cache schema, so this verifies that the restore path actively removes that cached copy.
    let schema_less_snapshots_dir = TempDir::new()?;
    let mut schema_less_config = config.clone();
    schema_less_config.worker.snapshots.destination = Some(
        Url::from_file_path(schema_less_snapshots_dir.path())
            .expect("temporary snapshot directory is a valid file URL")
            .to_string(),
    );
    let schema_less_nodes = NodeSpec::new_test_nodes(
        schema_less_config.clone(),
        BinarySource::CargoTest,
        EnumSet::all(),
        1,
        false,
    );
    let mut schema_less_target = Cluster::builder()
        .cluster_name("v0-backup-schema-less-target")
        .nodes(schema_less_nodes)
        .temp_base_dir("v0_backup_schema_less_target")
        .build()
        .start()
        .await?;
    let mut schema_less_client = new_node_ctl_client(
        create_tonic_channel(
            schema_less_target.nodes[0].advertised_address().clone(),
            &schema_less_config.networking,
            DNSResolution::Gai,
        ),
        &schema_less_config.networking,
    );
    let schema_less_request = RestoreClusterRequest {
        descriptor_json: serde_json::to_vec(&exact_descriptor)?.into(),
        restore_schema: false,
        preserve_cluster_fingerprint: false,
        dry_run: false,
    };
    let mut dry_run_request = schema_less_request.clone();
    dry_run_request.dry_run = true;
    let dry_run = restore_when_ready(&mut schema_less_client, dry_run_request).await?;
    assert!(dry_run.dry_run);
    assert_eq!(dry_run.partition_count, 2);
    assert!(!dry_run.schema_restored);
    assert_eq!(
        schema_less_client
            .cluster_health(())
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable,
        "dry-run must leave the target unprovisioned"
    );
    let schema_less_restored =
        restore_when_ready(&mut schema_less_client, schema_less_request).await?;
    assert!(!schema_less_restored.schema_restored);
    schema_less_target
        .wait_healthy(Duration::from_secs(60))
        .await?;
    let schema_less_admin = unix_client(
        schema_less_target.nodes[0]
            .admin_address()
            .as_ref()
            .expect("all-role target node has an admin address"),
    )?;
    assert_no_deployments(&schema_less_admin).await?;
    schema_less_target
        .graceful_shutdown(Duration::from_secs(30))
        .await?;

    // Restore a topology-only descriptor with schema. Give the target a distinct active snapshot
    // repository: the source repository is read-only restore input, not the new cluster's output.
    let target_snapshots_dir = TempDir::new()?;
    let mut target_config = config.clone();
    target_config.worker.snapshots.destination = Some(
        Url::from_file_path(target_snapshots_dir.path())
            .expect("temporary snapshot directory is a valid file URL")
            .to_string(),
    );
    let target_nodes = NodeSpec::new_test_nodes(
        target_config.clone(),
        BinarySource::CargoTest,
        EnumSet::all(),
        1,
        false,
    );
    let mut target = Cluster::builder()
        .cluster_name("v0-backup-target")
        .nodes(target_nodes)
        .temp_base_dir("v0_backup_target")
        .build()
        .start()
        .await?;
    let mut target_client = new_node_ctl_client(
        create_tonic_channel(
            target.nodes[0].advertised_address().clone(),
            &target_config.networking,
            DNSResolution::Gai,
        ),
        &target_config.networking,
    );
    let restored = restore_when_ready(
        &mut target_client,
        RestoreClusterRequest {
            descriptor_json: serde_json::to_vec(&topology_descriptor)?.into(),
            restore_schema: true,
            preserve_cluster_fingerprint: false,
            dry_run: false,
        },
    )
    .await?;
    assert!(!restored.dry_run);
    assert_eq!(restored.partition_count, 2);
    assert!(restored.schema_restored);
    target.wait_healthy(Duration::from_secs(60)).await?;

    let target_admin = unix_client(
        target.nodes[0]
            .admin_address()
            .as_ref()
            .expect("all-role target node has an admin address"),
    )?;
    let deployments: serde_json::Value = target_admin
        .get("http://localhost/deployments")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        deployments["deployments"]
            .as_array()
            .is_some_and(|deployments| !deployments.is_empty()),
        "restored schema does not contain the registered deployment: {deployments}"
    );

    let target_ingress = unix_client(
        target.nodes[0]
            .ingress_address()
            .as_ref()
            .expect("all-role target node has an ingress address"),
    )?;
    let restored_value: i32 = serde_json::from_slice(
        invoke_until_success(&target_ingress, "http://localhost/Counter/0/get", None)
            .await?
            .bytes()
            .await?
            .as_ref(),
    )?;
    assert_eq!(restored_value, source_value);

    let advanced_value: i32 = serde_json::from_slice(
        invoke_until_success(&target_ingress, "http://localhost/Counter/0/add", Some("1"))
            .await?
            .bytes()
            .await?
            .as_ref(),
    )?;
    assert_eq!(advanced_value, source_value + 1);

    // A new-name/new-fingerprint restored cluster must be able to snapshot into its own active
    // repository without colliding with source repository identity metadata.
    create_snapshot_round(&target, &target_config, 2).await?;

    target.graceful_shutdown(Duration::from_secs(30)).await?;

    Ok(())
}

async fn run_backup_command(
    source_address: &str,
    source_repository: &str,
    output: &Path,
    use_latest: bool,
) -> anyhow::Result<()> {
    let mut args = vec![
        OsString::from("restatectl"),
        OsString::from("--single-address"),
        OsString::from(source_address),
        OsString::from("snapshots"),
        OsString::from("backup"),
        OsString::from("--output"),
        output.as_os_str().to_owned(),
        OsString::from("--snapshot-repository"),
        OsString::from(source_repository),
        OsString::from("--include-schema"),
        OsString::from("--concurrency"),
        OsString::from("2"),
    ];
    if use_latest {
        args.push(OsString::from("--use-latest"));
    }
    Cling::<restatectl::CliApp>::try_parse_from(args)?
        .run()
        .await
        .result()?;
    Ok(())
}

async fn read_descriptor(path: &Path) -> anyhow::Result<ClusterBackupDescriptor> {
    Ok(serde_json::from_slice(&tokio::fs::read(path).await?)?)
}

async fn create_snapshot_round(
    cluster: &StartedCluster,
    config: &Configuration,
    partition_count: u32,
) -> anyhow::Result<()> {
    let mut client = new_cluster_ctrl_client(
        create_tonic_channel(
            cluster.nodes[0].advertised_address().clone(),
            &config.networking,
            DNSResolution::Gai,
        ),
        &config.networking,
    );
    for partition_id in 0..partition_count {
        for attempt in 0..120 {
            match client
                .create_partition_snapshot(CreatePartitionSnapshotRequest {
                    partition_id,
                    min_target_lsn: None,
                    trim_log: false,
                    protect_from_retention: false,
                })
                .await
            {
                Ok(_) => break,
                Err(error) if error.code() == Code::Internal && attempt < 119 => {
                    info!(partition_id, %error, "waiting for a snapshot-capable partition processor");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

async fn restore_when_ready(
    client: &mut NodeCtlSvcClient<tonic::transport::Channel>,
    request: RestoreClusterRequest,
) -> anyhow::Result<RestoreClusterResponse> {
    for attempt in 0..120 {
        match client.restore_cluster(request.clone()).await {
            Ok(response) => return Ok(response.into_inner()),
            Err(error) if error.code() == Code::Unavailable && attempt < 119 => {
                info!(%error, "waiting for the unprovisioned target RPC server");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("restore retry loop returns on its final attempt")
}

async fn assert_no_deployments(client: &reqwest::Client) -> anyhow::Result<()> {
    let deployments: serde_json::Value = client
        .get("http://localhost/deployments")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        deployments["deployments"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "schema-less restore retained imported deployments: {deployments}"
    );
    Ok(())
}

fn unix_client<P: ListenerPort>(
    address: &restate_types::net::address::AdvertisedAddress<P>,
) -> anyhow::Result<reqwest::Client> {
    let address = address.clone().into_address()?;
    let PeerNetAddress::Uds(address) = address else {
        anyhow::bail!("expected a unix-domain admin or ingress address");
    };
    Ok(reqwest::Client::builder().unix_socket(address).build()?)
}

async fn invoke_until_success(
    client: &reqwest::Client,
    url: &str,
    body: Option<&str>,
) -> anyhow::Result<reqwest::Response> {
    for attempt in 0..60 {
        let mut request = client.post(url);
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_owned());
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                info!(%url, status = %response.status(), attempt, "waiting for invocation to become available")
            }
            Err(error) => {
                info!(%url, %error, attempt, "waiting for invocation to become available")
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("invocation at {url} did not become available within 15 seconds")
}

async fn start_mock_service() -> anyhow::Result<String> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let address = listener.local_addr()?;
    let (running_tx, running_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Err(error) = mock_service_endpoint::listener::run_listener(listener, || {
            let _ = running_tx.send(());
        })
        .await
        {
            panic!("mock service endpoint failed: {error:?}");
        }
    });
    running_rx.await?;
    Ok(format!("http://{address}"))
}
