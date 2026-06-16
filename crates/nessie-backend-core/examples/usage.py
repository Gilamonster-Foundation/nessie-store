"""Runnable example for nessie_backend_core.

    python examples/usage.py

Demonstrates the core domain vocabulary exposed to Python (the "outside"
scripting surface): build a volume spec, declare backend capabilities, and
construct a sparse PATCH.
"""

import nessie_backend_core as core


def main() -> None:
    # A request to create a 50 GiB volume.
    spec = core.VolumeSpec("build-cache", size_bytes=50 * 2**30)
    print(spec)
    assert spec.name == "build-cache"

    # Declare what a backend can do — and verify the tiers are self-consistent
    # (clones imply snapshots; replication implies snapshots).
    caps = core.Capabilities(snapshots=True, clones=True)
    assert caps.is_consistent()
    print(caps)

    # A backend that claims clones without snapshots is dishonest.
    bogus = core.Capabilities(snapshots=False, clones=True)
    assert not bogus.is_consistent()

    # A sparse PATCH: resize and set the NFS junction path.
    patch = core.VolumePatch(size_bytes=100 * 2**30, junction_path="/build")
    assert not patch.is_empty()
    print(patch)

    print("nessie_backend_core example OK")


if __name__ == "__main__":
    main()
