use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytestring::ByteString;
use rand::Rng;
use restate_types::config::NetworkingOptions;
use restate_types::errors::ConversionError;
use tokio::sync::RwLock;
use tonic::transport::Channel;
use tonic::{Code, Status};
use tracing::{debug, warn};

use restate_core::network::net_util::create_tonic_channel;
use restate_metadata_store::protobuf::metadata_proxy_svc::client::new_metadata_proxy_client;
use restate_metadata_store::protobuf::metadata_proxy_svc::{
    DeleteRequest, GetRequest, PutRequest, metadata_proxy_svc_client::MetadataProxySvcClient,
};
use restate_metadata_store::{MetadataStore, ProvisionError, ReadError, WriteError};
use restate_types::{
    Version,
    metadata::{Precondition, VersionedValue},
    net::AdvertisedAddress,
    nodes_config::NodesConfiguration,
    retries::RetryPolicy,
};

/// A cluster-level client for accessing a remote Metadata Store via [`MetadataProxySvcClient`].
/// This client internally manages connections to a set of known metadata proxy servers and cycles
/// through them in round-robin fashion starting with a random one.
pub struct RemoteMetadataStore {
    connections: Arc<RwLock<Vec<MetadataProxySvcClient<Channel>>>>,
    current_index: Arc<AtomicUsize>,
    request_timeout: Duration,
    retry_policy: RetryPolicy,
}

impl RemoteMetadataStore {
    pub fn new(
        addresses: Vec<AdvertisedAddress>,
        request_timeout: Duration,
        retry_policy: RetryPolicy,
    ) -> Result<Self, RemoteClientError> {
        let initial_index = rand::rng().random_range(0..addresses.len());
        Self::new_with_initial_index(addresses, request_timeout, retry_policy, initial_index)
    }

    fn new_with_initial_index(
        addresses: Vec<AdvertisedAddress>,
        request_timeout: Duration,
        retry_policy: RetryPolicy,
        initial_index: usize,
    ) -> Result<Self, RemoteClientError> {
        if addresses.is_empty() {
            return Err(RemoteClientError::NoAddresses);
        }

        let connections = addresses
            .into_iter()
            .map(|addr| {
                new_metadata_proxy_client(create_tonic_channel(addr, &NetworkingOptions::default()))
            })
            .collect::<Vec<_>>();

        let current_index = Arc::new(AtomicUsize::new(initial_index));

        Ok(Self {
            connections: Arc::new(RwLock::new(connections)),
            current_index,
            request_timeout,
            retry_policy,
        })
    }

