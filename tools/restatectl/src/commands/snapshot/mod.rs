// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

mod backup;
mod create_snapshot;
mod restore;

use cling::prelude::*;

#[derive(Run, Subcommand, Clone)]
#[allow(clippy::enum_variant_names)] // Preserve the existing `create-snapshot` CLI spelling.
pub enum Snapshot {
    /// Capture a V0 cluster backup descriptor.
    Backup(backup::BackupOpts),
    /// Create.
    CreateSnapshot(create_snapshot::CreateSnapshotOpts),
    /// Restore a V0 cluster backup descriptor onto an unprovisioned node.
    Restore(restore::RestoreOpts),
}
