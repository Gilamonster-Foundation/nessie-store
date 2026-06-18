"""ZFS-backed ONTAP-shaped storage backend (PyO3 bindings).

Drive real ZFS (no runner), or intercept every command from Python (the
``runner`` callable) — useful for tests, auditing, or privilege wrapping:

    from nessie_backend_zfs import ZfsBackend

    # Outside: real zfs/zpool/exportfs (needs ZFS + privilege).
    b = ZfsBackend(pool="ontap-sim", data_lif="192.168.1.100")

    # Inside: route commands through your own function.
    def fake(argv): return {"success": True, "stdout": "", "stderr": ""}
    b = ZfsBackend(pool="tank", runner=fake)
    vol = b.create_volume("vol1", size_bytes=1 << 30)
"""

from ._core import NessieError, ZfsBackend

__all__ = ["NessieError", "ZfsBackend"]
