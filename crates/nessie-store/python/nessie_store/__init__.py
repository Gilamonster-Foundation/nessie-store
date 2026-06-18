"""nessie-store daemon config + identity tooling (PyO3 bindings).

Build, validate, and serialize the daemon's ``config.toml`` from Python, and mint
the stable cluster identity — useful for automation and the container/k8s deploy
paths:

    from nessie_store import Config, mint_identity

    cfg = Config.from_dict({"backend": "zfs", "zfs_pool": "tank"})
    print(cfg.to_toml())          # ready for `nessie-store serve --config`
    ident = mint_identity()       # {'cluster_uuid': ..., 'svm_uuid': ..., ...}
"""

from ._core import Config, NessieError, default_config_toml, mint_identity

__all__ = ["Config", "NessieError", "default_config_toml", "mint_identity"]
