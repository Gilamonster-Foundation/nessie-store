//! Python bindings: the ZFS backend, scriptable from Python.
//!
//! `ZfsBackend` is exposed as a Python class whose methods return plain dicts
//! (serde→Python via `pythonize`) and raise [`NessieError`] on failure. Two
//! extension points meet here:
//!
//! - **Outside** — construct it with no `runner` and it drives real
//!   `zfs`/`zpool`/`exportfs` via [`SystemRunner`] (needs ZFS + privilege).
//! - **Inside** — pass a `runner` callable and every command is routed through
//!   *your* Python function (`runner(argv: list[str]) -> {"success", "stdout",
//!   "stderr"}`). Mock it for tests, audit it, or wrap it in `sudo`.

#![allow(clippy::result_large_err)] // BackendError is the project-wide error type.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use serde::{Deserialize, Serialize};

use nessie_backend_core::{
    BackendError, CloneBackend, SnapshotBackend, SnapshotUuid, VolumeBackend, VolumePatch,
    VolumeSpec, VolumeUuid,
};

use crate::runner::{CommandOutput, CommandRunner};
use crate::{SystemRunner, ZfsBackend, ZfsConfig};

create_exception!(
    nessie_backend_zfs,
    NessieError,
    PyException,
    "A nessie-store backend error."
);

fn err(e: BackendError) -> PyErr {
    NessieError::new_err(e.to_string())
}

fn vol_id(s: &str) -> PyResult<VolumeUuid> {
    VolumeUuid::from_str(s).map_err(|_| NessieError::new_err(format!("invalid volume uuid {s:?}")))
}

fn snap_id(s: &str) -> PyResult<SnapshotUuid> {
    SnapshotUuid::from_str(s)
        .map_err(|_| NessieError::new_err(format!("invalid snapshot uuid {s:?}")))
}

fn to_py<T: Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    pythonize::pythonize(py, value)
        .map(pyo3::Bound::unbind)
        .map_err(|e| NessieError::new_err(format!("serialize to Python failed: {e}")))
}

/// What a Python `runner` callable must return.
#[derive(Deserialize)]
struct RunResult {
    success: bool,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
}

/// A [`CommandRunner`] that delegates each command to a Python callable — the
/// inside extension point. The callable gets `argv` as a list of strings and
/// returns a mapping `{"success": bool, "stdout": str, "stderr": str}`.
struct PyCommandRunner {
    callable: Py<PyAny>,
}

impl CommandRunner for PyCommandRunner {
    fn run(&self, argv: &[&str]) -> Result<CommandOutput, BackendError> {
        let command = argv.join(" ");
        Python::attach(|py| {
            let args: Vec<&str> = argv.to_vec();
            let ret =
                self.callable
                    .call1(py, (args,))
                    .map_err(|e| BackendError::CommandFailed {
                        command: command.clone(),
                        stderr: format!("python runner raised: {e}"),
                    })?;
            let parsed: RunResult =
                pythonize::depythonize(ret.bind(py)).map_err(|e| BackendError::CommandFailed {
                    command: command.clone(),
                    stderr: format!("python runner returned an undecodable value: {e}"),
                })?;
            Ok(CommandOutput {
                success: parsed.success,
                stdout: parsed.stdout,
                stderr: parsed.stderr,
            })
        })
    }
}

/// A ZFS-backed ONTAP-shaped storage backend.
#[pyclass(name = "ZfsBackend")]
pub(crate) struct PyZfsBackend {
    inner: ZfsBackend<Arc<dyn CommandRunner>>,
}

