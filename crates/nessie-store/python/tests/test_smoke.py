"""Smoke tests for the nessie_store PyO3 bindings (config + identity)."""

import pytest
from nessie_store import Config, NessieError, default_config_toml, mint_identity


def test_default_config() -> None:
    d = Config().to_dict()
    assert d["backend"] == "mem"
    assert d["admin_username"] == "admin"
    assert d["ontap_version"] == "9.14.1"


def test_from_dict_fills_defaults() -> None:
    cfg = Config.from_dict({"backend": "zfs", "zfs_pool": "tank"})
    d = cfg.to_dict()
    assert d["backend"] == "zfs"
    assert d["zfs_pool"] == "tank"
    assert d["cluster_name"] == "nessie-store"  # defaulted


def test_toml_roundtrip() -> None:
    cfg = Config.from_dict({"backend": "zfs", "svm_name": "svmX"})
    back = Config.from_toml(cfg.to_toml())
    assert back.to_dict()["backend"] == "zfs"
    assert back.to_dict()["svm_name"] == "svmX"


def test_default_config_toml_parses() -> None:
    text = default_config_toml()
    assert 'backend = "mem"' in text
    assert Config.from_toml(text).to_dict()["backend"] == "mem"


def test_bad_toml_raises() -> None:
    with pytest.raises(NessieError):
        Config.from_toml("listen = 12345")  # wrong type for a SocketAddr


def test_mint_identity_is_five_distinct_uuids() -> None:
    ident = mint_identity()
    assert set(ident) == {
        "cluster_uuid",
        "svm_uuid",
        "node_uuid",
        "aggregate_uuid",
        "lif_uuid",
    }
    assert len(set(ident.values())) == 5
    # Two mints never collide.
    assert mint_identity()["cluster_uuid"] != ident["cluster_uuid"]
