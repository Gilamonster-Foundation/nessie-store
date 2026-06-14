//! The ONTAP async job envelope and the job-poll status.
//!
//! ONTAP mutating calls return a `job` the client polls until it reports
//! `success`. nessie-store's substrate operations are synchronous, so the work is
//! already done by the time the envelope is returned and the poll endpoint always
//! reports success — but the *shape* must match or SDKs hang waiting for a job
//! that never resolves.

use serde::{Deserialize, Serialize};

use crate::links::Links;

/// The embedded `job` object on a create/modify response:
/// `{ "uuid": ..., "_links": { "self": { "href": "/api/cluster/jobs/<uuid>" } } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRef {
    /// The job UUID (clients poll `/api/cluster/jobs/{uuid}`).
    pub uuid: String,
    /// HAL link to the job-poll endpoint.
    #[serde(rename = "_links")]
    pub links: Links,
}

impl JobRef {
    /// Build a job reference whose self-link points at the poll endpoint.
    #[must_use]
    pub fn new(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            links: Links::to(format!("/api/cluster/jobs/{uuid}")),
        }
    }
}

/// The response shape for a create/modify: `{ job, record, num_records }`.
///
/// Note the singular `record` (not `records`) — this is ONTAP's create envelope,
/// distinct from the plural collection envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResponse<T> {
    /// The async job the client may poll.
    pub job: JobRef,
    /// The single created/modified record.
    pub record: T,
    /// Always 1 for a single-record create/modify.
    pub num_records: usize,
}

impl<T> CreateResponse<T> {
    /// Wrap a freshly created `record` with a job referencing `job_uuid`.
    pub fn new(job_uuid: &str, record: T) -> Self {
        Self {
            job: JobRef::new(job_uuid),
            record,
            num_records: 1,
        }
    }
}

/// A simplified job stub used on DELETE responses: `{ "uuid", "state" }`
/// (no `_links`, carries an inline state — matching ONTAP's delete shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleJob {
    /// The job UUID.
    pub uuid: String,
    /// The job state (always `success` — work is synchronous).
    pub state: String,
}

/// The DELETE response: `{ "job": { "uuid", "state": "success" } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteResponse {
    /// The completed job stub.
    pub job: SimpleJob,
}

impl DeleteResponse {
    /// Build a successful delete response for `uuid`.
    #[must_use]
    pub fn success(uuid: &str) -> Self {
        Self {
            job: SimpleJob {
                uuid: uuid.to_string(),
                state: "success".to_string(),
            },
        }
    }
}

/// The job-poll status returned by `GET /api/cluster/jobs/{uuid}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    /// The job UUID echoed back.
    pub uuid: String,
    /// The job state (always `success`).
    pub state: String,
    /// A human-readable message (`Complete`).
    pub message: String,
    /// HAL self-link to this job.
    #[serde(rename = "_links")]
    pub links: Links,
}

impl JobStatus {
    /// Build the always-success poll status for any `uuid`.
    #[must_use]
    pub fn success(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            state: "success".to_string(),
            message: "Complete".to_string(),
            links: Links::to(format!("/api/cluster/jobs/{uuid}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_response_uses_singular_record_and_job_link() {
        let resp = CreateResponse::new("job-1", json!({ "uuid": "vol-1", "name": "v" }));
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "job": {
                    "uuid": "job-1",
                    "_links": { "self": { "href": "/api/cluster/jobs/job-1" } }
                },
                "record": { "uuid": "vol-1", "name": "v" },
                "num_records": 1
            })
        );
    }

    #[test]
    fn delete_response_is_the_simplified_shape() {
        assert_eq!(
            serde_json::to_value(DeleteResponse::success("job-9")).unwrap(),
            json!({ "job": { "uuid": "job-9", "state": "success" } })
        );
    }

    #[test]
    fn job_poll_always_succeeds() {
        assert_eq!(
            serde_json::to_value(JobStatus::success("j")).unwrap(),
            json!({
                "uuid": "j",
                "state": "success",
                "message": "Complete",
                "_links": { "self": { "href": "/api/cluster/jobs/j" } }
            })
        );
    }
}
