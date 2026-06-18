//! Python bindings: the **inside** extension point.
//!
//! `run_all(backend)` accepts any Python object that implements the storage
//! backend protocol (duck-typed methods returning domain-shaped dicts) and
//! validates it against the same conformance suites Rust backends pass. A Rust
//! [`PyBackend`] adapter wraps the Python object and implements
//! [`VolumeBackend`]/[`SnapshotBackend`]/[`CloneBackend`] by calling back into
//! Python under the GIL — so you can write a storage backend in Python and prove
//! it conforms.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use serde::de::DeserializeOwned;

use nessie_backend_core::{
    AccessHandle, BackendError, Capabilities, CloneBackend, Snapshot, SnapshotBackend,
    SnapshotUuid, Volume, VolumeBackend, VolumePatch, VolumeSpec, VolumeUuid,
};

create_exception!(
    nessie_backend_conformance,
    ConformanceError,
    PyException,
    "A Python-authored backend failed conformance."
);

fn pyerr(e: PyErr) -> BackendError {
    BackendError::Internal(format!("python backend raised: {e}"))
}

fn decode<T: DeserializeOwned>(obj: &Bound<'_, PyAny>) -> Result<T, BackendError> {
    pythonize::depythonize(obj)
        .map_err(|e| BackendError::Internal(format!("decoding python value: {e}")))
}

/// Adapts a Python backend object to the Rust trait stack.
pub(crate) struct PyBackend {
    obj: Py<PyAny>,
    caps: Capabilities,
}

impl PyBackend {
    fn new(obj: Py<PyAny>) -> PyResult<Self> {
        let caps = Python::attach(|py| -> PyResult<Capabilities> {
            let c = obj.bind(py).call_method0("capabilities")?;
            pythonize::depythonize(&c)
                .map_err(|e| ConformanceError::new_err(format!("capabilities() decode: {e}")))
        })?;
        Ok(Self { obj, caps })
    }
}

impl VolumeBackend for PyBackend {
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn list_volumes(&self) -> Result<Vec<Volume>, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method0("list_volumes")
                .map_err(pyerr)?;
            decode(&r)
        })
    }

    fn create_volume(&self, spec: VolumeSpec) -> Result<Volume, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1("create_volume", (spec.name, spec.size_bytes))
                .map_err(pyerr)?;
            decode(&r)
        })
    }

    fn get_volume(&self, uuid: &VolumeUuid) -> Result<Volume, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1("get_volume", (uuid.to_string(),))
                .map_err(pyerr)?;
            if r.is_none() {
                return Err(BackendError::VolumeNotFound(*uuid));
            }
            decode(&r)
        })
    }

    fn delete_volume(&self, uuid: &VolumeUuid) -> Result<(), BackendError> {
        Python::attach(|py| {
            self.obj
                .bind(py)
                .call_method1("delete_volume", (uuid.to_string(),))
                .map_err(pyerr)?;
            Ok(())
        })
    }

    fn patch_volume(&self, uuid: &VolumeUuid, patch: VolumePatch) -> Result<Volume, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1(
                    "patch_volume",
                    (
                        uuid.to_string(),
                        patch.size_bytes,
                        patch.junction_path,
                        patch.export_policy,
                    ),
                )
                .map_err(pyerr)?;
            decode(&r)
        })
    }

    fn access_handle(&self, uuid: &VolumeUuid) -> Result<AccessHandle, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1("access_handle", (uuid.to_string(),))
                .map_err(pyerr)?;
            decode(&r)
        })
    }

    fn as_snapshot(&self) -> Option<&dyn SnapshotBackend> {
        self.caps.snapshots.then_some(self as &dyn SnapshotBackend)
    }
}

impl SnapshotBackend for PyBackend {
    fn list_snapshots(&self, vol: &VolumeUuid) -> Result<Vec<Snapshot>, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1("list_snapshots", (vol.to_string(),))
                .map_err(pyerr)?;
            decode(&r)
        })
    }

    fn create_snapshot(&self, vol: &VolumeUuid, name: &str) -> Result<Snapshot, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1("create_snapshot", (vol.to_string(), name))
                .map_err(pyerr)?;
            decode(&r)
        })
    }

    fn get_snapshot(
        &self,
        vol: &VolumeUuid,
        snap: &SnapshotUuid,
    ) -> Result<Snapshot, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1("get_snapshot", (vol.to_string(), snap.to_string()))
                .map_err(pyerr)?;
            if r.is_none() {
                return Err(BackendError::SnapshotNotFound {
                    volume: *vol,
                    snapshot: *snap,
                });
            }
            decode(&r)
        })
    }

    fn delete_snapshot(&self, vol: &VolumeUuid, snap: &SnapshotUuid) -> Result<(), BackendError> {
        Python::attach(|py| {
            self.obj
                .bind(py)
                .call_method1("delete_snapshot", (vol.to_string(), snap.to_string()))
                .map_err(pyerr)?;
            Ok(())
        })
    }

    fn as_clone(&self) -> Option<&dyn CloneBackend> {
        self.caps.clones.then_some(self as &dyn CloneBackend)
    }
}

impl CloneBackend for PyBackend {
    fn create_clone(
        &self,
        parent_vol: &VolumeUuid,
        parent_snap: &SnapshotUuid,
        new_name: &str,
    ) -> Result<Volume, BackendError> {
        Python::attach(|py| {
            let r = self
                .obj
                .bind(py)
                .call_method1(
                    "create_clone",
                    (parent_vol.to_string(), parent_snap.to_string(), new_name),
                )
                .map_err(pyerr)?;
            decode(&r)
        })
    }
}

/// Validate a Python-authored backend against the full conformance suite.
///
/// `backend` is any Python object implementing the backend protocol. Raises
/// `ConformanceError` (with the failing assertion) if it does not conform.
#[pyfunction]
fn run_all(backend: Py<PyAny>) -> PyResult<()> {
    let pb = PyBackend::new(backend)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::run_all(&pb)));
    result.map_err(|panic| {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "conformance suite panicked".to_string());
        ConformanceError::new_err(msg)
    })
}

/// The compiled extension module `nessie_backend_conformance._core`.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("ConformanceError", m.py().get_type::<ConformanceError>())?;
    m.add_function(wrap_pyfunction!(run_all, m)?)?;
    Ok(())
}
