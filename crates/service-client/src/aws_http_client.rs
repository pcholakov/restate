// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! HTTP client for the AWS Lambda API.
//!
//! The SDK ships its own hyper-backed client, but exposes none of hyper's HTTP/2
//! settings. Each partition-local client multiplexes Lambda invocations onto
//! long-lived HTTP/2 connections with no liveness detection. If a connection is
//! silently blackholed, its in-flight requests hang until the kernel exhausts its
//! retransmission budget, roughly 15 minutes later. This client enables keep-alive
//! pings, which bound that delay to the ping interval plus its timeout.
//!
//! This transport is HTTP/2-only. Endpoint overrides and proxies must support H2.
//!
//! Everything else here reproduces the contract of the SDK's own adapter
//! (`aws-smithy-http-client` 1.3.0, `src/client.rs`), which the Lambda client's
//! interceptors rely on: capturing the pooled connection so that it can be
//! poisoned after a transient failure, classifying transport errors, honoring the
//! `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment variables, and offering the
//! same TLS parameters to AWS endpoints. Re-check the ported pieces against that
//! source when the SDK dependencies are upgraded.

use std::borrow::Cow;
use std::error::Error;
use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::Duration;

use aws_smithy_runtime_api::client::connection::{CaptureSmithyConnection, ConnectionMetadata};
use aws_smithy_runtime_api::client::connector_metadata::ConnectorMetadata;
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpClient,
    SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;
use dashmap::DashMap;
use http::Uri;
use http::uri::Scheme;
use hyper_rustls::{ConfigBuilderExt, HttpsConnector, MaybeHttpsStream};
use hyper_util::client::legacy::connect::HttpConnector as TcpConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioIo;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::{CipherSuite, ClientConfig};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tower::{Service, ServiceExt};

use restate_types::config::Http2KeepAliveOptions;

use crate::pool::{self, ConnectionHandle, Pool};

type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// TLS parity with the SDK's rustls provider (`aws-smithy-http-client` 1.3.0,
/// `src/client/tls/rustls_provider.rs`): the same restricted cipher-suite list,
/// native roots, no client auth. Deliberately not the crate's deployment-facing
/// TLS config: that one honors `SSLKEYLOGFILE`, and sessions to the AWS API carry
/// credentials that a debugging env var must not make decryptable.
static AWS_TLS_CONFIG: LazyLock<ClientConfig> = LazyLock::new(|| {
    // The crypto provider is set explicitly because the ring and aws_lc_rs rustls
    // features are both active, which disables auto-installation.
    ClientConfig::builder_with_provider(Arc::new(restrict_ciphers(
        rustls::crypto::aws_lc_rs::default_provider(),
    )))
    .with_protocol_versions(rustls::DEFAULT_VERSIONS)
    .expect("default versions are supported")
    .with_native_roots()
    .expect("can load native certificates")
    .with_no_client_auth()
});

/// The handshake config for connections tunneled through a proxy.
static AWS_TUNNEL_TLS_CONFIG: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| {
    let mut config = AWS_TLS_CONFIG.clone();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(config)
});

/// The cipher suites the SDK offers to AWS endpoints, in the SDK's order.
fn restrict_ciphers(base: CryptoProvider) -> CryptoProvider {
    const SUITES: &[CipherSuite] = &[
        CipherSuite::TLS13_AES_256_GCM_SHA384,
        CipherSuite::TLS13_AES_128_GCM_SHA256,
        // TLS 1.2 suites
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ];

    CryptoProvider {
        cipher_suites: SUITES
            .iter()
            .filter_map(|suite| {
                base.cipher_suites
                    .iter()
                    .find(|s| &s.suite() == suite)
                    .cloned()
            })
            .collect(),
        ..base
    }
}

pub(crate) fn from_options(keep_alive: &Http2KeepAliveOptions) -> SharedHttpClient {
    SharedHttpClient::new(AwsHttpClient {
        keep_alive: KeepAlive::from_options(keep_alive),
        // The same environment variables the SDK's default client reads
        // (`ProxyConfig::from_env()`, behavior versions since 2025-08-07).
        proxy: Arc::new(Matcher::from_env()),
        connectors: Default::default(),
    })
}

#[derive(Debug, Clone, Copy)]
struct KeepAlive {
    /// `None` disables keep-alive pings.
    ping_interval: Option<Duration>,
    ping_timeout: Duration,
    ping_jitter: f32,
}

