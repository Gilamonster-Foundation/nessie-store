//! Python bindings: ONTAP envelope/HAL builders.
//!
//! The wheel exchanges plain dicts at the boundary (never another wheel's
//! `#[pyclass]`), so a Python user can assemble ONTAP-faithful responses: wrap a
//! record dict in the create/delete/job envelopes, build a HAL collection, or
//! format a `delta.time_elapsed`. Records cross as `dict`s (serde ⇄ Python via
//! `pythonize`).

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde::Serialize;

use crate::{CreateResponse, DeleteResponse, ErrorBody, ErrorEnvelope, HalCollection, JobStatus};

/// Convert a serde value into a Python object (dict/list/scalar).
fn to_py<T: Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    pythonize::pythonize(py, value)
        .map(|b| b.unbind())
        .map_err(|e| PyValueError::new_err(format!("serialize to Python failed: {e}")))
}

/// Read a Python object into a serde JSON value.
fn from_py(obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    pythonize::depythonize(obj)
        .map_err(|e| PyValueError::new_err(format!("not a JSON-compatible value: {e}")))
}

/// Format seconds as an ISO-8601 duration (`PT…H…M…S`, ONTAP `delta.time_elapsed`).
#[pyfunction]
fn iso8601_duration(secs: i64) -> String {
    crate::iso8601_duration(secs)
}

/// Wrap `records` in a HAL collection envelope: `{records, num_records, _links}`.
#[pyfunction]
fn hal_collection(
    py: Python<'_>,
    records: &Bound<'_, PyAny>,
    self_href: &str,
) -> PyResult<Py<PyAny>> {
    let recs: Vec<serde_json::Value> = from_py(records)?
        .as_array()
        .cloned()
        .ok_or_else(|| PyValueError::new_err("records must be a list"))?;
    to_py(py, &HalCollection::new(recs, self_href))
}

/// Wrap a single `record` in the create envelope: `{job, record, num_records}`.
#[pyfunction]
fn create_response(
    py: Python<'_>,
    job_uuid: &str,
    record: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    to_py(py, &CreateResponse::new(job_uuid, from_py(record)?))
}

/// The simplified delete envelope: `{job: {uuid, state: "success"}}`.
#[pyfunction]
fn delete_response(py: Python<'_>, uuid: &str) -> PyResult<Py<PyAny>> {
    to_py(py, &DeleteResponse::success(uuid))
}

/// The always-success job-poll body for `GET /api/cluster/jobs/{uuid}`.
#[pyfunction]
fn job_status(py: Python<'_>, uuid: &str) -> PyResult<Py<PyAny>> {
    to_py(py, &JobStatus::success(uuid))
}

/// The ONTAP-native error envelope: `{error: {code, message, target?}}`.
#[pyfunction]
#[pyo3(signature = (code, message, target=None))]
fn error_envelope(
    py: Python<'_>,
    code: &str,
    message: &str,
    target: Option<String>,
) -> PyResult<Py<PyAny>> {
    let env = ErrorEnvelope {
        error: ErrorBody {
            code: code.to_string(),
            message: message.to_string(),
            target,
        },
    };
    to_py(py, &env)
}

/// The compiled extension module `nessie_ontap_protocol._core`.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(iso8601_duration, m)?)?;
    m.add_function(wrap_pyfunction!(hal_collection, m)?)?;
    m.add_function(wrap_pyfunction!(create_response, m)?)?;
    m.add_function(wrap_pyfunction!(delete_response, m)?)?;
    m.add_function(wrap_pyfunction!(job_status, m)?)?;
    m.add_function(wrap_pyfunction!(error_envelope, m)?)?;
    Ok(())
}
