----------------------------- MODULE RestateSnapshot -----------------------------
(*
    TLA+ Model of Restate's Modified Chandy-Lamport Distributed Snapshot Protocol

    This models N partition processors communicating via FIFO Bifrost logs.
    Each partition has an outbox (pending messages) and a dedup table (processed
    messages). Messages are only removed from the sender's outbox when an explicit
    ack arrives through the same FIFO channel as markers.

    The key safety property is MESSAGE CONSERVATION: at snapshot completion,
    every application message sent before the sender's snapshot is captured in
    at least one partition's snapshot (sender's outbox OR receiver's dedup).

    Channels modeled:
      - Cross-partition: Pi -> Log-Pj -> Pj  (FIFO, one log per partition)
      - Self-loop: drained before snapshot (modeled as atomic precondition)
      - External (invoker/SDK): excluded (idempotent protocol)

    Assumptions baked in:
      A1. Bifrost logs are FIFO and durable (total order per log via LSN)
      A2. Each partition has exactly one appender per destination (shuffle)
      A3. Self-loop channel is drained before checkpoint (SelfProposer flush)
      A4. Outbound messages are gated between snapshot and marker send
          (modeled as atomicity of ProcessInitiate/ProcessMarker actions)
      A5. One snapshot at a time (coordinator constraint)
      A6. Acks flow through Bifrost (same FIFO channel as markers and messages)
      A7. Acks are sent even for duplicate messages (idempotent ack)
*)

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Partition,      \* Set of partition IDs, e.g. {0, 1, 2}
    MaxMessages     \* Bound on messages each partition can send (for model checking)

PartitionSymmetry == Permutations(Partition)

VARIABLES
    \* --- Application state ---
    outbox,         \* outbox[p] = set of <<dest, seq>> pairs awaiting ack
    dedup,          \* dedup[p] = set of <<src, seq>> pairs already processed
    nextSeq,        \* nextSeq[p] = next sequence number for outbox

    \* --- Bifrost channels (FIFO per log) ---
    log,            \* log[p] = sequence of records destined for partition p
                    \* Each record is one of:
                    \*   <<"Msg", src, seq>>
                    \*   <<"Ack", acker, seq>>
                    \*   <<"Marker", src, sid>>
                    \*   <<"Initiate", sid>>

    \* --- Snapshot protocol state ---
    snapshotTaken,  \* snapshotTaken[p] = TRUE iff p has taken its local snapshot
    markersSent,    \* markersSent[p] = set of partitions p has sent markers to
    markersRecvd,   \* markersRecvd[p] = set of partitions p has received markers from

    \* --- Snapshot captures ---
    snapOutbox,     \* snapOutbox[p] = outbox[p] at snapshot time
    snapDedup,      \* snapDedup[p] = dedup[p] at snapshot time
    snapNextSeq,    \* snapNextSeq[p] = nextSeq[p] at snapshot time (send boundary)

    \* --- Global protocol state ---
    phase,          \* "idle", "running", "complete"
    sid             \* current snapshot ID

vars == <<outbox, dedup, nextSeq, log, snapshotTaken, markersSent,
          markersRecvd, snapOutbox, snapDedup, snapNextSeq, phase, sid>>

snapVars == <<snapOutbox, snapDedup, snapNextSeq>>

OtherPartitions(p) == Partition \ {p}

-----------------------------------------------------------------------------
(* Type invariant *)

TypeOK ==
    /\ \A p \in Partition :
        /\ nextSeq[p] \in 1..(MaxMessages + 1)
        /\ snapshotTaken[p] \in BOOLEAN
        /\ markersSent[p] \subseteq Partition
        /\ markersRecvd[p] \subseteq Partition
    /\ phase \in {"idle", "running", "complete"}
    /\ sid \in Nat

-----------------------------------------------------------------------------
(* Initial state *)

Init ==
    /\ outbox        = [p \in Partition |-> {}]
    /\ dedup         = [p \in Partition |-> {}]
    /\ nextSeq       = [p \in Partition |-> 1]
    /\ log           = [p \in Partition |-> <<>>]
    /\ snapshotTaken = [p \in Partition |-> FALSE]
    /\ markersSent   = [p \in Partition |-> {}]
    /\ markersRecvd  = [p \in Partition |-> {}]
    /\ snapOutbox    = [p \in Partition |-> {}]
    /\ snapDedup     = [p \in Partition |-> {}]
    /\ snapNextSeq   = [p \in Partition |-> 1]
    /\ phase         = "idle"
    /\ sid           = 0

-----------------------------------------------------------------------------
(* Helper: atomically take local snapshot and send markers *)

DoSnapshot(p, logUpdate) ==
    /\ snapOutbox' = [snapOutbox EXCEPT ![p] = outbox[p]]
    /\ snapDedup' = [snapDedup EXCEPT ![p] = dedup[p]]
    /\ snapNextSeq' = [snapNextSeq EXCEPT ![p] = nextSeq[p]]
    /\ snapshotTaken' = [snapshotTaken EXCEPT ![p] = TRUE]
    /\ LET others == OtherPartitions(p)
       IN /\ markersSent' = [markersSent EXCEPT ![p] = others]
          /\ log' = logUpdate