impl KeepAlive {
    fn from_options(options: &Http2KeepAliveOptions) -> Self {
        let ping_interval: Duration = options.http2_keep_alive_interval.into();

        Self {
            ping_interval: (!ping_interval.is_zero()).then_some(ping_interval),
            ping_timeout: options.http2_keep_alive_timeout.into(),
            ping_jitter: options.http2_keep_alive_jitter,
        }
    }
}

/// The timeouts the SDK asks for, which vary per operation and so cannot be baked
/// into a single connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Timeouts {
    connect: Option<Duration>,
    read: Option<Duration>,
}

#[derive(Debug)]
struct AwsHttpClient {
    keep_alive: KeepAlive,
    proxy: Arc<Matcher>,
    connectors: DashMap<Timeouts, SharedHttpConnector>,
}

impl HttpClient for AwsHttpClient {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        let timeouts = Timeouts {
            connect: settings.connect_timeout(),
            read: settings.read_timeout(),
        };

        // This runs on every request, always with the same timeouts, so take the
        // shared-lock hit path before entry()'s exclusive shard lock.
        if let Some(connector) = self.connectors.get(&timeouts) {
            return connector.clone();
        }

        self.connectors
            .entry(timeouts)
            .or_insert_with(|| {
                SharedHttpConnector::new(AwsHttpConnector::new(
                    self.keep_alive,
                    timeouts,
                    Arc::clone(&self.proxy),
                ))
            })
            .clone()
    }

    fn connector_metadata(&self) -> Option<ConnectorMetadata> {
        Some(ConnectorMetadata::new(
            "restate-h2",
            Some(Cow::Borrowed("0.4")),
        ))
    }
}

#[derive(Clone)]
struct AwsHttpConnector<C = ConnectTimeout<ProxiedConnector>> {
    pool: Pool<C>,
    read_timeout: Option<Duration>,
    proxy: Arc<Matcher>,
}

impl<C> Debug for AwsHttpConnector<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsHttpConnector")
            .field("read_timeout", &self.read_timeout)
            .finish_non_exhaustive()
    }
}

impl AwsHttpConnector {
    fn new(keep_alive: KeepAlive, timeouts: Timeouts, proxy: Arc<Matcher>) -> Self {
        // The timeout covers proxy tunneling and the TLS handshake as well as the
        // TCP connect, so that a peer that goes silent midway through the handshake
        // is bounded too.
        let connector = ConnectTimeout {
            inner: ProxiedConnector::new(Arc::clone(&proxy)),
            timeout: timeouts.connect,
        };

        Self::with_connector(connector, keep_alive, timeouts.read, proxy)
    }
}

impl<C> AwsHttpConnector<C>
where
    C: Clone + Send + Sync + 'static,
{
    fn with_connector(
        connector: C,
        keep_alive: KeepAlive,
        read_timeout: Option<Duration>,
        proxy: Arc<Matcher>,
    ) -> Self {
        Self {
            pool: pool::PoolBuilder::default()
                .keep_alive_interval(keep_alive.ping_interval)
                .keep_alive_timeout(keep_alive.ping_timeout)
                .keep_alive_interval_jitter(keep_alive.ping_jitter)
                // Smithy may construct connectors outside a Tokio runtime. The
                // pool's idle evictor is the only part that requires one here.
                .idle_connection_timeout(None)
                .build(connector),
            read_timeout,
            proxy,
        }
    }
}

