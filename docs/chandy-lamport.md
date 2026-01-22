# Chandy-Lamport algorithm - exact definition

## Model
- Finite set of processes P = {P1..Pn}.
- Directed channels C_ij from Pi to Pj; the channel graph is strongly connected.
- Channels are reliable, FIFO, and unbounded; every message sent is delivered exactly once after finite (but unbounded) delay.
- Processes do not fail during snapshot collection.
- System is asynchronous: no shared clock or memory; processes communicate only by message passing and may take internal steps between message receipts.
- Self-loops C_ii are allowed; the rules below apply to them unchanged.

## State
- Each process Pi has local state s_i (application-defined; includes local variables and any local buffers).
- Each channel C_ij has state c_ij, the sequence of messages sent on C_ij but not yet received.
- Global state G = (s_1..s_n, {c_ij}).
- A cut is consistent if whenever a receive event is included, its corresponding send event is also included.

## Marker
- Marker M is a special control message distinct from application messages; markers are not part of any channel state.
- Each snapshot instance has a unique snapshot id, denoted sid; markers carry sid.
- All rules below apply independently per snapshot id.

## Algorithm (per snapshot id sid)
State variables per process Pi and snapshot sid:
- state_recorded_i[sid]: boolean, initially false.
- For each incoming channel from k to i: recorded_i[sid][k]: boolean, initially false.

Initiation at Pi for snapshot sid:
1) Record local state s_i^sid; set state_recorded_i[sid] = true.
2) For each outgoing channel C_ij, send marker M(sid) before any further application messages on C_ij.

On receiving marker M(sid) on incoming channel C_lk at process Pk:
- If state_recorded_k[sid] == false (first marker for sid):
  - Record local state s_k^sid; set state_recorded_k[sid] = true.
  - Record channel state for C_lk as empty.
  - For each outgoing channel C_km, send marker M(sid) before any further application messages on C_km.
- Else (state already recorded for sid):
  - Record channel state for C_lk as the sequence of messages received on C_lk after Pk recorded s_k^sid and before receiving this marker M(sid).
- Set recorded_k[sid][l] = true.

Completion for snapshot sid:
- Pk completes its local snapshot for sid when recorded_k[sid][l] == true for every incoming channel from l.
- The global snapshot for sid is the union of all recorded local states s_i^sid and recorded channel states c_ij^sid.
- Any process may initiate sid; multiple initiators for the same sid are allowed and are handled by the first-marker rule.
- The collection mechanism for assembling the global snapshot (e.g., a coordinator or distributed reduction) is outside the algorithm and may be chosen independently.

## Termination (why it completes)
- With at least one initiator for sid and a strongly connected channel graph, every process is reachable by a directed path of channels from an initiator.
- Each process that records its local state for sid sends exactly one marker on every outgoing channel for sid.
- Because channels are reliable and deliver messages after finite delay, every marker sent for sid is eventually received, so every process eventually records its local state and every incoming channel eventually receives a marker; therefore each process completes the snapshot for sid.

## Consistency (why the snapshot is consistent)
- Consider any channel C_ij in snapshot sid. If Pj records a message m in c_ij^sid, then m was received after Pj recorded s_j^sid and before Pj received marker M(sid) on C_ij.
- By FIFO, any message sent by Pi before it sends M(sid) on C_ij is received by Pj before that marker. Since Pi sends M(sid) only after recording s_i^sid and before any further application messages on C_ij, m must have been sent by Pi before recording s_i^sid.
- Thus every recorded channel message was sent before the sender's snapshot and received after the receiver's snapshot, i.e., it is in transit across the cut. Therefore no receive is included without its corresponding send, so the recorded global state is consistent.