    async fn execute_with_retry<F, Fut, T>(&self, operation: F) -> Result<T, RemoteClientError>
    where
        F: Fn(MetadataProxySvcClient<Channel>) -> Fut,
        Fut: std::future::Future<Output = Result<T, RemoteClientError>>,
    {
        let mut retry_iter = self.retry_policy.iter();

        loop {
            match self.try_execute_operation(&operation).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    if !err.is_retryable() {
                        debug!("Non-retryable error encountered: {}", err);
                        return Err(err);
                    }

                    if let Some(delay) = retry_iter.next() {
                        debug!(
                            "Retrying operation after error: {}, delay: {:?}",
                            err, delay
                        );
                        tokio::time::sleep(delay).await;
                    } else {
                        warn!("Retries exhausted for operation: {}", err);
                        return Err(err);
                    }
                }
            }
        }
    }

    async fn try_execute_operation<F, Fut, T>(&self, operation: &F) -> Result<T, RemoteClientError>
    where
        F: Fn(MetadataProxySvcClient<Channel>) -> Fut,
        Fut: std::future::Future<Output = Result<T, RemoteClientError>>,
    {
        let current_index = self.current_index.load(Ordering::Relaxed);
        let connections = self.connections.write().await;
        let client = connections[current_index % connections.len()].clone();

        let result = tokio::time::timeout(self.request_timeout, operation(client))
            .await
            .unwrap_or_else(|_| Err(RemoteClientError::Timeout));

        match result {
            Ok(value) => return Ok(value),
            Err(err) => {
                if err.is_connection_error() {
                    warn!("Connection error: {}", err);
                    self.current_index.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        Err(RemoteClientError::AllConnectionsFailed)
    }
}

#[async_trait]
impl MetadataStore for RemoteMetadataStore {
    async fn get(&self, key: ByteString) -> Result<Option<VersionedValue>, ReadError> {
        let request = GetRequest {
            key: key.to_string(),
        };

        self.execute_with_retry(|mut client| {
            let request = request.clone();
            async move {
                let response = client
                    .get(request)
                    .await
                    .map_err(RemoteClientError::from)?
                    .into_inner();

                response
                    .value
                    .map(|v| v.try_into())
                    .transpose()
                    .map_err(|e: ConversionError| RemoteClientError::ClientError(e.to_string()))
            }
        })
        .await
        .map_err(ReadError::from)
    }

    async fn get_version(&self, key: ByteString) -> Result<Option<Version>, ReadError> {
        let request = GetRequest {
            key: key.to_string(),
        };

        self.execute_with_retry(|mut client| {
            let request = request.clone();
            async move {
                let response = client
                    .get_version(request)
                    .await
                    .map_err(RemoteClientError::from)?
                    .into_inner();

                Ok(response.version.map(Version::from))
            }
        })
        .await
        .map_err(ReadError::from)
    }

    async fn put(
        &self,
        key: ByteString,
        value: VersionedValue,
        precondition: Precondition,
    ) -> Result<(), WriteError> {
        let request = PutRequest {
            key: key.to_string(),
            value: Some(value.into()),
            precondition: Some(precondition.into()),
        };

        self.execute_with_retry(|mut client| {
            let request = request.clone();
            async move {
                client.put(request).await.map_err(RemoteClientError::from)?;
                Ok(())
            }
        })
        .await
        .map_err(WriteError::from)
    }

    async fn delete(&self, key: ByteString, precondition: Precondition) -> Result<(), WriteError> {
        let request = DeleteRequest {
            key: key.to_string(),
            precondition: Some(precondition.into()),
        };

        self.execute_with_retry(|mut client| {
            let request = request.clone();
            async move {
                client
                    .delete(request)
                    .await
                    .map_err(RemoteClientError::from)?;
                Ok(())
            }
        })
        .await
        .map_err(WriteError::from)
    }

    async fn provision(
        &self,
        _nodes_configuration: &NodesConfiguration,
    ) -> Result<bool, ProvisionError> {
        Err(ProvisionError::NotSupported(
            "Remote metadata store does not support direct provisioning".to_string(),
        ))
    }
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum RemoteClientError {
    #[error("No addresses provided")]
    NoAddresses,
    #[error("All connections failed")]
    AllConnectionsFailed,
    #[error("gRPC error: {0}")]
    GrpcError(String, Code),
    #[error("Client error: {0}")]
    ClientError(String),
    #[error("Timeout")]
    Timeout,
}

impl RemoteClientError {
    fn is_retryable(&self) -> bool {
        match self {
            RemoteClientError::NoAddresses => false,
            RemoteClientError::AllConnectionsFailed | RemoteClientError::Timeout => true,
            RemoteClientError::GrpcError(_, code) => match code {
                Code::Unavailable | Code::DeadlineExceeded | Code::ResourceExhausted => true,
                Code::FailedPrecondition => false,
                _ => true,
            },
            RemoteClientError::ClientError(_) => false,
        }
    }

    fn is_connection_error(&self) -> bool {
        match self {
            RemoteClientError::Timeout => true,
            RemoteClientError::GrpcError(_, code) => {
                matches!(code, Code::Unavailable | Code::DeadlineExceeded)
            }
            _ => false,
        }
    }
}

impl From<Status> for RemoteClientError {
    fn from(status: Status) -> Self {
        RemoteClientError::GrpcError(status.message().to_string(), status.code())
    }
}

impl From<RemoteClientError> for ReadError {
    fn from(err: RemoteClientError) -> Self {
        {
            match err {
                RemoteClientError::GrpcError(_, Code::FailedPrecondition) => {
                    ReadError::terminal(err)
                }
                _ => ReadError::retryable(err),
            }
        }
    }
}

impl From<RemoteClientError> for WriteError {
    fn from(err: RemoteClientError) -> Self {
        {
            match err {
                RemoteClientError::GrpcError(msg, Code::FailedPrecondition) => {
                    WriteError::FailedPrecondition(msg)
                }
                _ => WriteError::retryable(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use restate_metadata_store::MetadataStoreClient;
    use restate_node::network_server::grpc_svc_handler::MetadataProxySvcHandler;
    use restate_types::errors::MaybeRetryableError;
    use restate_types::net::AdvertisedAddress;
    use restate_types::retries::RetryPolicy;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use test_log::test;
    use tokio::net::TcpListener;

    async fn start_proxy_server(port: u16) -> anyhow::Result<()> {
        let metadata_store = MetadataStoreClient::new_in_memory();
        let metadata_service = MetadataProxySvcHandler::new(metadata_store).into_server();

        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;
        let healthy_server = tonic::transport::Server::builder()
            .add_service(metadata_service)
            .serve(addr);
        tokio::spawn(healthy_server);

        Ok(())
    }

    async fn start_hanging_server(
        port: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use std::net::SocketAddr;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;
        let listener = TcpListener::bind(&addr).await?;

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Just hold the connection open and sleep forever
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    drop(stream);
                });
            }
        });

        // Give the server a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    #[test]
    fn test_remote_client_error_is_connection_error() {
        let error = RemoteClientError::GrpcError("test".to_string(), tonic::Code::Unavailable);
        assert!(error.is_connection_error());

        let error = RemoteClientError::GrpcError("test".to_string(), tonic::Code::DeadlineExceeded);
        assert!(error.is_connection_error());

        let error = RemoteClientError::GrpcError("test".to_string(), tonic::Code::Internal);
        assert!(!error.is_connection_error());

        let error = RemoteClientError::AllConnectionsFailed;
        assert!(!error.is_connection_error());
    }

    #[cfg(feature = "grpc-client")]
    #[test]
    fn test_error_conversions() {
        use restate_metadata_store::{ReadError, WriteError};
        use tonic::Status;

        // Test Status to RemoteClientError conversion
        let status = Status::unavailable("test message");
        let remote_error = RemoteClientError::from(status);
        assert!(matches!(
            remote_error,
            RemoteClientError::GrpcError(_, tonic::Code::Unavailable)
        ));

        // Test RemoteClientError to ReadError conversion
        let read_error = ReadError::from(RemoteClientError::AllConnectionsFailed);
        assert!(read_error.retryable());

        let read_error = ReadError::from(RemoteClientError::GrpcError(
            "test".to_string(),
            tonic::Code::FailedPrecondition,
        ));
        assert!(!read_error.retryable());

        // Test RemoteClientError to WriteError conversion
        let write_error = WriteError::from(RemoteClientError::GrpcError(
            "precondition failed".to_string(),
            tonic::Code::FailedPrecondition,
        ));
        assert!(matches!(write_error, WriteError::FailedPrecondition(_)));

        let write_error = WriteError::from(RemoteClientError::AllConnectionsFailed);
        assert!(write_error.retryable());
    }

    #[test(restate_core::test)]
    async fn test_connection_cycling_with_failures() -> googletest::Result<()> {
        start_proxy_server(15001)
            .await
            .expect("Failed to start simple server");

        start_hanging_server(15002)
            .await
            .expect("Failed to start hanging server");

        let addresses = vec![
            AdvertisedAddress::from_str("http://127.0.0.1:15003").unwrap(),
            AdvertisedAddress::from_str("http://127.0.0.1:15002").unwrap(),
            AdvertisedAddress::from_str("http://127.0.0.1:15000").unwrap(),
            // AdvertisedAddress::from_str("http://127.0.0.1:15001").unwrap(),
        ];

        let remote_store = RemoteMetadataStore::new_with_initial_index(
            addresses,
            Duration::from_millis(10),
            RetryPolicy::exponential(
                Duration::from_millis(5),
                2.0,
                Some(5),
                Some(Duration::from_millis(200)),
            ),
            0,
        )
        .expect("Failed to create RemoteMetadataStore");

        let initial_index = remote_store.current_index.load(Ordering::Relaxed);
        assert_eq!(initial_index, 0);

        let response = remote_store.get("k".into()).await?;
        assert!(response.is_none());

        let client_index_after_call = remote_store.current_index.load(Ordering::Relaxed);
        assert_eq!(client_index_after_call, 2);

        Ok(())
    }
}