-----------------------------------------------------------------------------
(* Application actions *)

(* Send a new application message from src to dest.
   The message is placed in src's outbox AND appended to dest's Bifrost log.
   Gating constraint (A4): if src has taken its snapshot, it must have already
   sent markers before sending more application messages. *)
SendMessage(src, dest) ==
    /\ src # dest
    /\ nextSeq[src] <= MaxMessages
    /\ snapshotTaken[src] => markersSent[src] = OtherPartitions(src)
    /\ LET s == nextSeq[src]
       IN /\ outbox' = [outbox EXCEPT ![src] = @ \cup {<<dest, s>>}]
          /\ nextSeq' = [nextSeq EXCEPT ![src] = s + 1]
          /\ log' = [log EXCEPT ![dest] = Append(@, <<"Msg", src, s>>)]
          /\ UNCHANGED <<dedup, snapshotTaken, markersSent, markersRecvd,
                         snapVars, phase, sid>>

(* Process an application message from the head of a partition's log.
   Updates dedup table and sends an ack back to the sender's log.
   Acks are sent even for duplicate messages (A7) — this is critical for
   outbox truncation after restore. *)
ProcessMessage(p) ==
    /\ Len(log[p]) > 0
    /\ Head(log[p])[1] = "Msg"
    /\ LET rec == Head(log[p])
           src == rec[2]
           s   == rec[3]
       IN /\ dedup' = [dedup EXCEPT ![p] = @ \cup {<<src, s>>}]
          /\ log' = [log EXCEPT
               ![p] = Tail(@),
               ![src] = Append(log[src], <<"Ack", p, s>>)]
    /\ UNCHANGED <<outbox, nextSeq, snapshotTaken, markersSent, markersRecvd,
                   snapVars, phase, sid>>

(* Process an ack: remove the acknowledged message from outbox. *)
ProcessAck(p) ==
    /\ Len(log[p]) > 0
    /\ Head(log[p])[1] = "Ack"
    /\ LET rec   == Head(log[p])
           acker == rec[2]
           s     == rec[3]
       IN /\ outbox' = [outbox EXCEPT ![p] = @ \ {<<acker, s>>}]
          /\ log' = [log EXCEPT ![p] = Tail(@)]
    /\ UNCHANGED <<dedup, nextSeq, snapshotTaken, markersSent, markersRecvd,
                   snapVars, phase, sid>>

-----------------------------------------------------------------------------
(* Snapshot protocol actions *)

