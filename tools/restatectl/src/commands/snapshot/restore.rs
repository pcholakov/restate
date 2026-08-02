// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::path::PathBuf;

use cling::prelude::*;

use restate_cli_util::ui::console::confirm_or_exit;
use restate_cli_util::{CliContext, c_println, c_warn};
use restate_core::protobuf::node_ctl_svc::{
    RestoreClusterRequest, RestoreClusterResponse, new_node_ctl_client,
};
use restate_types::cluster_backup::{BackupArtifactSet, ClusterBackupDescriptor};

use crate::connection::ConnectionInfo;
use crate::util::grpc_channel;

#[derive(Run, Parser, Collect, Clone, Debug)]
#[cling(run = "restore")]
pub struct RestoreOpts {
    /// Path to a V0 cluster backup descriptor JSON file.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    input: PathBuf,

    /// Restore the schema captured in the backup descriptor.
    #[arg(long)]
    restore_schema: bool,

    /// Keep the source cluster fingerprint instead of generating a new one.
    #[arg(long)]
    preserve_cluster_fingerprint: bool,

    /// Validate the restore on the target node without changing it.
    #[arg(long)]
    dry_run: bool,
}

async fn restore(connection: &ConnectionInfo, opts: &RestoreOpts) -> anyhow::Result<()> {
    let descriptor_json = tokio::fs::read(&opts.input).await?;
    let descriptor = parse_descriptor(
        &descriptor_json,
        opts.restore_schema,
        opts.preserve_cluster_fingerprint,
    )?;

    let address = connection.single_address.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "snapshots restore requires an explicitly addressed node; pass --single-address <address>"
        )
    })?;
    let mut client = new_node_ctl_client(grpc_channel(address.clone()), &CliContext::get().network);

    let ident = client.get_ident(()).await?.into_inner();
    if ident.node_id.is_some() {
        anyhow::bail!("refusing to restore onto {address}: the target node is already provisioned");
    }

    c_warn!(
        "V0 backups are best-effort and not a consistent cluster-wide cut. This restores all partitions onto one bootstrap node and replaces its cluster metadata and partition state. Ensure the target is empty and unprovisioned."
    );
    c_println!(
        "Backup source: cluster '{}' ({} partitions).",
        descriptor.source_cluster_name,
        descriptor.partition_table.len()
    );

    if !opts.dry_run {
        confirm_or_exit("Restore this backup onto the target node?")?;
    }

    let response = client
        .restore_cluster(RestoreClusterRequest {
            descriptor_json: descriptor_json.into(),
            restore_schema: opts.restore_schema,
            preserve_cluster_fingerprint: opts.preserve_cluster_fingerprint,
            dry_run: opts.dry_run,
        })
        .await?
        .into_inner();

    print_response(&response);
    Ok(())
}

fn parse_descriptor(
    descriptor_json: &[u8],
    restore_schema: bool,
    preserve_cluster_fingerprint: bool,
) -> anyhow::Result<ClusterBackupDescriptor> {
    let descriptor = serde_json::from_slice::<ClusterBackupDescriptor>(descriptor_json)
        .map_err(|error| anyhow::anyhow!("invalid backup descriptor JSON: {error}"))?;
    match descriptor.validate()? {
        BackupArtifactSet::TopologyOnly | BackupArtifactSet::Complete => {}
        BackupArtifactSet::Incomplete => {
            anyhow::bail!("backup descriptor is incomplete and cannot be restored");
        }
    }
    if restore_schema && descriptor.schema.is_none() {
        anyhow::bail!("--restore-schema requires a backup descriptor containing a schema");
    }
    if preserve_cluster_fingerprint && descriptor.source_cluster_fingerprint.is_none() {
        anyhow::bail!(
            "--preserve-cluster-fingerprint requires a backup descriptor containing a source cluster fingerprint"
        );
    }
    Ok(descriptor)
}

fn print_response(response: &RestoreClusterResponse) {
    let mode = if response.dry_run {
        "Restore validation succeeded"
    } else {
        "Cluster restore succeeded"
    };
    c_println!("{mode}.");
    c_println!("Target cluster: {}", response.target_cluster_name);
    if let Some(fingerprint) = &response.target_cluster_fingerprint {
        c_println!("Target cluster fingerprint: {fingerprint}");
    }
    c_println!(
        "{} partitions: {}",
        if response.dry_run {
            "Validated"
        } else {
            "Restored"
        },
        response.partition_count
    );
    c_println!(
        "Schema {}: {}",
        if response.dry_run {
            "would be restored"
        } else {
            "restored"
        },
        if response.schema_restored {
            "yes"
        } else {
            "no"
        }
    );
}

#[cfg(test)]
mod tests {
    use restate_types::Version;
    use restate_types::cluster_backup::{BackupConsistency, ClusterBackupDescriptor};
    use restate_types::partition_table::PartitionTable;
    use restate_types::time::MillisSinceEpoch;

    use super::parse_descriptor;

    fn descriptor_json() -> Vec<u8> {
        serde_json::to_vec(&ClusterBackupDescriptor {
            version: ClusterBackupDescriptor::VERSION,
            created_at: MillisSinceEpoch::new(1),
            consistency: BackupConsistency::BestEffort,
            source_snapshot_repository: "s3://bucket/backups".to_owned(),
            source_cluster_name: "source".to_owned(),
            source_cluster_fingerprint: None,
            source_cluster_features: Default::default(),
            partition_table: PartitionTable::with_equally_sized_partitions(Version::MIN, 2),
            schema: None,
            artifacts: None,
            failures: vec![],
        })
        .unwrap()
    }

    #[test]
    fn parses_a_valid_topology_only_descriptor() {
        let descriptor = parse_descriptor(&descriptor_json(), false, false).unwrap();

        assert_eq!(descriptor.source_cluster_name, "source");
        assert_eq!(descriptor.partition_table.len(), 2);
    }

    #[test]
    fn restore_options_require_their_descriptor_data() {
        let error = parse_descriptor(&descriptor_json(), true, false).unwrap_err();

        assert!(error.to_string().contains("--restore-schema"));

        let error = parse_descriptor(&descriptor_json(), false, true).unwrap_err();

        assert!(error.to_string().contains("--preserve-cluster-fingerprint"));
    }
}