impl<C> HttpConnector for AwsHttpConnector<C>
where
    C: Service<Uri> + Clone + Debug + Send + Sync + 'static,
    C::Response:
        tokio::io::AsyncRead + tokio::io::AsyncWrite + Debug + Unpin + Send + Sync + 'static,
    C::Future: Send + 'static,
    C::Error: Into<pool::Error>,
{
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let pool = self.pool.clone();
        let read_timeout = self.read_timeout;

        let mut request = match request.try_into_http1x() {
            Ok(request) => request,
            Err(err) => return HttpConnectorFuture::ready(Err(ConnectorError::user(err.into()))),
        };

        let proxied = self.proxy.intercept(request.uri()).is_some();
        if request.uri().scheme() == Some(&Scheme::HTTP)
            && !request
                .headers()
                .contains_key(http::header::PROXY_AUTHORIZATION)
            && let Some(auth) = self
                .proxy
                .intercept(request.uri())
                .and_then(|intercept| intercept.basic_auth().cloned())
        {
            request
                .headers_mut()
                .insert(http::header::PROXY_AUTHORIZATION, auth);
        }
        let capture = request
            .extensions()
            .get::<CaptureSmithyConnection>()
            .cloned();

        HttpConnectorFuture::new(async move {
            let response = match read_timeout {
                Some(read_timeout) => tokio::time::timeout(read_timeout, pool.request(request))
                    .await
                    .map_err(|_| {
                        ConnectorError::timeout(
                            format!("no response received within {read_timeout:?}").into(),
                        )
                    })?,
                None => pool.request(request).await,
            }
            .map_err(pool_error)?;

            if let Some(capture) = capture {
                let handle = response.body().connection_handle();
                capture.set_connection_retriever(move || {
                    Some(smithy_connection(handle.clone(), proxied))
                });
            }

            let (parts, body) = response.into_parts();
            HttpResponse::try_from(http::Response::from_parts(
                parts,
                SdkBody::from_body_1_x(body),
            ))
            .map_err(|err| ConnectorError::other(err.into(), None))
        })
    }
}

/// Establishes connections directly, or through the proxy configured via the
/// standard environment variables, reproducing the SDK default client's behavior:
/// plain-HTTP requests are forwarded through the proxy (in absolute form, via the
/// proxied-connection marker), and HTTPS is tunneled with `CONNECT` followed by a
/// TLS handshake inside the tunnel.
#[derive(Debug, Clone)]
struct ProxiedConnector {
    https: HttpsConnector<TcpConnector>,
    proxy: Arc<Matcher>,
}

impl ProxiedConnector {
    fn new(proxy: Arc<Matcher>) -> Self {
        let mut tcp_connector = TcpConnector::new();
        tcp_connector.enforce_http(false);
        tcp_connector.set_nodelay(true);

        // Not https_only: an endpoint override, such as a local test double, may be
        // plain HTTP.
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(AWS_TLS_CONFIG.clone())
            .https_or_http()
            .enable_http2()
            .wrap_connector(tcp_connector);

        Self { https, proxy }
    }
}

type HttpsStream = MaybeHttpsStream<TokioIo<TcpStream>>;

impl Service<Uri> for ProxiedConnector {
    type Response = TokioIo<ProxiedConnection>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.https.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let Some(intercept) = self.proxy.intercept(&dst) else {
            let connect = self.https.call(dst);
            return Box::pin(async move {
                Ok(TokioIo::new(ProxiedConnection::Forward(Box::new(
                    connect.await?,
                ))))
            });
        };

        if dst.scheme() == Some(&Scheme::HTTPS) {
            let mut tunnel = Tunnel::new(intercept.uri().clone(), self.https.clone());
            if let Some(auth) = intercept.basic_auth() {
                tunnel = tunnel.with_auth(auth.clone());
            }

            Box::pin(async move {
                let host = dst.host().ok_or("missing host in URI for TLS handshake")?;
                let server_name = ServerName::try_from(host.to_owned())?;

                let tunneled = tunnel.oneshot(dst.clone()).await?;
                let stream = tokio_rustls::TlsConnector::from(AWS_TUNNEL_TLS_CONFIG.clone())
                    .connect(server_name, TokioIo::new(tunneled))
                    .await?;

                Ok(TokioIo::new(ProxiedConnection::Tunneled {
                    stream: Box::new(TokioIo::new(stream)),
                }))
            })
        } else {
            let connect = self.https.call(intercept.uri().clone());
            Box::pin(async move {
                Ok(TokioIo::new(ProxiedConnection::Forward(Box::new(
                    connect.await?,
                ))))
            })
        }
    }
}

#[derive(Debug)]
enum ProxiedConnection {
    Forward(Box<HttpsStream>),
    Tunneled {
        // Boxed: the TLS session state makes this variant twice the size of the
        // other, and connections are created rarely.
        stream: Box<TokioIo<TlsStream<TokioIo<HttpsStream>>>>,
    },
}

