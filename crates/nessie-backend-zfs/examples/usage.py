"""Runnable example for nessie_backend_zfs.

    python examples/usage.py

Drives a create → snapshot → FlexClone lifecycle through the ZFS backend WITHOUT
real ZFS, by supplying a Python ``runner`` (the *inside* extension point): every
zfs/zpool command the backend would run is routed to your function instead. The
same class drives real ZFS if you construct it with no runner — see the note at
the bottom.
"""

from nessie_backend_zfs import ZfsBackend


class FakeZfs:
    """A stand-in for zfs/zpool: records argv, returns canned output.

    This is the inside extension point — nessie hands every command to us, so we
    can mock it (here), audit it, or wrap it in sudo.
    """

    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def __call__(self, argv: list[str]) -> dict[str, object]:
        self.calls.append(list(argv))
        # The few reads the backend parses; everything else just succeeds.
        if argv[:2] == ["zfs", "get"]:  # access_handle → mountpoint
            return {"success": True, "stdout": "/ontap-sim/build-cache\n"}
        if argv[1:2] == ["list"] and "snapshot" in argv:  # list_snapshots
            snap = "ontap-sim/build-cache@pre-build\t1700000000\t0\t128K\n"
            return {"success": True, "stdout": snap}
        if argv[1:2] == ["list"]:  # get_volume / list_volumes
            vol = "ontap-sim/build-cache\t/ontap-sim/build-cache\t0\t10G\t-\n"
            return {"success": True, "stdout": vol}
        return {"success": True, "stdout": "", "stderr": ""}


def main() -> None:
    runner = FakeZfs()
    b = ZfsBackend(pool="ontap-sim", data_lif="192.168.1.100", runner=runner)
    assert b.capabilities()["clones"] is True

    vol = b.create_volume("build-cache", size_bytes=10 * 2**30)
    print("volume:", vol)
    assert vol["name"] == "build-cache"
    assert vol["size_bytes"] == 10 * 2**30

    snap = b.create_snapshot(vol["uuid"], "pre-build")
    print("snapshot:", snap)

    clone = b.create_clone(vol["uuid"], snap["uuid"], "build-cache-pr1234")
    print("clone:", clone)
    assert clone["clone"]["parent_volume"] == "build-cache"
    assert clone["clone"]["parent_snapshot"] == "pre-build"

    handle = b.access_handle(vol["uuid"])
    print("access handle:", handle)
    assert handle["kind"] == "nfs_export"
    assert handle["server"] == "192.168.1.100"

    # The payoff: the exact zfs argv the backend emitted — auditable from Python.
    print("\ncommands nessie ran:")
    for argv in runner.calls:
        print("  ", " ".join(argv))
    assert ["zfs", "create", "-o", "quota=10737418240", "ontap-sim/build-cache"] in runner.calls
    assert ["zfs", "snapshot", "ontap-sim/build-cache@pre-build"] in runner.calls
    assert [
        "zfs",
        "clone",
        "ontap-sim/build-cache@pre-build",
        "ontap-sim/build-cache-pr1234",
    ] in runner.calls

    print("\nnessie_backend_zfs example OK")

    # For REAL ZFS (needs zfs/zpool + privilege), construct with no runner:
    #   b = ZfsBackend(pool="ontap-sim", data_lif="192.168.1.100")
    #   b.create_volume("build-cache", size_bytes=10 * 2**30)


if __name__ == "__main__":
    main()
