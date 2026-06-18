"""Smoke tests for the nessie_backend_mem PyO3 bindings."""

import pytest
from nessie_backend_mem import MemBackend, NessieError


def test_volume_lifecycle() -> None:
    b = MemBackend()
    vol = b.create_volume("vol1", size_bytes=1024)
    assert vol["name"] == "vol1"
    assert vol["size_bytes"] == 1024  # domain shape (not the wire "size")
    got = b.get_volume(vol["uuid"])
    assert got["uuid"] == vol["uuid"]
    assert len(b.list_volumes()) == 1
    b.delete_volume(vol["uuid"])
    with pytest.raises(NessieError):
        b.get_volume(vol["uuid"])


def test_snapshot_and_clone() -> None:
    b = MemBackend()
    vol = b.create_volume("parent")
    snap = b.create_snapshot(vol["uuid"], "base")
    assert snap["name"] == "base"
    clone = b.create_clone(vol["uuid"], snap["uuid"], "child")
    assert clone["clone"]["parent_volume"] == "parent"
    assert clone["clone"]["parent_snapshot"] == "base"


def test_capabilities() -> None:
    caps = MemBackend().capabilities()
    assert caps["snapshots"] and caps["clones"]


def test_errors_raise_nessie_error() -> None:
    b = MemBackend()
    with pytest.raises(NessieError):
        b.get_volume("00000000-0000-0000-0000-000000000000")
    with pytest.raises(NessieError):
        b.get_volume("not-a-uuid")
