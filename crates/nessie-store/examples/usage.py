"""Runnable example for nessie_store (daemon config + identity tooling).

    python examples/usage.py

Generate a daemon config from Python, round-trip it through TOML, and mint a
stable cluster identity — no daemon process required.
"""

from nessie_store import Config, default_config_toml, mint_identity


def main() -> None:
    # The default config (what `nessie-store init` writes): mem backend, :8443.
    default = Config()
    d = default.to_dict()
    assert d["backend"] == "mem"
    assert d["ontap_version"] == "9.14.1"
    print("default config:\n" + default_config_toml())

    # Build a ZFS config from a dict — unspecified keys keep their defaults.
    cfg = Config.from_dict(
        {
            "backend": "zfs",
            "zfs_pool": "tank",
            "data_lif": "192.168.1.100",
            "zfs_nfs_clients": ["192.168.0.0/16"],
        }
    )
    toml_text = cfg.to_toml()
    print("\nzfs config.toml:\n" + toml_text)

    # Round-trips: parse the TOML back and the backend selection survives.
    parsed = Config.from_toml(toml_text)
    assert parsed.to_dict()["backend"] == "zfs"
    assert parsed.to_dict()["zfs_pool"] == "tank"

    # Mint the stable cluster identity the control plane reports.
    ident = mint_identity()
    print("\nminted identity:", ident)
    assert set(ident) == {
        "cluster_uuid",
        "svm_uuid",
        "node_uuid",
        "aggregate_uuid",
        "lif_uuid",
    }
    assert len(set(ident.values())) == 5  # all distinct

    print("\nnessie_store example OK")


if __name__ == "__main__":
    main()
