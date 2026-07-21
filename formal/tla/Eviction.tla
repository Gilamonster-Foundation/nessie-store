-------------------------------- MODULE Eviction --------------------------------
(***************************************************************************)
(* PO-GC-2: cache-mode eviction never loses a reachable blob SWARM-WIDE.    *)
(*                                                                         *)
(* Each node holds a set of blob replicas. A CACHE node may drop a replica  *)
(* only when the replica gate says it is safe: a DURABLE peer holds it, or  *)
(* at least R OTHER nodes hold it. Durable nodes never evict. The invariant *)
(* NoReachableLost says every reachable blob is still held by some node.    *)
(*                                                                         *)
(* `Gate` toggles the replica gate: TRUE (Eviction.cfg, with a durable node *)
(* and R=2) must hold NoReachableLost; FALSE with no durable node           *)
(* (Eviction_Unsafe.cfg) must EXHIBIT the loss — proving the gate is        *)
(* load-bearing (PO-GC-2-B), mirroring the AcCrdt / Gc boundary models.     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Nodes, Durable, Blobs, Reachable, R, Gate

ASSUME /\ Durable \subseteq Nodes
       /\ Reachable \subseteq Blobs
       /\ R \in Nat

VARIABLE held           \* held[n] : the replicas node n holds
vars == << held >>

TypeOK == held \in [Nodes -> SUBSET Blobs]

\* Start fully replicated: every node holds every reachable blob.
Init == held = [n \in Nodes |-> Reachable]

\* Other nodes (besides n) that hold b.
OtherHolders(n, b) == { m \in Nodes \ {n} : b \in held[m] }

\* A durable peer holds b.
DurablyHeld(b) == \E d \in Durable : b \in held[d]

\* The replica gate: safe to drop b at n iff a durable peer holds it, or >= R
\* other nodes do. With the gate off, dropping is unconditionally allowed.
SafeToEvict(n, b) ==
    \/ ~Gate
    \/ DurablyHeld(b)
    \/ Cardinality(OtherHolders(n, b)) >= R

\* A cache node drops a replica it holds, if the gate permits. Durable nodes never
\* evict (they are the store of record).
Evict(n, b) ==
    /\ n \notin Durable
    /\ b \in held[n]
    /\ SafeToEvict(n, b)
    /\ held' = [held EXCEPT ![n] = @ \ {b}]

Next == \E n \in Nodes, b \in Blobs : Evict(n, b)

\* Stuttering allowed so the model does not deadlock once nothing is evictable.
Spec == Init /\ [][Next \/ UNCHANGED vars]_vars

(***************************************************************************)
(* The safety invariant.                                                   *)
(***************************************************************************)

\* PO-GC-2: no reachable blob is absent from EVERY node. VIOLATED with no gate and
\* no durable node — a pure cache swarm can evict every copy of a blob.
NoReachableLost == \A b \in Reachable : \E n \in Nodes : b \in held[n]

===============================================================================
