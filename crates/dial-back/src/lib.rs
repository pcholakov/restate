// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Rendezvous between an invoker attempt and the HTTP/2 "attach" request that the service
//! dials back with (experimental lease + dial-back invocations).
//!
//! The invoker registers a one-shot token before sending the lease request to the service. The
//! ingress `POST /restate/attach` route redeems the token and hands the attach request (head and
//! body) to the waiting invoker task, then serves whatever response the invoker gives back.
//!
//! prototype: the registry is a process-global, which is only correct on a single node and
//! avoids plumbing a handle through the node/worker/ingress constructors.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};

use bytes::Bytes;
use http::{HeaderName, Request, Response, StatusCode, request};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use tokio::sync::oneshot;
use tracing::debug;

/// Header, on the lease request, carrying the URL the service must dial back to.
pub const ATTACH_URL_HEADER: HeaderName = HeaderName::from_static("x-restate-attach-url");
/// Header, on both the lease and the attach request, carrying the rendezvous token.
pub const ATTACH_TOKEN_HEADER: HeaderName = HeaderName::from_static("x-restate-attach-token");
/// Deployment `additional_headers` marker that switches a deployment to lease + dial-back.
pub const DIAL_BACK_MARKER_HEADER: HeaderName =
    HeaderName::from_static("x-restate-experimental-dial-back");
/// Path of the attach route on the ingress server.
pub const ATTACH_PATH: &str = "/restate/attach";

pub type AttachResponseBody = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// An attach request that reached the ingress, waiting for the invoker to say what to answer.
pub struct Attach {
    pub parts: request::Parts,
    pub body: Incoming,
    /// The response served on the attach stream. Its body carries Restate -> service messages.
    pub respond: oneshot::Sender<Response<AttachResponseBody>>,
}

static REGISTRY: LazyLock<Mutex<HashMap<String, oneshot::Sender<Attach>>>> =
    LazyLock::new(Default::default);

/// A pending attach. Dropping it removes the registry entry, so a token never outlives its attempt.
pub struct Registration {
    token: String,
}

impl Registration {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut registry = REGISTRY.lock().expect("registry lock poisoned");
        registry.remove(&self.token);
        debug!(
            registry_size = registry.len(),
            "Dial-back registration dropped"
        );
    }
}

/// Registers a fresh token. The receiver resolves when the service attaches.
pub fn register() -> (Registration, oneshot::Receiver<Attach>) {
    let bytes: [u8; 32] = rand::random();
    let mut token = String::with_capacity(64);
    for b in bytes {
        write!(token, "{b:02x}").expect("writing to a String cannot fail");
    }
    let (tx, rx) = oneshot::channel();
    let mut registry = REGISTRY.lock().expect("registry lock poisoned");
    registry.insert(token.clone(), tx);
    debug!(registry_size = registry.len(), "Dial-back registered");
    (Registration { token }, rx)
}

pub fn registry_size() -> usize {
    REGISTRY.lock().expect("registry lock poisoned").len()
}

fn status_response(status: StatusCode) -> Response<AttachResponseBody> {
    Response::builder()
        .status(status)
        .body(
            Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("static response is valid")
}

/// Serve `POST /restate/attach`. Unknown, replayed or expired tokens get 410.
pub async fn serve_attach(req: Request<Incoming>) -> Response<AttachResponseBody> {
    let (parts, body) = req.into_parts();
    let Some(token) = parts
        .headers
        .get(&ATTACH_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return status_response(StatusCode::BAD_REQUEST);
    };

    let waiting = REGISTRY
        .lock()
        .expect("registry lock poisoned")
        .remove(&token);
    let Some(waiting) = waiting else {
        debug!("Attach with unknown or expired token");
        return status_response(StatusCode::GONE);
    };

    let (respond, response) = oneshot::channel();
    if waiting
        .send(Attach {
            parts,
            body,
            respond,
        })
        .is_err()
    {
        debug!("Attach for an attempt that already ended");
        return status_response(StatusCode::GONE);
    }

    response
        .await
        .unwrap_or_else(|_| status_response(StatusCode::GONE))
}
