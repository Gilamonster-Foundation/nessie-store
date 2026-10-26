//! Mutual TLS built on agent-mesh keys: nessie invents no identity of its own.
//!
//! * [`NodeIdentity`] — this node's agent-mesh `AgentKey` (seed + signed
//!   `CertChain` + the operator's `GitHubBinding`), on disk under
//!   `<data_dir>/identity/`.
//! * [`proof`] — the self-signed X.509 certificate whose key *is* the agent key
//!   and whose single extension carries the already-signed chain and binding
//!   (the libp2p-TLS pattern). No CA.
//! * [`AuthorizedKeys`] — the SSH `authorized_keys` file that is the only trust
//!   root (format: one `ssh-ed25519` public key per line, no options): a peer
//!   is admitted iff its operator's binding is signed by a listed key.
//! * [`MeshVerifier`] — implements rustls' `ServerCertVerifier` and
//!   `ClientCertVerifier` over the above.
//!
//! # Certificate extension OID
//!
//! The extension is `2.25.8763957256297098986` ([`proof::PROOF_OID`]): the
//! UUID-arc form (`2.25.<uuid>`) of the UUID
//! `00000000-0000-0000-799f-d4b0f04baeea` (decimal `8763957256297098986`). `rcgen` encodes OID arcs
//! as `u64`, so the arc cannot carry a full random 128-bit UUID; the high 64 bits
//! are fixed to zero and the low 63 bits are random, which keeps the OID
//! collision-free in practice. Its value is the serde JSON of
//! `{ "chain": <CertChain>, "binding": <GitHubBinding> }` — no new encoding.
//!
//! # Security posture
//!
//! Every rule fails closed; see [`MeshVerifier::verify_peer`] for the acceptance
//! rules. Handshakes are TLS 1.3 only.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod authorized_keys;
mod error;
mod identity;
pub mod proof;
mod tls;
mod verifier;

pub use authorized_keys::AuthorizedKeys;
pub use error::MtlsError;
pub use identity::NodeIdentity;
pub use proof::PeerProof;
pub use tls::{client_config, server_config};
pub use verifier::MeshVerifier;
