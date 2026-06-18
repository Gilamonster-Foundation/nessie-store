"""Smoke tests for the nessie_backend_zfs PyO3 bindings.

Hermetic: a Python ``runner`` stands in for zfs/zpool, so these run in CI with no
ZFS and no privilege (and exercise the inside extension point at the same time).
"""

import pytest
from nessie_backend_zfs import NessieError, ZfsBackend


class FakeZfs:
    def __init__(self, ok: bool = True) -> None:
        self.ok = ok
        self.calls: list[list[str]] = []

    def __call__(self, argv: list[str]) -> dict[str, object]:
        self.calls.append(list(argv))
        if not self.ok:
            return {"success": False, "stdout": "", "stderr": "boom"}
        if argv[:2] == ["zfs", "get"]:
            return {"success": True, "stdout": "/ontap-sim/vol1\n"}
        if argv[1:2] == ["list"] and "snapshot" in argv:
            return {"success": True, "stdout": "ontap-sim/vol1@base\t1700000000\t0\t128K\n"}
        if argv[1:2] == ["list"]:
            return {"success": True, "stdout": "ontap-sim/vol1\t/ontap-sim/vol1\t0\t10G\t-\n"}
        return {"success": True, "stdout": "", "stderr": ""}


def backend(runner: FakeZfs) -> ZfsBackend:
    return ZfsBackend(pool="ontap-sim", data_lif="10.0.0.1", runner=runner)


def test_capabilities() -> None:
    caps = backend(FakeZfs()).capabilities()
    assert caps["snapshots"] and caps["clones"]


def test_volume_lifecycle_and_argv() -> None:
    r = FakeZfs()
    b = backend(r)
    vol = b.create_volume("vol1", size_bytes=1024)
    assert vol["name"] == "vol1"
    assert vol["size_bytes"] == 1024  # domain shape (not the wire "size")
    # The backend emitted the exact zfs argv — the inside seam saw it.
    assert ["zfs", "create", "-o", "quota=1024", "ontap-sim/vol1"] in r.calls


def test_snapshot_and_clone() -> None:
    r = FakeZfs()
    b = backend(r)
    vol = b.create_volume("vol1")
    snap = b.create_snapshot(vol["uuid"], "base")
    assert snap["name"] == "base"
    clone = b.create_clone(vol["uuid"], snap["uuid"], "child")
    assert clone["clone"]["parent_volume"] == "vol1"
    assert clone["clone"]["parent_snapshot"] == "base"
    assert ["zfs", "snapshot", "ontap-sim/vol1@base"] in r.calls


def test_access_handle_is_nfs_export() -> None:
    b = backend(FakeZfs())
    vol = b.create_volume("vol1")
    handle = b.access_handle(vol["uuid"])
    assert handle["kind"] == "nfs_export"
    assert handle["server"] == "10.0.0.1"


def test_command_failure_raises() -> None:
    b = backend(FakeZfs(ok=False))
    with pytest.raises(NessieError):
        b.create_volume("vol1")


def test_invalid_uuid_raises() -> None:
    b = backend(FakeZfs())
    with pytest.raises(NessieError):
        b.get_volume("not-a-uuid")