#[pymethods]
impl PyZfsBackend {
    /// Build a backend. With no `runner` it drives real ZFS via `SystemRunner`;
    /// pass a `runner(argv) -> {success, stdout, stderr}` callable to intercept
    /// every command (tests, auditing, privilege wrapping).
    #[new]
    #[pyo3(signature = (
        pool=None, data_lif=None, nfs_clients=None, dataset_owner=None,
        dataset_mode=None, exports_dir=None, srv_root=None, runner=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        pool: Option<String>,
        data_lif: Option<String>,
        nfs_clients: Option<Vec<String>>,
        dataset_owner: Option<String>,
        dataset_mode: Option<String>,
        exports_dir: Option<String>,
        srv_root: Option<String>,
        runner: Option<Py<PyAny>>,
    ) -> Self {
        let mut cfg = ZfsConfig::default();
        if let Some(p) = pool {
            cfg.pool = p;
        }
        if let Some(d) = data_lif {
            cfg.data_lif = d;
        }
        if let Some(n) = nfs_clients {
            cfg.nfs_clients = n;
        }
        cfg.dataset_owner = dataset_owner.or(cfg.dataset_owner);
        cfg.dataset_mode = dataset_mode.or(cfg.dataset_mode);
        if let Some(e) = exports_dir {
            cfg.exports_dir = PathBuf::from(e);
        }
        if let Some(s) = srv_root {
            cfg.srv_root = PathBuf::from(s);
        }

        let runner: Arc<dyn CommandRunner> = match runner {
            Some(callable) => Arc::new(PyCommandRunner { callable }),
            None => Arc::new(SystemRunner),
        };
        Self {
            inner: ZfsBackend::new(runner, cfg),
        }
    }

    /// What this backend can do (`{snapshots, clones, replication}`).
    fn capabilities(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        to_py(py, &self.inner.capabilities())
    }

    #[pyo3(signature = (name, size_bytes=None))]
    fn create_volume(
        &self,
        py: Python<'_>,
        name: String,
        size_bytes: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        let vol = self
            .inner
            .create_volume(VolumeSpec { name, size_bytes })
            .map_err(err)?;
        to_py(py, &vol)
    }

    fn list_volumes(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        to_py(py, &self.inner.list_volumes().map_err(err)?)
    }

    fn get_volume(&self, py: Python<'_>, uuid: &str) -> PyResult<Py<PyAny>> {
        to_py(py, &self.inner.get_volume(&vol_id(uuid)?).map_err(err)?)
    }

    fn delete_volume(&self, uuid: &str) -> PyResult<()> {
        self.inner.delete_volume(&vol_id(uuid)?).map_err(err)
    }

    #[pyo3(signature = (uuid, size_bytes=None, junction_path=None, export_policy=None))]
    fn patch_volume(
        &self,
        py: Python<'_>,
        uuid: &str,
        size_bytes: Option<u64>,
        junction_path: Option<String>,
        export_policy: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let patch = VolumePatch {
            size_bytes,
            junction_path,
            export_policy,
        };
        let vol = self
            .inner
            .patch_volume(&vol_id(uuid)?, patch)
            .map_err(err)?;
        to_py(py, &vol)
    }

    /// The substrate-native data-plane handle (an NFS export for ZFS).
    fn access_handle(&self, py: Python<'_>, uuid: &str) -> PyResult<Py<PyAny>> {
        to_py(py, &self.inner.access_handle(&vol_id(uuid)?).map_err(err)?)
    }

    fn create_snapshot(&self, py: Python<'_>, vol_uuid: &str, name: &str) -> PyResult<Py<PyAny>> {
        let snap = self
            .inner
            .create_snapshot(&vol_id(vol_uuid)?, name)
            .map_err(err)?;
        to_py(py, &snap)
    }

    fn list_snapshots(&self, py: Python<'_>, vol_uuid: &str) -> PyResult<Py<PyAny>> {
        to_py(
            py,
            &self.inner.list_snapshots(&vol_id(vol_uuid)?).map_err(err)?,
        )
    }

    fn get_snapshot(&self, py: Python<'_>, vol_uuid: &str, snap_uuid: &str) -> PyResult<Py<PyAny>> {
        let snap = self
            .inner
            .get_snapshot(&vol_id(vol_uuid)?, &snap_id(snap_uuid)?)
            .map_err(err)?;
        to_py(py, &snap)
    }

    fn delete_snapshot(&self, vol_uuid: &str, snap_uuid: &str) -> PyResult<()> {
        self.inner
            .delete_snapshot(&vol_id(vol_uuid)?, &snap_id(snap_uuid)?)
            .map_err(err)
    }

    fn create_clone(
        &self,
        py: Python<'_>,
        parent_vol_uuid: &str,
        parent_snap_uuid: &str,
        new_name: &str,
    ) -> PyResult<Py<PyAny>> {
        let vol = self
            .inner
            .create_clone(
                &vol_id(parent_vol_uuid)?,
                &snap_id(parent_snap_uuid)?,
                new_name,
            )
            .map_err(err)?;
        to_py(py, &vol)
    }
}

/// The compiled extension module `nessie_backend_zfs._core`.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("NessieError", m.py().get_type::<NessieError>())?;
    m.add_class::<PyZfsBackend>()?;
    Ok(())
}
