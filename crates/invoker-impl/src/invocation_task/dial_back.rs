// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Lease + dial-back invocations (experimental).
//!
//! The service receives a short, complete request (the lease) and dials back to the ingress with
//! an HTTP/2 attach request, on which the normal service protocol runs. This module races the
//! lease response against the attach so the rest of the invocation task keeps consuming a plain
//! `Response<ResponseBody>`.
//!
//! Ordering rule: a healthy service ends the attach request body before it answers the lease.
//! The attach stream is therefore authoritative for the attempt outcome, and the lease only
//! matters when it ends the attempt early.

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{HeaderName, HeaderValue, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::time::Instant;
use tracing::{debug, warn};

use restate_dial_back::{Attach, Registration};
use restate_service_client::{HttpResponseBody, ResponseBody, ServiceClientError};
use restate_types::identifiers::InvocationId;

use super::InvokerBodyType;

/// How long the service has to dial back after the lease was sent.
const ATTACH_DEADLINE: Duration = Duration::from_secs(30);

/// How long, after a successful lease response, the attach request body may still be open.
/// The service ends the attach before answering the lease, but over different connections.
const LEASE_GRACE: Duration = Duration::from_secs(1);

/// Upper bound on waiting for the lease response after the attach ended, only used for logging.
const LEASE_LOG_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn is_dial_back(additional_headers: &HashMap<HeaderName, HeaderValue>) -> bool {
    additional_headers
        .get(&restate_dial_back::DIAL_BACK_MARKER_HEADER)
        .is_some_and(|v| v == "1")
}

/// Resolves with the response the invocation task should process: the attach (with a synthesized
/// 200 status) if the service dials back, otherwise the lease response or a failure status.
///
/// `attach_ended` is dropped by the invocation task once the attach request body was consumed
/// (or the task is gone); until then the lease finishing means the service died.
pub(super) async fn lease_or_attach(
    invocation_id: InvocationId,
    lease: impl Future<Output = Result<Response<ResponseBody>, ServiceClientError>> + Send + 'static,
    registration: Registration,
    attach: oneshot::Receiver<Attach>,
    invoker_body: InvokerBodyType,
    attach_ended: oneshot::Receiver<()>,
) -> Result<Response<ResponseBody>, ServiceClientError> {
    // Removes the registry entry however this function ends.
    let _registration = registration;
    let started = Instant::now();
    let mut lease = Box::pin(lease);

    let attach = tokio::select! {
        biased;
        attach = attach => attach,
        lease_result = &mut lease => {
            log_lease(&invocation_id, started, &lease_result, "before attach");
            return lease_result;
        }
        _ = tokio::time::sleep(ATTACH_DEADLINE) => {
            warn!(restate.invocation.id = %invocation_id, "Service did not attach within {ATTACH_DEADLINE:?}");
            return Ok(failure_response(StatusCode::GATEWAY_TIMEOUT));
        }
    };
    let Ok(Attach {
        parts,
        body,
        respond,
    }) = attach
    else {
        return Ok(failure_response(StatusCode::BAD_GATEWAY));
    };
    debug!(
        restate.invocation.id = %invocation_id,
        attach_latency = ?started.elapsed(),
        "Service attached"
    );

    let (abort_tx, abort_rx) = oneshot::channel();
    let _ = respond.send(Response::new(
        AbortableBody {
            inner: invoker_body,
            abort: Some(abort_rx),
        }
        .boxed_unsync(),
    ));
    tokio::spawn(watch_lease(
        invocation_id,
        lease,
        attach_ended,
        abort_tx,
        started,
    ));

    // A request has no status line; the attach headers carry content-type and x-restate-server.
    let mut response = Response::new(ResponseBody::Left(HttpResponseBody::from(body)));
    *response.headers_mut() = parts.headers;
    Ok(response)
}

async fn watch_lease(
    invocation_id: InvocationId,
    mut lease: BoxFuture<'static, Result<Response<ResponseBody>, ServiceClientError>>,
    mut attach_ended: oneshot::Receiver<()>,
    abort: oneshot::Sender<()>,
    started: Instant,
) {
    tokio::select! {
        _ = &mut attach_ended => {
            if let Ok(lease_result) = tokio::time::timeout(LEASE_LOG_TIMEOUT, lease).await {
                log_lease(&invocation_id, started, &lease_result, "after attach ended");
            }
        }
        lease_result = &mut lease => {
            log_lease(&invocation_id, started, &lease_result, "while attach open");
            let lease_ok = matches!(&lease_result, Ok(r) if r.status().is_success());
            let attach_open = matches!(attach_ended.try_recv(), Err(TryRecvError::Empty));
            if !attach_open {
                return;
            }
            if lease_ok && tokio::time::timeout(LEASE_GRACE, &mut attach_ended).await.is_ok() {
                return;
            }
            warn!(restate.invocation.id = %invocation_id, "Lease finished while the attach is still open, failing the attempt");
            let _ = abort.send(());
        }
    }
}

fn log_lease(
    invocation_id: &InvocationId,
    started: Instant,
    lease_result: &Result<Response<ResponseBody>, ServiceClientError>,
    phase: &str,
) {
    match lease_result {
        Ok(response) => debug!(
            restate.invocation.id = %invocation_id,
            lease.status = response.status().as_u16(),
            lease.latency = ?started.elapsed(),
            registry_size = restate_dial_back::registry_size(),
            "Lease finished {phase}"
        ),
        Err(err) => debug!(
            restate.invocation.id = %invocation_id,
            lease.error = %err,
            lease.latency = ?started.elapsed(),
            registry_size = restate_dial_back::registry_size(),
            "Lease failed {phase}"
        ),
    }
}

fn failure_response(status: StatusCode) -> Response<ResponseBody> {
    let mut response = Response::new(ResponseBody::Right(http_body_util::Full::new(
        Bytes::new(),
    )));
    *response.status_mut() = status;
    response
}

/// The attach response body. Errors on demand so that hyper resets the attach stream, which the
/// invoker then observes as a body error on the attach request.
struct AbortableBody {
    inner: InvokerBodyType,
    /// `None` once the abort channel resolved without an abort.
    abort: Option<oneshot::Receiver<()>>,
}

impl Body for AbortableBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if let Some(abort) = self.abort.as_mut()
            && let Poll::Ready(result) = Pin::new(abort).poll(cx)
        {
            self.abort = None;
            if result.is_ok() {
                return Poll::Ready(Some(Err("lease finished before the attach ended".into())));
            }
        }
        Pin::new(&mut self.inner)
            .poll_frame(cx)
            .map_err(|never| match never {})
    }
}
