// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::num::NonZeroU32;

use anyhow::{bail, Context};
use cling::prelude::*;
use tonic::codec::CompressionEncoding;
use tracing::error;

use restate_admin::cluster_controller::protobuf::cluster_ctrl_svc_client::ClusterCtrlSvcClient;
use restate_admin::cluster_controller::protobuf::{ChainExtension, SealAndExtendChainRequest};
use restate_cli_util::{c_eprintln, c_println};
use restate_types::logs::metadata::{Logs, ProviderKind, Segment, SegmentIndex};
use restate_types::logs::{LogId, LogletId};
use restate_types::nodes_config::Role;
use restate_types::protobuf::common::Version;
use restate_types::replicated_loglet::ReplicatedLogletParams;
use restate_types::replication::{NodeSet, ReplicationProperty};
use restate_types::{GenerationalNodeId, PlainNodeId};

use crate::connection::ConnectionInfo;
use crate::util::RangeParam;

#[derive(Run, Parser, Collect, Clone, Debug)]
#[cling(run = "reconfigure")]
pub struct ReconfigureOpts {
    /// The log id or range to seal and extend, e.g. "0", "1-4".
    #[clap(long, short, required = true)]
    log_id: Vec<RangeParam>,
    /// If a loglet provider is specified, the new loglet will be configured using the provided parameters.
    /// Otherwise, it will be automatically reconfigured based on the cluster configuration.
    #[clap(long, short)]
    provider: Option<ProviderKind>,
    /// Option segment index to seal. The tail segment is chosen automatically if not provided.
    #[clap(long, short = 'i')]
    segment_index: Option<u32>,
    /// The [minimum] expected metadata version
    #[clap(long, short, default_value = "1")]
    min_version: NonZeroU32,
    /// Replication property for a new replicated segment; by default reuse
    /// the sealed segment's value iff its provider kind was also "replicated"
    #[clap(long)]
    replication: Option<ReplicationProperty>,
    /// A comma-separated list of the nodes in the nodeset. e.g. N1,N2,N4 or 1,2,3
    /// By default reuse the sealed segment's value iff its provider kind was also "replicated"
    #[clap(long, value_delimiter=',', num_args = 0..)]
    nodeset: Vec<PlainNodeId>,
    /// The generational node id of the sequencer node, e.g. N1:1
    /// By default reuse the sealed segment's value iff its provider kind was also "replicated"
    #[clap(long)]
    sequencer: Option<GenerationalNodeId>,
}

async fn reconfigure(connection: &ConnectionInfo, opts: &ReconfigureOpts) -> anyhow::Result<()> {
    let logs = connection.get_logs().await?;

    for log_id in opts.log_id.iter().flatten().map(LogId::from) {
        if let Err(err) = inner_reconfigure(connection, opts, &logs, log_id).await {
            error!("Failed to reconfigure log {log_id}: {err}");
        }
        c_println!("");
    }

    Ok(())
}

