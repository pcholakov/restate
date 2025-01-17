// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use async_trait::async_trait;
use bytestring::ByteString;
use tonic::transport::Channel;
use tonic::{Code, Status};

use restate_core::metadata_store::{
    Precondition, ProvisionedMetadataStore, ReadError, VersionedValue, WriteError,
};
use restate_core::network::net_util::create_tonic_channel;
use restate_core::network::net_util::CommonClientConnectionOptions;
use restate_types::net::AdvertisedAddress;
use restate_types::Version;

use crate::grpc_svc::metadata_store_svc_client::MetadataStoreSvcClient;
use crate::grpc_svc::{DeleteRequest, GetRequest, PutRequest};
use crate::local::grpc::pb_conversions::ConversionError;

/// Client end to interact with the [`LocalMetadataStore`].
#[derive(Debug, Clone)]
pub struct LocalMetadataStoreClient {
    svc_client: MetadataStoreSvcClient<Channel>,
}

impl LocalMetadataStoreClient {
    pub fn new<T: CommonClientConnectionOptions>(
        metadata_store_address: AdvertisedAddress,
        options: &T,
    ) -> Self {
        let channel = create_tonic_channel(metadata_store_address, options);

        Self {
            svc_client: MetadataStoreSvcClient::new(channel),
        }
    }
}

#[async_trait]
impl ProvisionedMetadataStore for LocalMetadataStoreClient {
    async fn get(&self, key: ByteString) -> Result<Option<VersionedValue>, ReadError> {
        let response = self
            .svc_client
            .clone()
            .get(GetRequest { key: key.into() })
            .await
            .map_err(map_status_to_read_error)?;

        response
            .into_inner()
            .try_into()
            .map_err(|err: ConversionError| ReadError::Internal(err.to_string()))
    }

    async fn get_version(&self, key: ByteString) -> Result<Option<Version>, ReadError> {
        let response = self
            .svc_client
            .clone()
            .get_version(GetRequest { key: key.into() })
            .await
            .map_err(map_status_to_read_error)?;

        Ok(response.into_inner().into())
    }

    async fn put(
        &self,
        key: ByteString,
        value: VersionedValue,
        precondition: Precondition,
    ) -> Result<(), WriteError> {
        self.svc_client
            .clone()
            .put(PutRequest {
                key: key.into(),
                value: Some(value.into()),
                precondition: Some(precondition.into()),
            })
            .await
            .map_err(map_status_to_write_error)?;

        Ok(())
    }

    async fn delete(&self, key: ByteString, precondition: Precondition) -> Result<(), WriteError> {
        self.svc_client
            .clone()
            .delete(DeleteRequest {
                key: key.into(),
                precondition: Some(precondition.into()),
            })
            .await
            .map_err(map_status_to_write_error)?;

        Ok(())
    }
}

fn map_status_to_read_error(status: Status) -> ReadError {
    match &status.code() {
        Code::Unavailable => ReadError::Network(status.into()),
        _ => ReadError::Internal(status.to_string()),
    }
}

fn map_status_to_write_error(status: Status) -> WriteError {
    match &status.code() {
        Code::Unavailable => WriteError::Network(status.into()),
        Code::FailedPrecondition => WriteError::FailedPrecondition(status.message().to_string()),
        _ => WriteError::Internal(status.to_string()),
    }
}
