-------------------------------- MODULE AcCrdt --------------------------------
(***************************************************************************)
(* The ActionCache attestation-CRDT, and the safety of its k-of-n         *)
(* confirmation gate under a Byzantine signer MINORITY.                    *)
(*                                                                         *)
(* Design claim this discharges (see ../README.md, obligations PO-AC-1..3):*)
(*                                                                         *)
(*   nessie's AC is a grow-only set of SIGNED attestations. A result is    *)
(*   "confirmed" only when >= K distinct signers agree on it. As long as   *)
(*   fewer than K signers are Byzantine, no FALSE result is ever confirmed *)
(*   at any node (no-forgery), no two nodes confirm different results for  *)
(*   the same action (agreement), and confirmation, once reached, is       *)
(*   stable because the store only grows (monotonicity).                   *)
(*                                                                         *)
(* One action is modelled; distinct actions are independent instances of   *)
(* this model, so the single-action result carries. The signer set and     *)
(* the honest/Byzantine split are ASSUMED given by the swarm-membership /   *)
(* identity primitive (agent-mesh signed peers); this spec does not model   *)
(* how membership is established, only its consequence for confirmation.    *)
(***************************************************************************)
EXTENDS FiniteSets, Naturals

CONSTANTS Nodes,        \* the swarm's nodes (replicas of the AC state)
          Signers,      \* the identities that can sign attestations
          Byzantine,    \* the subset of Signers that may lie
          Results,      \* possible result values for the action
          K,            \* confirmation threshold (k-of-n)
          TrueResult    \* the one correct result of the (deterministic) action

ASSUME /\ Byzantine \subseteq Signers
       /\ K \in Nat
       /\ TrueResult \in Results

Honest == Signers \ Byzantine

\* An attestation: signer S asserts the action produced result R.
Attestation == [signer : Signers, result : Results]

VARIABLE store        \* store[n] : the set of attestations node n has seen
vars == << store >>

TypeOK == store \in [Nodes -> SUBSET Attestation]

Init == store = [n \in Nodes |-> {}]

(***************************************************************************)
(* Actions.                                                                *)
(*                                                                         *)
(* Attest: a signer S produces an attestation, entering the system at some *)
(* node N. HONEST signers only ever attest the true result; Byzantine      *)
(* signers may attest anything. This honesty constraint is the whole game. *)
(***************************************************************************)
Attest(n, s, r) ==
    /\ (s \in Honest) => (r = TrueResult)
    /\ store' = [store EXCEPT ![n] = @ \cup {[signer |-> s, result |-> r]}]

\* Gossip: node N merges everything M has seen (monotone set union = the join).
Gossip(n, m) ==
    store' = [store EXCEPT ![n] = @ \cup store[m]]

Next ==
    \/ \E n \in Nodes, s \in Signers, r \in Results : Attest(n, s, r)
    \/ \E n \in Nodes, m \in Nodes : Gossip(n, m)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Confirmation.                                                           *)
(*                                                                         *)
(* The distinct signers who, in node N's view, have attested result R;     *)
(* R is confirmed at N iff at least K distinct signers back it.            *)
(***************************************************************************)
SignersFor(n, r) == { a.signer : a \in { x \in store[n] : x.result = r } }

Confirmed(n, r) == Cardinality(SignersFor(n, r)) >= K

(***************************************************************************)
(* The properties TLC checks.                                             *)
(***************************************************************************)

\* PO-AC-1  No-forgery: with a Byzantine minority (< K), a false result is
\* never confirmed anywhere. (A false result is attested only by Byzantine
\* signers, so its backer set is a subset of Byzantine, whose size is < K.)
NoForgery ==
    (Cardinality(Byzantine) < K) =>
        \A n \in Nodes, r \in Results :
            Confirmed(n, r) => (r = TrueResult)

\* PO-AC-2  Agreement: no two nodes confirm different results for the action.
Agreement ==
    (Cardinality(Byzantine) < K) =>
        \A n1 \in Nodes, n2 \in Nodes, r1 \in Results, r2 \in Results :
            (Confirmed(n1, r1) /\ Confirmed(n2, r2)) => (r1 = r2)

\* PO-AC-3  Monotonicity: the store only grows, so confirmation never revokes.
\* Checked as an action property over every step.
MonotoneStore == [][ \A n \in Nodes : store[n] \subseteq store'[n] ]_vars

\* Boundary demonstration (see AcCrdt_ByzThreshold.cfg): the UNCONDITIONAL
\* no-forgery claim, dropping the < K hypothesis. TLC must find a COUNTEREXAMPLE
\* when |Byzantine| >= K — that is the point: K signatures is exactly the security
\* boundary, so a Byzantine cohort of size K can forge. This proves the minority
\* assumption in NoForgery is load-bearing, not decorative.
ForgeryFree ==
    \A n \in Nodes, r \in Results : Confirmed(n, r) => (r = TrueResult)

===============================================================================
