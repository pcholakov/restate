# Release Notes: Experimental V0 Cluster Backup and Recovery

## New Feature

### What Changed

`restatectl snapshots backup` and `restatectl snapshots restore` introduce an experimental V0
operator workflow for capturing a cluster backup descriptor and recovering it onto a fresh,
unprovisioned bootstrap node.

### Why This Matters

Operators now have a supported CLI workflow for recovery from partition snapshots, including
optional schema capture and restore.

### Impact on Users

- This is a new, user-visible experimental feature.
- Backups are best-effort and are not a cross-partition consistent cut.
- Restore is only supported onto a new, unprovisioned single bootstrap node. If the target has an
  active snapshot destination, it must use a different repository root or prefix from the backup
  source; scale out after recovery completes.
- Restore generates a new cluster fingerprint by default. Preserving the source fingerprint is
  opt-in with `--preserve-cluster-fingerprint`.
- The metadata store is not included, and V0 restore has no in-place restore, rollback, or
  atomicity guarantee.

### Migration Guidance

Before relying on the feature, validate the intended descriptor and target with
`restatectl snapshots restore --dry-run`. Review the V0 recovery procedure in
`docs/dev/cluster-backup-recovery.md`, and retain the backup descriptor and snapshot repository
contents until the recovered cluster has been verified.
