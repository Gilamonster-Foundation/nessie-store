# Deploying nessie-store

A cheap ONTAP on-ramp. Two supported ways to run it: a container (fastest) or a
systemd service on a host with ZFS. Both need ZFS privilege — the daemon shells
out to `zfs`/`zpool`/`exportfs`.

## Quick install (binary + wizard)

One line installs the release binary and launches an interactive wizard that
writes the config, optionally creates a file-backed ZFS pool, and enables the
systemd service:

```bash
curl -fsSL https://raw.githubusercontent.com/Gilamonster-Foundation/nessie-store/main/scripts/install.sh | sudo bash
```

Already have the binary? Just run the wizard:

```bash
sudo nessie-store-setup                 # interactive
sudo nessie-store-setup --non-interactive   # use env vars / defaults (automation)
```

Or install a distro package (see Releases): `sudo dpkg -i nessie-store_*.deb` /
`sudo rpm -i nessie-store-*.rpm` — both ship the systemd unit; then run
`sudo nessie-store-setup`.

## Docker (quickest)

The image bootstraps a file-backed ZFS pool on first run, so `--privileged` (and
the host's ZFS kernel module) is required.

```bash
docker run --rm -it --privileged \
  -p 8443:8443 \
  -v nessie-data:/data \
  -e NESSIE_ADMIN_PASSWORD=secret \
  -e NESSIE_VDEV_SIZE=10G \
  ghcr.io/gilamonster-foundation/nessie-store:latest
```

The API is then at `https://localhost:8443` (self-signed cert — use `curl -k`).
The entrypoint creates a sparse vdev at `/data/vdev.img`, a pool, and a default
`/data/config.toml` wired to the ZFS backend on first run; persist `/data` to
keep them.

```bash
curl -ku admin:secret https://localhost:8443/api/cluster
```

## systemd (on a ZFS host)

1. Install the binary (`cargo install nessie-store` or copy the release binary to
   `/usr/bin/nessie-store`).
2. Create a pool (once): `zpool create ontap-sim <vdev-or-disk>`.
3. Config + secrets:
   ```bash
   sudo install -d /etc/nessie-store
   sudo cp deploy/config.example.toml   /etc/nessie-store/config.toml   # edit data_lif, zfs_pool
   sudo cp deploy/environment.example   /etc/nessie-store/environment   # set NESSIE_ADMIN_PASSWORD
   sudo chmod 600 /etc/nessie-store/environment
   ```
4. Install + start the service:
   ```bash
   sudo cp deploy/nessie-store.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now nessie-store
   journalctl -u nessie-store -f
   ```

### Running non-root

The service runs as root by default (ZFS + NFS export need broad access). To run
as a dedicated user instead, grant it ZFS rights and the export capability:

```bash
zfs allow nessie create,destroy,mount,snapshot,clone,send,receive ontap-sim
# plus CAP_SYS_ADMIN for mount/exportfs (AmbientCapabilities in the unit).
```

## TLS

The daemon serves HTTPS by default. Certificate resolution order: a Vault PKI
cert (`$VAULT_PKI_CERT_DIR/<name>.crt|.key`), an existing `cert.pem`/`key.pem` in
`<data_dir>/tls/`, else a generated self-signed cert. Use `serve --no-tls` only
for local testing.

## Graduating to NetApp

This is an on-ramp, not a scale replacement. When you outgrow a single node, your
Ansible/Terraform/Trident workflows carry over unchanged to a real NetApp filer —
the REST surface was faithful the whole time.
