//! Python bindings (Face A — the "outside" surface) for the core vocabulary.
//!
//! All `#[pyclass]`/`#[pymethods]` live here so the rest of the crate stays
//! PyO3-free and Rust-only consumers never link `python3`. The wrappers are thin
//! newtypes over the domain types; conversions happen at this boundary. The
//! compiled module is `nessie_backend_core._core`; the hand-written
//! `python/nessie_backend_core/__init__.py` re-exports from it.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

use crate::{
    Capabilities, CloneOrigin, Snapshot, Volume, VolumePatch, VolumeSpec, VolumeState, VolumeType,
};

create_exception!(
    nessie_backend_core,
    NessieError,
    PyException,
    "Base exception for nessie-store backend errors."
);

const fn state_str(s: VolumeState) -> &'static str {
    match s {
        VolumeState::Online => "online",
        VolumeState::Offline => "offline",
        VolumeState::Restricted => "restricted",
    }
}

const fn type_str(t: VolumeType) -> &'static str {
    match t {
        VolumeType::Rw => "rw",
        VolumeType::Dp => "dp",
    }
}

#[pyclass(name = "Capabilities", frozen, eq)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PyCapabilities {
    pub(crate) inner: Capabilities,
}

#[pymethods]
impl PyCapabilities {
    #[new]
    #[pyo3(signature = (snapshots=false, clones=false, replication=false))]
    fn new(snapshots: bool, clones: bool, replication: bool) -> Self {
        Self {
            inner: Capabilities {
                snapshots,
                clones,
                replication,
            },
        }
    }

    #[getter]
    fn snapshots(&self) -> bool {
        self.inner.snapshots
    }
    #[getter]
    fn clones(&self) -> bool {
        self.inner.clones
    }
    #[getter]
    fn replication(&self) -> bool {
        self.inner.replication
    }

    /// True if the advertised tiers respect the cumulative ordering.
    fn is_consistent(&self) -> bool {
        self.inner.is_consistent()
    }

    fn __repr__(&self) -> String {
        format!(
            "Capabilities(snapshots={}, clones={}, replication={})",
            py_bool(self.inner.snapshots),
            py_bool(self.inner.clones),
            py_bool(self.inner.replication),
        )
    }
}

fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

/// Render an optional integer the way Python's repr would: the number, or `None`.
fn opt_int(o: Option<u64>) -> String {
    o.map_or_else(|| "None".to_string(), |v| v.to_string())
}

/// Render an optional string Python-style: a quoted value, or `None`.
fn opt_str(o: &Option<String>) -> String {
    o.as_ref()
        .map_or_else(|| "None".to_string(), |s| format!("{s:?}"))
}

#[pyclass(name = "CloneOrigin", frozen, eq)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PyCloneOrigin {
    pub(crate) inner: CloneOrigin,
}

#[pymethods]
impl PyCloneOrigin {
    #[getter]
    fn parent_volume(&self) -> String {
        self.inner.parent_volume.clone()
    }
    #[getter]
    fn parent_snapshot(&self) -> String {
        self.inner.parent_snapshot.clone()
    }
    fn __repr__(&self) -> String {
        format!(
            "CloneOrigin(parent_volume={:?}, parent_snapshot={:?})",
            self.inner.parent_volume, self.inner.parent_snapshot
        )
    }
}

#[pyclass(name = "Volume", frozen, eq)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PyVolume {
    pub(crate) inner: Volume,
}

