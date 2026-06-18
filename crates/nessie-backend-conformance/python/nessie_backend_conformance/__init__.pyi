"""Type stubs for nessie_backend_conformance (hand-written, committed)."""

from typing import Any

class ConformanceError(Exception):
    """A Python-authored backend failed conformance."""

def run_all(backend: Any) -> None:
    """Validate `backend` against the full conformance suite.

    `backend` is any object implementing the storage-backend protocol
    (``capabilities``, ``create_volume``, ``get_volume``, … returning
    domain-shaped dicts; ``get_volume``/``get_snapshot`` return ``None`` when
    absent). Raises :class:`ConformanceError` on the first violation.
    """
