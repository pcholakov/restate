// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};

pub const NETWORK_CONNECTION_CREATED: &str = "restate.network.connection_created.total";
pub const NETWORK_CONNECTION_DROPPED: &str = "restate.network.connection_dropped.total";
pub const NETWORK_SERVICE_ACCEPTED_REQUEST_BYTES: &str =
    "restate.network.service.accepted_request_bytes.total";
pub const NETWORK_SERVICE_REJECTED_REQUEST_BYTES: &str =
    "restate.network.service.rejected_request_bytes.total";

pub const NETWORK_MESSAGE_PROCESSING_DURATION: &str =
    "restate.network.message_processing_duration.seconds";

/// Diagnostic gauges for [`super::PinGuard`], used to find which structure pins
/// tonic's per-stream decode buffer by retaining zero-copy fabric `Bytes` slices.
/// Labeled by `site`, the retention point where the `Bytes` is held.
pub const FABRIC_HELD_BYTES: &str = "restate_fabric_held_bytes";
pub const FABRIC_HELD_COUNT: &str = "restate_fabric_held_count";

pub fn describe_metrics() {
    describe_counter!(
        NETWORK_CONNECTION_CREATED,
        Unit::Count,
        "Number of connections created"
    );
    describe_counter!(
        NETWORK_CONNECTION_DROPPED,
        Unit::Count,
        "Number of connections dropped"
    );
    describe_counter!(
        NETWORK_SERVICE_ACCEPTED_REQUEST_BYTES,
        Unit::Bytes,
        "Number of bytes accepted by service name"
    );

    describe_counter!(
        NETWORK_SERVICE_REJECTED_REQUEST_BYTES,
        Unit::Bytes,
        "Number of bytes received and dropped/rejected by service name"
    );

    describe_histogram!(
        NETWORK_MESSAGE_PROCESSING_DURATION,
        Unit::Seconds,
        "Latency of deserializing and processing incoming messages"
    );

    describe_gauge!(
        FABRIC_HELD_BYTES,
        Unit::Bytes,
        "Bytes of fabric gRPC payloads currently held, by retention site"
    );
    describe_gauge!(
        FABRIC_HELD_COUNT,
        Unit::Count,
        "Number of fabric gRPC payloads currently held, by retention site"
    );
}