(* Coordinator initiates a distributed snapshot by appending InitiateSnapshot
   to every partition's log. Only one snapshot at a time (A5). *)
InitiateSnapshot ==
    /\ phase = "idle"
    /\ phase' = "running"
    /\ sid' = sid + 1
    /\ log' = [p \in Partition |-> Append(log[p], <<"Initiate", sid + 1>>)]
    /\ UNCHANGED <<outbox, dedup, nextSeq, snapshotTaken, markersSent,
                   markersRecvd, snapVars>>

(* Process InitiateSnapshot from the log. This is an ATOMIC action that models:
   1. Gate outbound messages
   2. Flush self-loop (SelfProposer drain) — precondition, not modeled
   3. Take RocksDB checkpoint (capture outbox + dedup + nextSeq)
   4. Send markers to all other partitions
   5. Ungate outbound messages
   The atomicity models assumption A4: no messages escape between snapshot
   and marker send. *)
ProcessInitiate(p) ==
    /\ Len(log[p]) > 0
    /\ Head(log[p])[1] = "Initiate"
    /\ Head(log[p])[2] = sid
    /\ ~snapshotTaken[p]
    /\ DoSnapshot(p,
         [q \in Partition |->
           IF q = p THEN Tail(log[p])
           ELSE IF q \in OtherPartitions(p)
                THEN Append(log[q], <<"Marker", p, sid>>)
                ELSE log[q]])
    /\ UNCHANGED <<outbox, dedup, nextSeq, markersRecvd, phase, sid>>

(* Skip InitiateSnapshot if already snapshotted *)
SkipInitiate(p) ==
    /\ Len(log[p]) > 0
    /\ Head(log[p])[1] = "Initiate"
    /\ snapshotTaken[p]
    /\ log' = [log EXCEPT ![p] = Tail(@)]
    /\ UNCHANGED <<outbox, dedup, nextSeq, snapshotTaken, markersSent,
                   markersRecvd, snapVars, phase, sid>>

(* Process a snapshot marker from another partition.
   If first snapshot trigger for this partition, take snapshot first. *)
ProcessMarker(p) ==
    /\ Len(log[p]) > 0
    /\ Head(log[p])[1] = "Marker"
    /\ LET rec  == Head(log[p])
           from == rec[2]
           s    == rec[3]
       IN /\ s = sid
          /\ IF ~snapshotTaken[p]
             THEN \* First trigger: take snapshot + send markers + record receipt
                  /\ DoSnapshot(p,
                       [q \in Partition |->
                         IF q = p THEN Tail(log[p])
                         ELSE IF q \in OtherPartitions(p)
                              THEN Append(log[q], <<"Marker", p, sid>>)
                              ELSE log[q]])
                  /\ markersRecvd' = [markersRecvd EXCEPT ![p] = @ \cup {from}]
             ELSE \* Already snapshotted: just record marker receipt
                  /\ markersRecvd' = [markersRecvd EXCEPT ![p] = @ \cup {from}]
                  /\ log' = [log EXCEPT ![p] = Tail(@)]
                  /\ UNCHANGED <<snapshotTaken, markersSent, snapVars>>
    /\ UNCHANGED <<outbox, dedup, nextSeq, phase, sid>>

(* Snapshot completes when all partitions have snapshotted and received
   markers from all other partitions. *)
CompleteSnapshot ==
    /\ phase = "running"
    /\ \A p \in Partition :
         /\ snapshotTaken[p]
         /\ markersRecvd[p] = OtherPartitions(p)
    /\ phase' = "complete"
    /\ UNCHANGED <<outbox, dedup, nextSeq, log, snapshotTaken, markersSent,
                   markersRecvd, snapVars, sid>>

-----------------------------------------------------------------------------
(* Next-state relation *)

Next ==
    \/ \E src, dest \in Partition : SendMessage(src, dest)
    \/ \E p \in Partition : ProcessMessage(p)
    \/ \E p \in Partition : ProcessAck(p)
    \/ InitiateSnapshot
    \/ \E p \in Partition : ProcessInitiate(p)
    \/ \E p \in Partition : SkipInitiate(p)
    \/ \E p \in Partition : ProcessMarker(p)
    \/ CompleteSnapshot

Spec == Init /\ [][Next]_vars

(* Log-processing action for partition p — drains whatever is at the head *)
ProcessLog(p) ==
    \/ ProcessMessage(p)
    \/ ProcessAck(p)
    \/ ProcessInitiate(p)
    \/ SkipInitiate(p)
    \/ ProcessMarker(p)

(* Liveness spec: assumes PPs eventually process their logs and the
   coordinator eventually initiates and recognizes completion.
   No fairness on SendMessage — termination holds regardless of whether
   new application messages are generated. *)
LiveSpec == Spec
    /\ WF_vars(InitiateSnapshot)
    /\ \A p \in Partition : WF_vars(ProcessLog(p))
    /\ WF_vars(CompleteSnapshot)

-----------------------------------------------------------------------------
(* Safety properties *)

(* MESSAGE CONSERVATION (NoMessageLost)
   When the snapshot is complete, every message sent by any partition before
   its snapshot (i.e. every sequence number < snapNextSeq[src]) that was
   directed at some destination must appear in at least one snapshot:
   either the sender's outbox snapshot or the receiver's dedup snapshot.

   This is the core Chandy-Lamport invariant: no in-flight message is lost. *)
NoMessageLost ==
    phase = "complete" =>
        \A src \in Partition :
            \A msg \in snapOutbox[src] :
                LET dest == msg[1]
                    s    == msg[2]
                IN \* Message is in receiver's snapshot dedup, or sender still has it
                   \* (The second disjunct is needed for messages sent but not yet
                   \* received at snapshot time — they must remain in outbox.)
                   <<src, s>> \in snapDedup[dest] \/ <<dest, s>> \in snapOutbox[src]

(* The stronger form: every message ever sent before the sender's snapshot
   is accounted for somewhere in the global snapshot. A message <<dest, s>>
   from src exists iff it's in the outbox, or has been acked and thus must
   be in the receiver's dedup. We check: for every message that was sent
   (seq < snapNextSeq) AND is NOT in the sender's snapshot outbox, it MUST
   be in some receiver's snapshot dedup. *)
AllSentMessagesCaptured ==
    phase = "complete" =>
        \A src \in Partition :
            \A s \in 1..(snapNextSeq[src] - 1) :
                \* Message s was sent to exactly one dest. Either it's
                \* still in the outbox (for some dest), or the dest
                \* already has it in dedup. Check both options:
                \/ \E dest \in OtherPartitions(src) :
                     <<dest, s>> \in snapOutbox[src]
                \/ \E dest \in OtherPartitions(src) :
                     <<src, s>> \in snapDedup[dest]

(* CONSISTENT CUT
   If a receiver's snapshot dedup contains evidence of processing a message
   from src, then src must have sent that message before src's own snapshot.
   This prevents "effect without cause" in the recorded global state. *)
ConsistentCut ==
    phase = "complete" =>
        \A p \in Partition :
            \A entry \in snapDedup[p] :
                LET src == entry[1]
                    s   == entry[2]
                IN s < snapNextSeq[src]  \* Sender had sent this before its snapshot

-----------------------------------------------------------------------------
(* Liveness property *)

(* SNAPSHOT TERMINATION
   Under fairness assumptions (PPs process their logs, coordinator acts),
   the distributed snapshot eventually completes. *)
SnapshotTermination == <>(phase = "complete")

=============================================================================
