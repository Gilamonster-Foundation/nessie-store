"""Runnable example for nessie_backend_mem.

    python examples/usage.py

Drive a full create → snapshot → FlexClone lifecycle against the in-memory
ONTAP-shaped backend, with no daemon and no privileges.
"""

from nessie_backend_mem import MemBackend


def main() -> None:
    b = MemBackend()
    assert b.capabilities()["clones"] is True

    # Backend dicts are the substrate-neutral DOMAIN shape (size_bytes, vol_type,
    # clone={parent_volume, parent_snapshot}) — not the ONTAP wire shape, which is
    # nessie-ontap-protocol's job.
    vol = b.create_volume("build-cache", size_bytes=50 * 2**30)
    print("volume:", vol)
    assert vol["name"] == "build-cache"
    assert vol["size_bytes"] == 50 * 2**30

    snap = b.create_snapshot(vol["uuid"], "pre-build")
    print("snapshot:", snap)

    clone = b.create_clone(vol["uuid"], snap["uuid"], "build-cache-pr1234")
    print("clone:", clone)
    assert clone["clone"]["parent_volume"] == "build-cache"
    assert clone["clone"]["parent_snapshot"] == "pre-build"

    assert len(b.list_volumes()) == 2  # the volume + its clone
    b.delete_volume(clone["uuid"])
    assert len(b.list_volumes()) == 1

    print("nessie_backend_mem example OK")


if __name__ == "__main__":
    main()
