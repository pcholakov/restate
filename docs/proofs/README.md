# Formal Proof: Distributed Snapshot Consistency

This directory contains a TLA+ formal model of Restate's modified Chandy-Lamport
distributed snapshot protocol. The model proves that the protocol guarantees
**message conservation** — no application message is lost during a snapshot —
and **consistent cut** — the recorded global state could have existed during
execution.

## Quick start

```bash
# 2 partitions, 2 messages each (fast — seconds)
java -cp /path/to/tla2tools.jar tlc2.TLC RestateSnapshot

# 3 partitions, 1 message each (~1.8M states, a few seconds)
java -cp /path/to/tla2tools.jar tlc2.TLC RestateSnapshot -config RestateSnapshot3.cfg
```

## Files

| File | Purpose |
|------|---------|
| `RestateSnapshot.tla` | The TLA+ specification |
| `RestateSnapshot.cfg` | TLC config: 2 partitions, 2 messages (safety) |
| `RestateSnapshot3.cfg` | TLC config: 3 partitions, 1 message (safety) |
| `RestateSnapshotLive.cfg` | TLC config: 2 partitions, 2 messages (safety + liveness) |
| `RestateSnapshot3Live.cfg` | TLC config: 3 partitions, 1 message (safety + liveness) |
| `RestateSnapshot3x2.cfg` | TLC config: 3 partitions, 2 messages (safety, ~12 min) |

## What the model proves

Four safety invariants are verified across all reachable states:

1. **TypeOK** — Variable domains are well-formed
2. **NoMessageLost** — Every message in a sender's snapshot outbox is also in
   the receiver's snapshot dedup (or remains in the outbox)
3. **AllSentMessagesCaptured** — Every message sent before the sender's snapshot
   is captured *somewhere* in the global snapshot
4. **ConsistentCut** — If a receiver's snapshot shows a processed message from
   src, then src must have sent that message before src's own snapshot

One liveness (temporal) property is verified under weak fairness:

5. **SnapshotTermination** — The snapshot eventually completes (`<>(phase = "complete")`),
   assuming partition processors eventually process their logs and the coordinator
   eventually acts. No fairness on application message sends — termination holds
   regardless of new traffic.

## Model overview

The rest of this document is a literate walkthrough of the TLA+ spec, designed
to be readable even without TLA+ expertise.

---

### The system

We model N **partition processors** (PPs). Each PP has:

- An **outbox**: messages waiting to be delivered and acknowledged
- A **dedup table**: messages already processed (preventing duplicates)
- A **next sequence number**: monotonically increasing per-partition

PPs communicate through **Bifrost logs** — one FIFO log per partition. To send
a message from P0 to P1, P0 appends to P1's log. P1 reads its own log in order.

```
   P0                          P1
   │                           │
   │── outbox msg ──> Log-P1 ──│
   │                           │── process, add to dedup
   │<── ack ──────── Log-P0 <──│
   │                           │
   │── remove from outbox      │
```

**Key insight**: Acks flow through the *same* FIFO log as application messages
and snapshot markers. This FIFO ordering is what makes the protocol correct.

### The snapshot protocol

A coordinator initiates a snapshot by appending `InitiateSnapshot` to every
partition's log. When a PP reads this (or a snapshot marker from a peer), it:

