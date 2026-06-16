"""Smoke tests for the nessie_backend_core PyO3 bindings."""

import nessie_backend_core as core


def test_volume_spec_roundtrip() -> None:
    s = core.VolumeSpec("v1", size_bytes=1024)
    assert s.name == "v1"
    assert s.size_bytes == 1024
    # default size is None
    assert core.VolumeSpec("v2").size_bytes is None


def test_capabilities_consistency() -> None:
    assert core.Capabilities(snapshots=True, clones=True).is_consistent()
    assert core.Capabilities().is_consistent()  # volume-only
    bogus = core.Capabilities(snapshots=False, clones=True)
    assert not bogus.is_consistent()


def test_patch_empty_detection() -> None:
    assert core.VolumePatch().is_empty()
    assert not core.VolumePatch(size_bytes=1).is_empty()
    assert core.VolumePatch(junction_path="/x").junction_path == "/x"


def test_equality_and_repr() -> None:
    a = core.VolumeSpec("v1", size_bytes=1024)
    b = core.VolumeSpec("v1", size_bytes=1024)
    assert a == b
    assert "build" not in repr(a)
    assert repr(a).startswith("VolumeSpec(")


def test_error_is_exception_subclass() -> None:
    assert issubclass(core.NessieError, Exception)
