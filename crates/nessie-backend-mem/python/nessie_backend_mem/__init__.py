"""In-memory ONTAP-shaped reference backend (PyO3 bindings).

from nessie_backend_mem import MemBackend
b = MemBackend()
vol = b.create_volume("vol1", size_bytes=1 << 30)
"""

from ._core import MemBackend, NessieError

__all__ = ["MemBackend", "NessieError"]
