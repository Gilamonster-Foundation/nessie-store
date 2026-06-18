"""ONTAP REST envelope/HAL builders (PyO3 bindings).

Assemble ONTAP-faithful response bodies from plain Python dicts.
"""

from ._core import (
    create_response,
    delete_response,
    error_envelope,
    hal_collection,
    iso8601_duration,
    job_status,
)

__all__ = [
    "create_response",
    "delete_response",
    "error_envelope",
    "hal_collection",
    "iso8601_duration",
    "job_status",
]
