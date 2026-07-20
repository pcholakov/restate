// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use metrics::gauge;

use super::metric_definitions::{FABRIC_HELD_BYTES, FABRIC_HELD_COUNT};

/// RAII tracker for a zero-copy fabric `Bytes` slice held beyond the point it was
/// received.
///
/// Fabric gRPC payloads are zero-copy slices into tonic's single per-stream
/// `BytesMut` decode buffer: holding on to any slice pins the whole (potentially
/// multi-MB) buffer generation alive. Attach a `PinGuard` for the lifetime of every
/// `Bytes` handed off past the point it's received, so that `restate_fabric_held_bytes`
/// and `restate_fabric_held_count`, broken down by `site`, reveal which retention
/// point is responsible for a growing pinned-memory footprint.
#[derive(Debug)]
pub struct PinGuard {
    site: &'static str,
    len: usize,
}

impl PinGuard {
    /// Marks `len` bytes as held at `site`. `site` should be a fixed, low-cardinality
    /// label identifying the retaining structure (e.g. `"rpc_reply"`).
    pub fn new(site: &'static str, len: usize) -> Self {
        gauge!(FABRIC_HELD_BYTES, "site" => site).increment(len as f64);
        gauge!(FABRIC_HELD_COUNT, "site" => site).increment(1.0);
        Self { site, len }
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        gauge!(FABRIC_HELD_BYTES, "site" => self.site).decrement(self.len as f64);
        gauge!(FABRIC_HELD_COUNT, "site" => self.site).decrement(1.0);
    }
}