#[pymethods]
impl PyVolume {
    #[getter]
    fn uuid(&self) -> String {
        self.inner.uuid.to_string()
    }
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }
    #[getter]
    fn size_bytes(&self) -> Option<u64> {
        self.inner.size_bytes
    }
    #[getter]
    fn state(&self) -> &'static str {
        state_str(self.inner.state)
    }
    #[getter]
    fn style(&self) -> &'static str {
        "flexvol"
    }
    #[getter]
    fn vol_type(&self) -> &'static str {
        type_str(self.inner.vol_type)
    }
    #[getter]
    fn clone(&self) -> Option<PyCloneOrigin> {
        self.inner
            .clone
            .as_ref()
            .map(|o| PyCloneOrigin { inner: o.clone() })
    }
    /// True if this volume is a FlexClone of some parent snapshot.
    fn is_clone(&self) -> bool {
        self.inner.is_clone()
    }
    fn __repr__(&self) -> String {
        format!(
            "Volume(uuid={:?}, name={:?}, size_bytes={}, state={:?})",
            self.inner.uuid.to_string(),
            self.inner.name,
            opt_int(self.inner.size_bytes),
            state_str(self.inner.state),
        )
    }
}

#[pyclass(name = "Snapshot", frozen, eq)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PySnapshot {
    pub(crate) inner: Snapshot,
}

#[pymethods]
impl PySnapshot {
    #[getter]
    fn uuid(&self) -> String {
        self.inner.uuid.to_string()
    }
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }
    #[getter]
    fn create_time(&self) -> Option<String> {
        self.inner.create_time.map(|t| t.to_rfc3339())
    }
    #[getter]
    fn size_consumed(&self) -> u64 {
        self.inner.size_consumed
    }
    fn __repr__(&self) -> String {
        format!(
            "Snapshot(uuid={:?}, name={:?}, size_consumed={})",
            self.inner.uuid.to_string(),
            self.inner.name,
            self.inner.size_consumed
        )
    }
}

#[pyclass(name = "VolumeSpec", frozen, eq)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PyVolumeSpec {
    pub(crate) inner: VolumeSpec,
}

#[pymethods]
impl PyVolumeSpec {
    #[new]
    #[pyo3(signature = (name, size_bytes=None))]
    fn new(name: String, size_bytes: Option<u64>) -> Self {
        Self {
            inner: VolumeSpec { name, size_bytes },
        }
    }
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }
    #[getter]
    fn size_bytes(&self) -> Option<u64> {
        self.inner.size_bytes
    }
    fn __repr__(&self) -> String {
        format!(
            "VolumeSpec(name={:?}, size_bytes={})",
            self.inner.name,
            opt_int(self.inner.size_bytes)
        )
    }
}

#[pyclass(name = "VolumePatch", frozen, eq)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PyVolumePatch {
    pub(crate) inner: VolumePatch,
}

#[pymethods]
impl PyVolumePatch {
    #[new]
    #[pyo3(signature = (size_bytes=None, junction_path=None, export_policy=None))]
    fn new(
        size_bytes: Option<u64>,
        junction_path: Option<String>,
        export_policy: Option<String>,
    ) -> Self {
        Self {
            inner: VolumePatch {
                size_bytes,
                junction_path,
                export_policy,
            },
        }
    }
    #[getter]
    fn size_bytes(&self) -> Option<u64> {
        self.inner.size_bytes
    }
    #[getter]
    fn junction_path(&self) -> Option<String> {
        self.inner.junction_path.clone()
    }
    #[getter]
    fn export_policy(&self) -> Option<String> {
        self.inner.export_policy.clone()
    }
    /// True if the patch carries no changes.
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    fn __repr__(&self) -> String {
        format!(
            "VolumePatch(size_bytes={}, junction_path={}, export_policy={})",
            opt_int(self.inner.size_bytes),
            opt_str(&self.inner.junction_path),
            opt_str(&self.inner.export_policy),
        )
    }
}

/// The compiled extension module `nessie_backend_core._core`.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("NessieError", m.py().get_type::<NessieError>())?;
    m.add_class::<PyCapabilities>()?;
    m.add_class::<PyCloneOrigin>()?;
    m.add_class::<PyVolume>()?;
    m.add_class::<PySnapshot>()?;
    m.add_class::<PyVolumeSpec>()?;
    m.add_class::<PyVolumePatch>()?;
    m.add(
        "__doc__",
        "Core domain vocabulary for nessie-store (PyO3 bindings).",
    )?;
    Ok(())
}
