"""Write a nessie-store storage backend in Python — and prove it conforms.

    python examples/python_backend.py

`DictBackend` is a complete, dict-backed backend implementing the protocol the
Rust conformance suite drives. `run_all` wraps it in a Rust adapter and runs the
exact same suites a Rust backend passes — the "inside" extension point: extend
the system in Python without touching Rust.

Protocol (all values are plain domain-shaped dicts):
- ``capabilities() -> {snapshots, clones, replication}``
- ``create_volume(name, size_bytes) -> volume``
- ``list_volumes() -> [volume]``
- ``get_volume(uuid) -> volume | None``   (None ⇒ not found)
- ``delete_volume(uuid)``
- ``patch_volume(uuid, size_bytes, junction_path, export_policy) -> volume``
- ``access_handle(uuid) -> {"kind": ...}``
- ``create_snapshot(vol, name) -> snapshot`` / ``list_snapshots(vol) -> [snapshot]``
- ``get_snapshot(vol, snap) -> snapshot | None`` / ``delete_snapshot(vol, snap)``
- ``create_clone(parent_vol, parent_snap, new_name) -> volume``

A ``volume`` is ``{uuid, name, state, style, vol_type, size_bytes?, clone?}``;
a ``snapshot`` is ``{uuid, name, size_consumed, create_time?}``.
"""

import uuid as _uuid

from nessie_backend_conformance import run_all


class DictBackend:
    """A dict-backed reference backend implementing the full clone tier."""

    def __init__(self) -> None:
        self._vols: dict[str, dict] = {}
        self._snaps: dict[str, dict[str, dict]] = {}
        self._names: set[str] = set()

    def capabilities(self) -> dict:
        return {"snapshots": True, "clones": True, "replication": False}

    @staticmethod
    def _vol(uuid: str, name: str, size_bytes=None, clone=None) -> dict:
        v = {
            "uuid": uuid,
            "name": name,
            "state": "online",
            "style": "flexvol",
            "vol_type": "rw",
        }
        if size_bytes is not None:
            v["size_bytes"] = size_bytes
        if clone is not None:
            v["clone"] = clone
        return v

    def create_volume(self, name: str, size_bytes) -> dict:
        if name in self._names:
            raise ValueError(f"volume {name!r} already exists")
        u = str(_uuid.uuid4())
        v = self._vol(u, name, size_bytes)
        self._vols[u] = v
        self._names.add(name)
        self._snaps[u] = {}
        return v

    def list_volumes(self) -> list:
        return list(self._vols.values())

    def get_volume(self, uuid: str):
        return self._vols.get(uuid)

    def delete_volume(self, uuid: str) -> None:
        v = self._vols.pop(uuid, None)
        if v is not None:
            self._names.discard(v["name"])
            self._snaps.pop(uuid, None)

    def patch_volume(self, uuid, size_bytes, junction_path, export_policy) -> dict:
        v = self._vols[uuid]
        if size_bytes is not None:
            v["size_bytes"] = size_bytes
        return v

    def access_handle(self, uuid: str) -> dict:
        return {"kind": "in_memory"}

    def create_snapshot(self, vol: str, name: str) -> dict:
        su = str(_uuid.uuid4())
        s = {"uuid": su, "name": name, "size_consumed": 0}
        self._snaps[vol][su] = s
        return s

    def list_snapshots(self, vol: str) -> list:
        return list(self._snaps.get(vol, {}).values())

    def get_snapshot(self, vol: str, snap: str):
        return self._snaps.get(vol, {}).get(snap)

    def delete_snapshot(self, vol: str, snap: str) -> None:
        self._snaps.get(vol, {}).pop(snap, None)

    def create_clone(self, parent_vol: str, parent_snap: str, new_name: str) -> dict:
        pv = self._vols[parent_vol]
        ps = self._snaps[parent_vol][parent_snap]
        u = str(_uuid.uuid4())
        v = self._vol(
            u,
            new_name,
            clone={"parent_volume": pv["name"], "parent_snapshot": ps["name"]},
        )
        self._vols[u] = v
        self._names.add(new_name)
        self._snaps[u] = {}
        return v


def main() -> None:
    run_all(DictBackend())
    print("DictBackend conforms ✓")


if __name__ == "__main__":
    main()
