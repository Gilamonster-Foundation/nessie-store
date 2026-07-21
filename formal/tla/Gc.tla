----------------------------------- MODULE Gc -----------------------------------
(***************************************************************************)
(* PO-GC-1-op: the operational / concurrency half of durable GC safety.    *)
(*                                                                         *)
(* The Lean proof (`../lean/Gc.lean`) shows the sweep never reclaims a      *)
(* reachable blob on a fixed snapshot. The real hazard is a RACE: a blob is *)
(* put, and before the operation that will reference it lands (its confirmed *)
(* AC entry registered as a root), a GC pass runs and — seeing it as not-yet *)
(* a root — sweeps it. The Rust defense is the in-process WRITE-GUARD        *)
(* (`in_flight`): GC skips a guarded blob. This model checks that guard.     *)
(*                                                                         *)
(* `GuardInflight` toggles it: TRUE (Gc.cfg) must hold InflightProtected;   *)
(* FALSE (Gc_Unguarded.cfg) must EXHIBIT the violation — proving the guard   *)
(* is load-bearing, mirroring the AcCrdt boundary model.                    *)
(*                                                                         *)
(* Reachability here is depth-0 (a blob is reachable iff it is a root),     *)
(* matching that RootsStored = "GC never removes a root" is the operational *)
(* PO-GC-1; the transitive closure is the Lean model's concern.             *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS Blobs, GuardInflight

VARIABLES stored, roots, inflight
vars == << stored, roots, inflight >>

TypeOK ==
    /\ stored \subseteq Blobs
    /\ roots \subseteq Blobs
    /\ inflight \subseteq Blobs

Init ==
    /\ stored = {}
    /\ roots = {}
    /\ inflight = {}

\* Put a new blob: stored and guarded in-flight, not yet a root.
Put(b) ==
    /\ b \notin stored
    /\ stored' = stored \cup {b}
    /\ inflight' = inflight \cup {b}
    /\ roots' = roots

\* Reference (commit): the blob is now anchored by a root, guard released.
Reference(b) ==
    /\ b \in stored
    /\ roots' = roots \cup {b}
    /\ inflight' = inflight \ {b}
    /\ stored' = stored

\* Abandon: the blob will not be referenced; release the guard (GC may reclaim it).
Abandon(b) ==
    /\ b \in inflight
    /\ inflight' = inflight \ {b}
    /\ stored' = stored
    /\ roots' = roots

\* The blobs a GC pass may sweep: stored, not a root (unreachable), and — when the
\* guard is on — not in-flight.
Sweepable ==
    { b \in stored : /\ b \notin roots
                     /\ (GuardInflight => b \notin inflight) }

Gc ==
    /\ stored' = stored \ Sweepable
    /\ roots' = roots
    /\ inflight' = inflight

Next ==
    \/ \E b \in Blobs : Put(b)
    \/ \E b \in Blobs : Reference(b)
    \/ \E b \in Blobs : Abandon(b)
    \/ Gc

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Invariants.                                                             *)
(***************************************************************************)

\* PO-GC-1-op: an in-flight (guarded) blob is never swept — it is always still
\* stored. VIOLATED when the guard is off and GC sweeps a just-put blob.
InflightProtected == inflight \subseteq stored

\* PO-GC-1 (operational, depth-0): a root (reachable) blob is never swept. Holds
\* regardless of the guard — the sweep excludes roots by construction.
RootsStored == roots \subseteq stored

===============================================================================