async fn inner_reconfigure(
    connection: &ConnectionInfo,
    opts: &ReconfigureOpts,
    logs: &Logs,
    log_id: LogId,
) -> anyhow::Result<()> {
    let chain = logs
        .chain(&log_id)
        .with_context(|| format!("Unknown log id '{log_id}'"))?;

    let tail_segment = chain.tail();

    let tail_index = opts
        .segment_index
        .map(SegmentIndex::from)
        .unwrap_or(chain.tail_index());

    let next_loglet_id = LogletId::new(log_id, tail_index.next());

    let provider = opts.provider.unwrap_or(tail_segment.config.kind);

    let params = match (provider, tail_segment.config.kind) {
        // we can always go to replicated loglet
        (ProviderKind::Replicated, _) => {
            replicated_loglet_params(opts, next_loglet_id, &tail_segment)?
        }
        // but never back to anything else
        (_, ProviderKind::Replicated) => {
            bail!(
                "Switching back to {} provider kind is not supported",
                provider
            );
        }
        (ProviderKind::Local, _) => {
            if opts.sequencer.is_some() || !opts.nodeset.is_empty() {
                bail!("'sequencer' or 'nodeset' are only allowed with 'replicated' provider");
            }
            u64::from(next_loglet_id).to_string()
        }
        #[cfg(any(test, feature = "memory-loglet"))]
        (ProviderKind::InMemory, _) => {
            if opts.sequencer.is_some() || !opts.nodeset.is_empty() {
                bail!("'sequencer' or 'nodeset' are only allowed with 'replicated' provider");
            }
            u64::from(next_loglet_id).to_string()
        }
    };

    let extension = ChainExtension {
        provider: provider.to_string(),
        segment_index: Some(tail_index.into()),
        params,
    };

    let request = SealAndExtendChainRequest {
        log_id: log_id.into(),
        min_version: Some(Version {
            value: opts.min_version.get(),
        }),
        extension: Some(extension),
    };

    let response = connection
        .try_each(Some(Role::Admin), |channel| async {
            let mut client =
                ClusterCtrlSvcClient::new(channel).accept_compressed(CompressionEncoding::Gzip);

            client.seal_and_extend_chain(request.clone()).await
        })
        .await?
        .into_inner();

    let Some(sealed_segment) = response.sealed_segment else {
        c_println!("✅ Log scheduled for reconfiguration");
        return Ok(());
    };
    c_println!("✅ Log reconfiguration ({log_id})");
    c_println!(" └ Segment Index: {}", response.new_segment_index);

    c_println!();
    c_println!("Sealed segment");
    c_println!(" ├ Index: {}", response.new_segment_index - 1);
    c_println!(" ├ Tail LSN: {}", sealed_segment.tail_offset);
    c_println!(" ├ Provider: {}", sealed_segment.provider);
    let Ok(provider) = ProviderKind::from_str(&sealed_segment.provider, true) else {
        c_eprintln!("Unknown provider type '{}'", sealed_segment.provider);
        return Ok(());
    };

    if provider == ProviderKind::Replicated {
        let params = ReplicatedLogletParams::deserialize_from(sealed_segment.params.as_bytes())?;
        c_println!(" ├ Loglet ID: {}", params.loglet_id);
        c_println!(" ├ Sequencer: {}", params.sequencer);
        c_println!(" ├ Replication: {}", params.replication);
        c_println!(" └ Node Set: {:#}", params.nodeset);
    } else {
        c_println!(" └ Params: {}", sealed_segment.params);
    }

    Ok(())
}

fn replicated_loglet_params(
    opts: &ReconfigureOpts,
    loglet_id: LogletId,
    tail: &Segment<'_>,
) -> anyhow::Result<String> {
    let params = if tail.config.kind == ProviderKind::Replicated {
        let last_params = ReplicatedLogletParams::deserialize_from(tail.config.params.as_bytes())
            .context("Last segment params in chain is invalid")?;

        ReplicatedLogletParams {
            loglet_id,
            nodeset: if opts.nodeset.is_empty() {
                last_params.nodeset
            } else {
                NodeSet::from_iter(opts.nodeset.iter().cloned())
            },
            replication: opts
                .replication
                .clone()
                .unwrap_or(last_params.replication.clone()),
            sequencer: opts.sequencer.unwrap_or(last_params.sequencer),
        }
    } else {
        ReplicatedLogletParams {
            loglet_id,
            nodeset: if opts.nodeset.is_empty() {
                bail!("Missing nodeset. Nodeset is required if last segment is not of replicated type");
            } else {
                NodeSet::from_iter(opts.nodeset.iter().cloned())
            },
            replication: opts.replication.clone().context("Missing replication. Replication factor is required if last segment is not of replicated type")?,
            sequencer: opts.sequencer.context("Missing sequencer. Sequencer is required if last segment is not of replicated type")?,
        }
    };

    params.serialize().map_err(Into::into)
}