1. **Gates** outbound messages (queues them, doesn't send)
2. **Flushes** its self-loop channel (drains pending self-commands)
3. **Takes a checkpoint** of its RocksDB state (outbox + dedup + nextSeq)
4. **Sends markers** to all other partitions via their Bifrost logs
5. **Ungates** outbound messages

Steps 1–5 are **atomic** in the model — no application messages can escape
between the checkpoint and the marker send. This models the real implementation's
message gate.

When a PP has taken its snapshot AND received markers from all other PPs, it
reports completion. The snapshot is complete when all PPs have done so.

### Why it works: the FIFO argument

Consider a message M from P0 to P1:

**Case A: P1 processes M before P1's snapshot**
→ M is in P1's dedup at snapshot time. Conservation holds.

**Case B: P1 has NOT processed M at P1's snapshot time**
→ P0 has not received an ack for M (because P1 hasn't sent one yet).
→ M is still in P0's outbox.

But we need to know M is in P0's *snapshot* outbox (not just outbox later).
This is where FIFO + gating matters:

- P1 can only send an ack *after* processing M
- If P1 processes M after P1's snapshot, P1 is gated: the ack is queued
  behind P1's marker to P0
- FIFO ordering on Log-P0 means: marker arrives at P0 before the ack
- P0's snapshot happens before or when P0 sees P1's marker
- Therefore: ack arrives at P0 *after* P0's snapshot
- Therefore: M is still in P0's outbox at P0's snapshot time ✓

**Case C (N>2 partitions only): M sent by P2, received by P1 after P1's snapshot but before P2's marker arrives at P1**

This is the tricky case with 3+ partitions. P0 initiates the snapshot. P1
snapshots when it sees P0's marker. Later, P2 sends M to P1. P1 processes M
after its snapshot.

The argument is the same as Case B:
- P1 gates outbound messages when it snapshots
- P1's marker to P2 goes out before any acks
- P2's ack for messages from P1 would arrive at P1 after P1's marker...

Wait — M goes from P2 to P1, not from P1 to P2. Let's be precise:
- P2 has M in its outbox (sent to P1)
- P1 processes M, sends ack to P2 via Log-P2
- But P1 is gated (or has already ungated after sending markers)
- If P1 sends the ack after P1's markers: the ack appears in Log-P2
  after P1's marker in Log-P2 (FIFO on Log-P2)
- P2 sees P1's marker first, triggering P2's snapshot (or P2 already snapshotted)
- If P2 hasn't snapshotted yet: P2 snapshots on P1's marker, THEN processes
  the ack → M is still in P2's snapshot outbox ✓
- If P2 already snapshotted: M was already in P2's outbox at snapshot time
  (the ack hadn't arrived yet, so M hadn't been truncated) ✓

Either way, M is captured somewhere.

### Assumptions

The model explicitly encodes these assumptions (labeled A1–A7 in the spec):

| # | Assumption | Justification |
|---|-----------|---------------|
| A1 | Bifrost logs are FIFO and durable | LSN-ordered append-only logs |
| A2 | One appender per destination per partition | Single shuffle task per PP |
| A3 | Self-loop is drained before checkpoint | SelfProposer flush + wait for applied_lsn == target_lsn |
| A4 | Messages gated between snapshot and marker send | Message gate in leadership/mod.rs |
| A5 | One snapshot at a time | Coordinator constraint |
| A6 | Acks flow through Bifrost | Shuffle appends acks to source partition's log |
| A7 | Acks sent even for duplicates | Critical for post-restore outbox truncation |

### What the model does NOT cover

- **Self-loop channel dynamics**: Modeled as an atomic precondition (drain
  before snapshot), not as actual self-loop traffic. The drain correctness
  argument is: single SelfProposer ensures FIFO, flush waits for all queued
  commands to commit, then we wait for applied_lsn to catch up.

- **Partial failures**: Marker fanout is atomic per partition. In practice,
  if a PP crashes between sending some markers and others, the snapshot for
  that partition is never completed and the coordinator times out.

- **External channels** (invoker ↔ SDK): Excluded because the Restate protocol
  is designed for unreliable channels with idempotent replay.

- **Multiple snapshot rounds**: The model runs one snapshot. The single-snapshot
  constraint (A5) means this is sufficient.

### Model checking results

All configs use symmetry reduction (`Permutations(Partition)`) to collapse
equivalent states under partition renaming, reducing the state space by `N!`.

| Configuration | Properties | States | Distinct | Depth | Time | Result |
|--------------|------------|--------|----------|-------|------|--------|
| 2 partitions, 2 msgs | safety | 9,135 | 4,407 | 19 | <1s | PASS |
| 3 partitions, 1 msg | safety | 296,157 | 103,755 | 21 | ~1s | PASS |
| 3 partitions, 2 msgs | safety | 482,539,039 | 151,926,474 | 30 | ~12m | PASS |
| 2 partitions, 2 msgs | safety + liveness | 9,135 | 4,407 | 19 | <1s | PASS |
| 3 partitions, 1 msg | safety + liveness | 296,157 | 103,755 | 21 | ~1s | PASS |

All safety invariants and the liveness property hold across all reachable
states.

### Relationship to existing docs

This formal model supersedes the informal proofs in:
- `docs/distributed-snapshot-walkthrough.txt` — 2-partition walkthrough
- `docs/cross-partition-invocation-walkthrough.txt` — message flow details
- `docs/dst-snapshot-implementation-sketch.md` — simulation test design

The walkthroughs remain valuable as intuition-builders. The TLA+ model provides
machine-checked verification of the core invariants they argue for.

---

## FAQ

### In what ways have we modified the original Chandy-Lamport distributed snapshot protocol for Restate?

**1. No explicit channel state recording**

Classical CL records "channel state" for each incoming channel: all messages
received after the local snapshot but before the marker arrives on that channel.
We don't record channel state at all. Instead, the outbox/dedup pair implicitly
captures it — a message is always in the sender's outbox until the receiver acks
it through Bifrost. The outbox *is* the channel state.

**2. Acks flow through Bifrost (same FIFO channel as markers)**

Classical CL doesn't have acks — it just records what's in-flight. We need acks
because the sender truncates its outbox. By routing acks through the same FIFO
log as markers, we get a critical ordering guarantee: if a receiver processes a
message and sends an ack *after* sending its marker, the ack arrives at the
sender *after* the marker, so the sender hasn't truncated the message at snapshot
time. This is assumption A6 and the heart of the correctness argument.

**3. Message gate between snapshot and marker send**

Classical CL says "send markers before any more messages." We enforce this with
an explicit gate that queues outbound messages until markers are sent. The TLA+
model captures this as atomicity of the snapshot+marker action (A4).

**4. Self-loop channel handling**

Classical CL doesn't consider self-loops. Our PPs write commands to their own
Bifrost log via SelfProposer, creating a channel from a process to itself. We
drain this channel (flush + wait for applied_lsn == target_lsn) before taking
the checkpoint, ensuring no in-flight self-commands are missed. Modeled as A3.

**5. External channels excluded**

Classical CL would need to snapshot every channel. We exclude invoker/SDK
channels entirely because the Restate protocol is designed for unreliable
channels with idempotent replay. On restore, the invoker resends; the SDK
journal handles duplicates.

**6. Single snapshot at a time**

Classical CL allows concurrent snapshots (distinguished by snapshot ID). We
restrict to one active snapshot (A5), simplifying the state machine — a PP
that's mid-snapshot ignores new initiation requests.

**7. Coordinator-initiated, not process-initiated**

Classical CL allows any process to spontaneously initiate. We use a dedicated
coordinator (cluster controller) that appends `InitiateSnapshot` to every
partition's log. This gives us a single point of orchestration for tracking
completion.
