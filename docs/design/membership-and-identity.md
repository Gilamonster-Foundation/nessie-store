# Membership & identity — nessie rides agent-mesh, it does not reinvent it

**Status:** design convergence, grounded 2026-07-23. Resolves the "coordinate
swarm-join with agent-mesh before building a router" decision, and **corrects** the
routing assumption in [`p2p-cas-swarm.md`](./p2p-cas-swarm.md) (§ *Content routing*).

## Why this note exists

The single-node CAS substrate and the whole REAPI face are built and merged. What
remains is *distribution* — turning one node into a swarm — and the swarm needs an
identity, a membership set, and a way to answer *"who holds digest `H`?"*. Rather than
invent those, nessie sits **atop** two systems that already own them, and that are
being actively converged **right now** in the newt-web/newt-agent membership story
(web-authz epic [newt-agent#1354](https://github.com/Gilamonster-Foundation/newt-agent/issues/1354),
step 6 = [#1365](https://github.com/Gilamonster-Foundation/newt-agent/pull/1365)):

- **agent-mesh** owns *peer* identity, membership, discovery, and authenticated
  transport (iroh-QUIC).
- **agent-bridle** owns *operator* identity and the signed human↔agent binding
  (`HumanPrincipal`, `PrincipalBinding`, published in 0.7.13).

This note grounds the convergence in the **real, present-day types** of both (read
directly, not assumed), maps nessie's existing seams onto them, and pins the one thing
that is genuinely nessie's to build.

## Three layers, three owners

The newt-web triangle (IdP proves the human · browser proves session · agent proves
mesh-key) already stacks these. nessie slots in as the storage/verification substrate:

| Layer | Question it answers | Owner | The real type (today) |
|-------|--------------------|-------|-----------------------|
| **L1 — Operator** | *which human authorized this?* | agent-bridle | `HumanPrincipal { issuer, subject, email, groups }`, keyed `(issuer, subject)`, `is_member_of(group)` |
| **L1↔L2 edge** | *this human controls that agent* | agent-bridle | `PrincipalBinding { human, mesh_agent_fingerprint: Fingerprint, issued_generation, sig }`, signed with the mesh `AgentKey` |
| **L2 — Peer** | *which node, and is it in my swarm?* | agent-mesh-protocol | `AgentKey` (ed25519, cert-chained to a `UserKey` root, GitHub-rooted) · `Fingerprint([u8;32]) = BLAKE3(pubkey)` · `CertChain::verify_at(generation)` · `MeshSigner` |
| **L3 — Content location** | *which peer holds digest `H`?* | **nessie** | `ContentRouter` — **net-new** |

L1/L2 are complete and published. L3 does not exist anywhere (confirmed below), and is
nessie's contribution.

## What nessie consumes — and must NOT rebuild

- **Peer identity — reuse wholesale.** `AgentKey`/`CertChain`/`Fingerprint`/`MeshSigner`
  (ed25519 + BLAKE3, rooted at a GitHub-cross-signed `UserKey`). nessie signs every
  attestation and every provider record with an `AgentKey` and verifies to the user root
  exactly as mesh envelopes do.
- **Membership — consume the implicit model; invent no roster.** agent-mesh has *no*
  join/leave/roster API by design. The swarm **is** the set of agents whose cert chains
  root to the same **`user_fp`** — the "auto-team rule", enforced fail-closed at the
  transport handshake (`ensure_trustable(peer_cert, our_user_fp)` →
  `TransportError::DifferentUser`). Enumerate live peers with
  `PeerResolver::known()` filtered by `PeerInfo::is_same_user(&our_user_fp)`.
- **Discovery — reuse.** mDNS (`Announcer`/`Browser`/`PeerResolver`) gives identity →
  addresses on-LAN; a manual `PeerEndpoint { agent_pubkey, addr }` reaches off-LAN peers.
- **Transport + messaging — reuse.** iroh-QUIC `Endpoint`, the `Bus` (`request` /
  `handle_requests`, `publish_to` / `subscribe`), `SignedEnvelope` framing, and the
  built-in `NonceCache` + per-peer strictly-monotonic `SequenceTracker` replay defense.
  nessie's router is a **layer over `Bus`, not a new socket stack.**
- **Authorization — reuse the OCAP lattice.** `Caveats { fs_read, fs_write, exec, net,
  max_calls, valid_for_generation }` (a signed, meet-attenuating lattice inside the cert)
  gates what a peer may do; an announce/write can be caveat-scoped. Operator attribution
  rides `HumanPrincipal` + `PrincipalBinding` (who authorized a durable pin).

## The seam mapping (concrete)

nessie's identity seams were deliberately built as **opaque newtypes** so a foreign
identity drops in with no core change:

| nessie seam (today) | filled by (agent-mesh / agent-bridle) |
|---------------------|----------------------------------------|
| `PeerId(String)` | `Fingerprint::hex()` (32-byte BLAKE3 of the agent pubkey) |
| `SignerId(Vec<u8>)` | the `Fingerprint` bytes |
| `Signature(Vec<u8>)` | an ed25519 signature by the `AgentKey` |
| `trait SignatureVerifier::verify(&SignerId, &[u8], &Signature)` | ed25519 verify **after** `CertChain::verify_at(gen)` to the `user_fp` root — **retires `TestKeyring`** |
| `trait AttestationSigner::sign_statement(action, result)` | `AgentKey::sign` via `MeshSigner` — **retires `DevSelfSigner`** |
| AC `k`-of-`n` distinct signers | `k` distinct `Fingerprint`s under one `user_fp` (the auto-team) |
| AC attestation freshness / GC | `issued_generation` (causal generation, **not** wall-clock) |
| AC attestation revocation | `CertChain::verify_at(generation)` — a revoked-at-`G` signer's attestations drop |

### The content-digest vs. peer-fingerprint distinction

agent-mesh overloads `Fingerprint = BLAKE3(bytes)` for **both** keys and content, and its
own store RFC cautions against exactly that. nessie already keeps them distinct: content
ids are the self-describing **multihash `Digest`** (BLAKE3 default + SHA-256 first-class,
per *multihash over baked-in one-hash-only*), while peer ids are `Fingerprint`. Both are
BLAKE3-family but semantically separate — the multihash law already bought this separation.

## Swarm-join = mesh-join

A nessie node does not "join a nessie swarm." It **is** a mesh agent; completing the mesh
transport handshake under a `user_fp` *is* the join, and `ensure_trustable` is the
membership gate. There is no second roster, no nessie-specific membership CRDT, and no
separate auth. This is *law minimalism* applied to distribution: the membership law is
agent-mesh's cert chain; nessie adds none.

## The one net-new primitive — `ContentRouter` (content location)

agent-mesh is identity-addressed messaging + pub/sub. It has **no** content-location
layer: nothing maps a digest to the peers that hold it (`Anycast{capability}` is a
defined recipient with no routing behind it; `payload_cid` is integrity-only). So
`providers(digest)` / `announce(digest)` / `withdraw(digest)` are genuinely nessie's.

The mesh-native implementation (the primary one) is small precisely because it rides all
of the above:

- A provider announcement is a **signed record wrapped in a `SignedEnvelope`** (so it is
  authenticated and non-repudiable like every mesh message), published on a well-known
  `Topic(user_fp, "nessie/providers/v1")`.
- `providers(digest)` folds the announcements a node has seen into
  `digest → Vec<Fingerprint>`, then dials each provider with `Bus::request` (LAN
  resolver) or `request_direct(PeerEndpoint, …)` (WAN).
- `announce` / `withdraw` are add/remove ops on that index — **structurally the same
  "signed per-writer op + head-vector gossip over a bus topic" shape** the agent-mesh
  `docs/decisions/agent_mesh_store.md` RFC already blesses (`StoreOp`, `LogBackend`).

## Routing, corrected

[`p2p-cas-swarm.md`](./p2p-cas-swarm.md) recorded routing as **"settled — hybrid NATS
rendezvous + Kademlia DHT, both first-class."** Grounding against the real mesh revises
this:

- **Primary = the mesh-native `Bus` router** (iroh-QUIC), above. It reuses mesh identity,
  membership, transport, and replay defense, and shares its shape with the store RFC. This
  is the router that fits *this* fleet.
- **NATS remains a legitimate *alternative* `ContentRouter`** — for environments that
  already run NATS infra — but it is **not** the mesh fabric and not the default. It stays
  behind the one seam.
- **Kademlia / DHT** stays the open-swarm, no-central-anything variant — but it is a
  larger, later, **nessie-specific** build: agent-mesh itself deliberately declined a DHT
  (mDNS + explicit endpoints, iroh relay disabled), so a DHT is not shared infrastructure.

The `ContentRouter` seam (`providers`/`announce`/`withdraw`) is unchanged and still makes
the choice deferrable; only the *ranking* of implementations changes, toward the fabric
that actually exists.

## Convergence bonus — nessie's AC-CRDT ≈ the agent-mesh-store RFC

nessie's ActionCache is a grow-only set of signed attestations gossiped toward `k`-of-`n`
confirmation. The unbuilt `agent-mesh-store` RFC proposes per-writer signed BLAKE3-chained
logs + head-vector gossip. **These are the same primitive.** The convergence opportunity:
nessie's AC (and the provider index) become **consumers of a shared `LogBackend`/`StoreOp`
substrate** rather than a parallel gossip stack — one signed-log-over-a-bus-topic engine,
two indexes on top (attestations; providers). Worth raising on the agent-mesh side before
either project builds its own; captured as a cross-ref memo for newt-agent/agent-mesh.

## What this changes on the nessie roadmap

1. **The first real `ContentRouter` is the mesh `Bus` router, not a NATS router.** It
   needs a *running mesh* to validate (nuc1/nuc2/gnuc as mesh peers), not a running NATS —
   a different, and more aligned, unblock than the earlier plan assumed.
2. **`SignatureVerifier` / `AttestationSigner` get their production fill from agent-mesh
   `AgentKey`**, retiring `DevSelfSigner` (REAPI) and `TestKeyring` (conformance) from any
   non-test path.
3. **Attestation GC gets an answer** — prune by `issued_generation` + `verify_at(gen)`
   (a revoked/expired-generation signer's attestations are dropped), closing the design
   doc's open "attestation garbage collection" question.
4. **`PeerId`/`SignerId` should evolve toward carrying `Fingerprint` bytes** (they are
   opaque today, which is enough to start; a typed `Fingerprint` newtype is a later
   tightening).

None of this is built here — this note is the coordination the "agent-mesh first" decision
called for. Implementation waits on a running mesh and on the shared-log conversation with
agent-mesh landing.
