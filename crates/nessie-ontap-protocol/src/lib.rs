//! ONTAP REST wire shapes and the domain→wire mapping for `nessie-store`.
//!
//! This crate owns everything that has to be byte-faithful to NetApp's published
//! ONTAP REST API so that unmodified clients — the `netapp-ontap` Python SDK, the
//! `netapp.ontap` Ansible collection, the Terraform provider, Trident/CSI — work
//! against nessie-store without changes:
//!
//! - The HAL envelope: `_links` ([`Links`]) and the `records`/`num_records`
//!   collection ([`HalCollection`]).
//! - The async **job envelope** ([`CreateResponse`], [`DeleteResponse`]) and the
//!   job-poll status ([`JobStatus`]) — ONTAP's submit→poll fiction.
//! - The ONTAP-native **error envelope** ([`ErrorEnvelope`]) — `{"error":{...}}`,
//!   not a framework's default — plus [`status_for`] mapping each
//!   [`nessie_backend_core::BackendError`] onto the right HTTP status.
//! - Resource records ([`VolumeRecord`], [`SnapshotRecord`]) and mapping helpers
//!   from the substrate-neutral domain types.
//!
//! The crate is deliberately free of any HTTP framework: it returns plain data
//! and `u16` status codes, and the daemon turns those into `axum` responses.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod collection;
mod duration;
mod error;
mod job;
mod links;
mod records;

pub use collection::HalCollection;
pub use duration::iso8601_duration;
pub use error::{ErrorBody, ErrorEnvelope, envelope_for, status_for};
pub use job::{CreateResponse, DeleteResponse, JobRef, JobStatus, SimpleJob};
pub use links::{Links, SelfLink};
pub use records::{
    CloneInfo, NameRef, NasInfo, SnapshotDelta, SnapshotRecord, SvmRef, VolumeRecord,
    snapshot_record, volume_record,
};
