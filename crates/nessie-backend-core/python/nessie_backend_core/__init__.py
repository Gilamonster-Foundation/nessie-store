"""Core domain vocabulary for nessie-store (PyO3 bindings).

Re-exports the compiled types from the ``_core`` extension module so consumers
write ``import nessie_backend_core as core; core.VolumeSpec(...)``.
"""

from ._core import (
    Capabilities,
    CloneOrigin,
    NessieError,
    Snapshot,
    Volume,
    VolumePatch,
    VolumeSpec,
)

__all__ = [
    "Capabilities",
    "CloneOrigin",
    "NessieError",
    "Snapshot",
    "Volume",
    "VolumePatch",
    "VolumeSpec",
]
