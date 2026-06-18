//! Python bindings: generate/validate daemon config + mint cluster identity.
//!
//! The daemon itself is a binary, but its *library* surface is useful from
//! Python for automation: build and validate a `config.toml` (the same one
//! `nessie-store serve --config` consumes), and mint the stable cluster identity.
//! This is the **outside** extension point for the daemon crate — script the
//! daemon's configuration without shelling out.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use serde::Serialize;

use crate::config::Config;
use crate::identity::Identity;

create_exception!(
    nessie_store,
    NessieError,
    PyException,
    "A nessie-store configuration error."
);

fn to_py<T: Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    pythonize::pythonize(py, value)
        .map(pyo3::Bound::unbind)
        .map_err(|e| NessieError::new_err(format!("serialize to Python failed: {e}")))
}

/// A daemon configuration — the contents of `config.toml`.
#[pyclass(name = "Config")]
pub(crate) struct PyConfig {
    inner: Config,
}

#[pymethods]
impl PyConfig {
    /// A fresh config with every field at its default (mem backend, :8443, …).
    #[new]
    fn new() -> Self {
        Self {
            inner: Config::default(),
        }
    }

    /// Parse a config from a TOML string (missing fields take their defaults).
    #[staticmethod]
    fn from_toml(text: &str) -> PyResult<Self> {
        let inner: Config = toml::from_str(text)
            .map_err(|e| NessieError::new_err(format!("parsing config: {e}")))?;
        Ok(Self { inner })
    }

    /// Build a config from a dict; unspecified keys take their defaults.
    #[staticmethod]
    fn from_dict(obj: &Bound<'_, PyAny>) -> PyResult<Self> {
        let inner: Config = pythonize::depythonize(obj)
            .map_err(|e| NessieError::new_err(format!("decoding config dict: {e}")))?;
        Ok(Self { inner })
    }

    /// Serialize back to a TOML string (what `init` writes / `serve` reads).
    fn to_toml(&self) -> String {
        self.inner.to_toml()
    }

    /// The config as a plain dict.
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        to_py(py, &self.inner)
    }

    fn __repr__(&self) -> String {
        format!(
            "Config(backend={:?}, listen={}, cluster_name={:?})",
            self.inner.backend, self.inner.listen, self.inner.cluster_name
        )
    }
}

/// The default config serialized to TOML — what `nessie-store init` writes.
#[pyfunction]
fn default_config_toml() -> String {
    Config::default().to_toml()
}

/// Mint a fresh set of stable cluster UUIDs (cluster/svm/node/aggregate/lif).
///
/// The daemon mints these once and persists them; this exposes the same minting
/// for tooling (e.g. pre-seeding `identity.json`).
#[pyfunction]
fn mint_identity(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &Identity::mint())
}

/// The compiled extension module `nessie_store._core`.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("NessieError", m.py().get_type::<NessieError>())?;
    m.add_class::<PyConfig>()?;
    m.add_function(wrap_pyfunction!(default_config_toml, m)?)?;
    m.add_function(wrap_pyfunction!(mint_identity, m)?)?;
    Ok(())
}
