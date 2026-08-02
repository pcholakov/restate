# V0 cluster backup and recovery

V0 cluster backup and recovery is an experimental, operator-driven procedure for recovering
partition state onto a new Restate cluster. It writes a portable JSON backup descriptor and
restores the descriptor's partition topology and snapshots onto one bootstrap node.

This procedure is intended for recovery, not for cloning a live cluster or performing an
in-place restore.

## Create a backup descriptor

The normal mode captures the current partition topology and creates one exact snapshot per
partition. The descriptor records the resulting snapshot artifacts.

```bash
restatectl snapshots backup --output cluster-backup.json --snapshot-repository s3://my-bucket/restate-snapshots
```

Use `--include-schema` to embed the current global schema in the descriptor. This does not
restore the schema unless `--restore-schema` is also passed during restore.

```bash
restatectl snapshots backup --output cluster-backup.json --snapshot-repository s3://my-bucket/restate-snapshots --include-schema
```

Alternatively, `--use-latest` writes a topology-only descriptor. During restore, each
partition resolves the latest snapshot available in the repository rather than the exact
snapshot captured by the backup command. Use this mode only when that behavior is intended.

```bash
restatectl snapshots backup --output cluster-backup.json --snapshot-repository s3://my-bucket/restate-snapshots --use-latest
```

The repository URL must identify the repository configured on the source workers. Query
parameters are not recorded in the descriptor. The command verifies the configured repository
reported by every newly created snapshot before writing a complete exact descriptor. Exact backup
artifacts are protected from automatic rolling snapshot retention. V0 has no delete or unprotect
command, so protected artifacts accumulate. Do not delete their raw objects while `latest.json`
still references them; removing a backup requires a coordinated repository metadata update and
object cleanup. Topology-only capture cannot verify the operator-supplied repository because it
does not contact a worker to create a snapshot. If snapshot creation fails for any partition, the
descriptor is incomplete and cannot be restored; inspect it and retry the backup.

Protection upgrades each affected partition's `latest.json` pointer to format V3. Before creating
the first exact backup, upgrade every snapshot-writing worker that shares the repository to a build
which understands V3. An older or downgraded worker fails closed when it encounters V3 rather than
rewriting the pointer without its protection markers; it cannot write more snapshots to that
partition until upgraded again. Rolling back the repository pointer format is not supported.

## Restore onto a new bootstrap node

Restore only to a fresh, unprovisioned node, addressed explicitly with `--single-address`. The
node reads the descriptor's source repository for the duration of restore. Its configured snapshot
destination remains the new cluster's active repository and, when configured, must use a different
URL root or prefix from the source cluster. Restore rejects an identical source and target repository
after removing URL query parameters and non-root trailing slashes. It becomes the single bootstrap node
containing all restored partitions;
scale out the cluster only after restore has completed. Disable `auto-provision` on the bootstrap
node. The V0 bootstrap must run the worker and built-in metadata-server roles, plus the log-server
role when using replicated logs. A forced node ID, when configured, must be the initial node ID.
For a large repository, configure `initialization-timeout` long enough for every snapshot to
download and import before node startup times out (the default is five minutes). The bootstrap node
needs object-store credentials capable of reading the source repository and writing its configured
target repository.

First validate the descriptor and target without changing the target node:

```bash
restatectl --single-address http://bootstrap-node:5122 snapshots restore --input cluster-backup.json --dry-run
```

Then perform the restore:

```bash
restatectl --single-address http://bootstrap-node:5122 snapshots restore --input cluster-backup.json
```

Restoring generates a fresh cluster fingerprint by default. Use
`--preserve-cluster-fingerprint` only when the descriptor contains a source fingerprint and
the restored cluster must retain it.

To restore schema metadata captured by `--include-schema`, add `--restore-schema`:

```bash
restatectl --single-address http://bootstrap-node:5122 snapshots restore --input cluster-backup.json --restore-schema
```

Do not attempt to restore onto an existing or already provisioned cluster. There is no
in-place restore path.

## V0 safety and limitations

V0 backup is best-effort: topology, schema (when included), and partition snapshots are
captured independently. It is not a cross-partition consistent cut. The metadata store itself
is not snapshotted.

Restore is neither atomic nor rollback-capable. The recovered partition topology is published
last to reduce exposure of partially restored state, but a process crash can still leave a
target that must be discarded and recovered again from a fresh target. Keep the original
descriptor and repository data available until recovery is verified.

Topology-only descriptors do not protect the snapshots they eventually resolve. Their restore
result can change as the source repository advances, and rolling retention can make an older cut
unavailable. Prefer exact backup descriptors for durable recovery points.

V1 is planned to add a quiescence-based recovery workflow that can produce a consistent
cluster-wide cut.
