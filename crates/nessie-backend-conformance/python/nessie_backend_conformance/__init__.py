"""Validate a Python-authored nessie-store backend (PyO3 bindings).

from nessie_backend_conformance import run_all
run_all(MyPythonBackend())   # raises ConformanceError if it doesn't conform
"""

from ._core import ConformanceError, run_all

__all__ = ["ConformanceError", "run_all"]