impl hyper::rt::Read for ProxiedConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Forward(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            Self::Tunneled { stream } => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for ProxiedConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Forward(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            Self::Tunneled { stream } => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Forward(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            Self::Tunneled { stream } => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Forward(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            Self::Tunneled { stream } => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

fn smithy_connection(handle: ConnectionHandle, proxied: bool) -> ConnectionMetadata {
    ConnectionMetadata::builder()
        .proxied(proxied)
        .poison_fn(move || handle.close())
        .build()
}

fn pool_error(err: pool::Error) -> ConnectorError {
    match err {
        pool::Error::IO(err) if err.kind() == io::ErrorKind::TimedOut => {
            ConnectorError::timeout(err.into())
        }
        pool::Error::IO(err) => ConnectorError::io(err.into()),
        pool::Error::KeepAliveTimeout => ConnectorError::timeout(err.into()),
        pool::Error::Closed => ConnectorError::io(err.into()),
        pool::Error::H2(err)
            if err.is_go_away()
                || (err.is_reset() && err.reason() == Some(h2::Reason::REFUSED_STREAM)) =>
        {
            ConnectorError::io(err.into())
        }
        err => ConnectorError::other(err.into(), None),
    }
}

/// Bounds how long establishing a connection may take, handshake included.
#[derive(Debug, Clone)]
struct ConnectTimeout<C> {
    inner: C,
    timeout: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
#[error("could not connect within {0:?}")]
struct ConnectTimeoutError(Duration);

impl<C> Service<Uri> for ConnectTimeout<C>
where
    C: Service<Uri>,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    type Response = C::Response;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map_err(|err| io::Error::other(err.into()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connect = self.inner.call(uri);
        let timeout = self.timeout;

        Box::pin(async move {
            let Some(timeout) = timeout else {
                return connect.await.map_err(|err| io::Error::other(err.into()));
            };

            tokio::time::timeout(timeout, connect)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, ConnectTimeoutError(timeout)))?
                .map_err(|err| io::Error::other(err.into()))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponentsBuilder;
    use bytes::Bytes;
    use googletest::prelude::*;
    use http::{Response, StatusCode};
    // The proxy matcher's name collides with googletest's `Matcher` trait.
    use hyper_util::client::proxy::matcher::Matcher as ProxyMatcher;

    use restate_util_time::{FriendlyDuration, NonZeroFriendlyDuration};

    use super::*;

    fn keep_alive(interval: Option<Duration>) -> KeepAlive {
        KeepAlive {
            ping_interval: interval,
            ping_timeout: Duration::from_millis(50),
            ping_jitter: 0.0,
        }
    }

    fn no_proxy() -> Arc<ProxyMatcher> {
        Arc::new(ProxyMatcher::builder().build())
    }

    fn invoke_request() -> HttpRequest {
        HttpRequest::try_from(
            http::Request::builder()
                .uri("https://lambda.example/invoke")
                .body(SdkBody::empty())
                .unwrap(),
        )
        .unwrap()
    }

    /// Serves h2 connections that go silent after accepting a request - answering
    /// neither the response nor the pings - until `recover` is set, after which
    /// requests are answered normally. This is the incident's connection as hyper
    /// saw it, followed by a healthy replacement.
    #[derive(Clone, Debug)]
    struct StallingPeer {
        connections: Arc<AtomicUsize>,
        recover: Arc<AtomicBool>,
        release_stalled_peer: Arc<tokio::sync::Notify>,
    }

    impl Service<Uri> for StallingPeer {
        type Response = tokio::io::DuplexStream;
        type Error = io::Error;
        type Future =
            Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _uri: Uri) -> Self::Future {
            self.connections.fetch_add(1, Ordering::Relaxed);
            let recover = Arc::clone(&self.recover);
            let release_stalled_peer = Arc::clone(&self.release_stalled_peer);

            Box::pin(async move {
                let (client, server) = tokio::io::duplex(64 * 1024);
                tokio::spawn(async move {
                    let mut server = h2::server::Builder::new()
                        .handshake::<_, Bytes>(server)
                        .await
                        .unwrap();

                    while let Some(request) = server.accept().await {
                        let (_request, mut respond) = request.unwrap();
                        if !recover.load(Ordering::Relaxed) {
                            release_stalled_peer.notified().await;
                            return;
                        }
                        let response = Response::builder().status(StatusCode::OK).body(()).unwrap();
                        respond.send_response(response, true).unwrap();
                    }
                });
                Ok(client)
            })
        }
    }

    /// The regression this client exists to prevent: a connection that goes silent
    /// mid-flight must fail its in-flight request on the keep-alive timeout instead
    /// of hanging until the kernel gives up on the socket, and the next request must
    /// then succeed on a fresh connection.
    #[tokio::test]
    async fn silent_connection_fails_fast_and_the_next_request_recovers() {
        let peer = StallingPeer {
            connections: Arc::new(AtomicUsize::new(0)),
            recover: Arc::new(AtomicBool::new(false)),
            release_stalled_peer: Arc::new(tokio::sync::Notify::new()),
        };
        let connector = AwsHttpConnector::with_connector(
            peer.clone(),
            keep_alive(Some(Duration::from_millis(50))),
            None,
            no_proxy(),
        );

        let started = tokio::time::Instant::now();
        let stalled =
            tokio::time::timeout(Duration::from_secs(10), connector.call(invoke_request()))
                .await
                .expect("the keep-alive timeout must end the request");

        assert!(stalled.as_ref().is_err_and(|err| err.is_timeout()));
        assert_that!(started.elapsed(), lt(Duration::from_secs(5)));
        assert_that!(peer.connections.load(Ordering::Relaxed), eq(1));

        peer.recover.store(true, Ordering::Relaxed);
        peer.release_stalled_peer.notify_waiters();
        let recovered =
            tokio::time::timeout(Duration::from_secs(10), connector.call(invoke_request()))
                .await
                .expect("the retry must not hang");

        assert_that!(
            recovered.map(|response| response.status().as_u16()),
            ok(eq(200))
        );
        assert_that!(peer.connections.load(Ordering::Relaxed), eq(2));
    }

    #[tokio::test]
    async fn smithy_can_poison_the_connection_used_by_a_response() {
        let peer = StallingPeer {
            connections: Arc::new(AtomicUsize::new(0)),
            recover: Arc::new(AtomicBool::new(true)),
            release_stalled_peer: Arc::new(tokio::sync::Notify::new()),
        };
        let connector =
            AwsHttpConnector::with_connector(peer.clone(), keep_alive(None), None, no_proxy());

        let capture = CaptureSmithyConnection::new();
        let mut request = invoke_request();
        request.add_extension(capture.clone());
        let response = connector.call(request).await.unwrap();
        assert_that!(response.status().as_u16(), eq(200));

        capture.get().expect("connection metadata").poison();
        drop(response);

        let response = connector.call(invoke_request()).await.unwrap();
        assert_that!(response.status().as_u16(), eq(200));
        assert_that!(peer.connections.load(Ordering::Relaxed), eq(2));
    }

    #[derive(Clone, Debug, Default)]
    struct DropsFirstRequest {
        connections: Arc<AtomicUsize>,
    }

    impl Service<Uri> for DropsFirstRequest {
        type Response = tokio::io::DuplexStream;
        type Error = io::Error;
        type Future =
            Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _uri: Uri) -> Self::Future {
            let connection = self.connections.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                let (client, server) = tokio::io::duplex(64 * 1024);
                tokio::spawn(async move {
                    let mut server = h2::server::handshake(server).await.unwrap();
                    let Some(request) = server.accept().await else {
                        return;
                    };
                    let (_request, mut respond) = request.unwrap();
                    if connection == 0 {
                        return;
                    }
                    respond.send_response(Response::new(()), true).unwrap();
                    while server.accept().await.is_some() {}
                });
                Ok(client)
            })
        }
    }

    /// Unlike Hyper's legacy client, the H2 pool does not replay a request whose
    /// reused connection disappears before returning response headers.
    #[tokio::test]
    async fn canceled_requests_are_not_transparently_replayed() {
        let peer = DropsFirstRequest::default();
        let connector =
            AwsHttpConnector::with_connector(peer.clone(), keep_alive(None), None, no_proxy());

        assert_that!(connector.call(invoke_request()).await, err(anything()));
        assert_that!(peer.connections.load(Ordering::Relaxed), eq(1));

        let response = connector.call(invoke_request()).await.unwrap();
        assert_that!(response.status().as_u16(), eq(200));
        assert_that!(peer.connections.load(Ordering::Relaxed), eq(2));
    }

    #[tokio::test]
    async fn connect_timeout_covers_the_tls_handshake() {
        // Accepted by the kernel, but the handshake never completes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connector = AwsHttpConnector::new(
            keep_alive(None),
            Timeouts {
                connect: Some(Duration::from_millis(100)),
                read: None,
            },
            no_proxy(),
        );

        let request = HttpRequest::try_from(
            http::Request::builder()
                .uri(format!("https://{addr}/"))
                .body(SdkBody::empty())
                .unwrap(),
        )
        .unwrap();

        let started = tokio::time::Instant::now();
        let result = connector.call(request).await;
        assert!(result.as_ref().is_err_and(|err| err.is_timeout()));
        // The connect itself succeeds, so reaching the timeout is what ends the
        // attempt - and it must end there rather than hanging on the handshake.
        assert_that!(
            started.elapsed(),
            all!(ge(Duration::from_millis(100)), lt(Duration::from_secs(5)))
        );
    }

    /// Plain-HTTP forwarding remains possible when the proxy itself supports H2.
    #[tokio::test]
    async fn plain_http_requests_are_forwarded_through_an_h2_proxy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        let received_uri = Arc::new(Mutex::new(None));
        let captured_uri = Arc::clone(&received_uri);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut server = h2::server::handshake(stream).await.unwrap();
            while let Some(request) = server.accept().await {
                let (request, mut respond) = request.unwrap();
                *captured_uri.lock().unwrap() = Some(request.uri().clone());
                respond.send_response(Response::new(()), true).unwrap();
            }
        });

        let proxy = ProxyMatcher::builder()
            .http(format!("http://{proxy_addr}"))
            .build();
        let connector = AwsHttpConnector::new(
            keep_alive(None),
            Timeouts {
                connect: Some(Duration::from_secs(3)),
                read: None,
            },
            Arc::new(proxy),
        );

        let request = HttpRequest::try_from(
            http::Request::builder()
                .uri("http://lambda.example/invoke")
                .body(SdkBody::empty())
                .unwrap(),
        )
        .unwrap();

        let response = tokio::time::timeout(Duration::from_secs(10), connector.call(request))
            .await
            .expect("the proxied request must not hang");

        assert_that!(
            response.map(|response| response.status().as_u16()),
            ok(eq(200))
        );
        assert_that!(
            received_uri.lock().unwrap().as_ref(),
            some(eq(&"http://lambda.example/invoke".parse::<Uri>().unwrap()))
        );
    }

    #[test]
    fn connectors_are_cached_per_timeout_setting() {
        let client = AwsHttpClient {
            keep_alive: KeepAlive::from_options(&Http2KeepAliveOptions::default()),
            proxy: no_proxy(),
            connectors: Default::default(),
        };
        let components = RuntimeComponentsBuilder::for_tests().build().unwrap();

        let settings = HttpConnectorSettings::builder()
            .connect_timeout(Duration::from_secs(3))
            .build();
        client.http_connector(&settings, &components);
        client.http_connector(&settings, &components);
        assert_that!(client.connectors.len(), eq(1));

        let other = HttpConnectorSettings::builder()
            .connect_timeout(Duration::from_secs(5))
            .build();
        client.http_connector(&other, &components);
        assert_that!(client.connectors.len(), eq(2));
    }

    #[test]
    fn a_zero_interval_disables_pings() {
        let disabled = KeepAlive::from_options(&Http2KeepAliveOptions {
            http2_keep_alive_interval: FriendlyDuration::ZERO,
            http2_keep_alive_timeout: NonZeroFriendlyDuration::from_secs_unchecked(20),
            http2_keep_alive_jitter: 0.2,
        });
        assert_that!(disabled.ping_interval, none());

        let default = KeepAlive::from_options(&Http2KeepAliveOptions::default());
        assert_that!(default.ping_interval, some(eq(Duration::from_secs(40))));
        assert_that!(default.ping_timeout, eq(Duration::from_secs(20)));
    }

    /// Guards the SDK-parity TLS parameters: the curated cipher-suite list, and
    /// ALPN offered only on the in-tunnel handshake (the direct path gets ALPN
    /// from hyper-rustls).
    #[test]
    fn tls_configs_match_the_sdk_parameters() {
        let suites: Vec<_> = AWS_TLS_CONFIG
            .crypto_provider()
            .cipher_suites
            .iter()
            .map(|suite| suite.suite())
            .collect();
        assert_eq!(
            suites,
            [
                CipherSuite::TLS13_AES_256_GCM_SHA384,
                CipherSuite::TLS13_AES_128_GCM_SHA256,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
            ]
        );

        assert_that!(AWS_TLS_CONFIG.alpn_protocols, empty());
        assert_eq!(AWS_TUNNEL_TLS_CONFIG.alpn_protocols, [b"h2".to_vec()]);
    }
}
