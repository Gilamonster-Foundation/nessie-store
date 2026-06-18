"""The inside-extension point: a Python-authored backend is validated by run_all."""

import sys
from pathlib import Path

import pytest
from nessie_backend_conformance import ConformanceError, run_all

# Reuse the reference backend from the example (kept DRY + tested).
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "examples"))
from python_backend import DictBackend


def test_reference_backend_conforms() -> None:
    run_all(DictBackend())  # raises ConformanceError if it doesn't


def test_nonconforming_backend_is_rejected() -> None:
    class Broken(DictBackend):
        def create_volume(self, name, size_bytes):
            v = super().create_volume(name, size_bytes)
            v["name"] = "WRONG"  # violates "created volume keeps its name"
            return v

    with pytest.raises(ConformanceError):
        run_all(Broken())


def test_inconsistent_capabilities_are_rejected() -> None:
    # clones without snapshots violates the cumulative tier ordering.
    class Inconsistent(DictBackend):
        def capabilities(self):
            return {"snapshots": False, "clones": True, "replication": False}

    with pytest.raises(ConformanceError):
        run_all(Inconsistent())
