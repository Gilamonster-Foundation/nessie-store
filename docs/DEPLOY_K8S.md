# Deploying nessie-store on Kubernetes / k3s

Run the daemon as a pod backed by a PersistentVolume, exposing the ONTAP REST
API as a Service. This is the "k3s deploy surface" — for operators who already
run a homelab cluster and want their ONTAP on-ramp there instead of on a bare
host or in plain Docker. For the host/systemd and Docker paths see
[DEPLOY.md](DEPLOY.md).

The manifests live in [`deploy/k8s/`](../deploy/k8s/):

| File | Role |
|------|------|
| `namespace.yaml` | the `nessie-store` namespace |
| `secret.example.yaml` | admin (basic-auth) password — **example, create your own** |
| `pvc.yaml` | the `/data` claim (vdev + config + identity) — default StorageClass |
| `pv.example.yaml` | optional static, node-pinned PV |
| `deployment.yaml` | the privileged daemon pod (replicas: 1, Recreate) |
| `service.yaml` | HTTPS/8443 control-plane Service |
| `kustomization.yaml` | `kubectl apply -k` aggregator |

## Prerequisites

- **The container image** `ghcr.io/gilamonster-foundation/nessie-store:latest`
  (built + pushed by the release pipeline).
- **ZFS on the node.** The pod is privileged and mounts `/dev/zfs`; the entrypoint
  creates a file-backed ZFS pool inside the PVC. The host node must have the ZFS
  kernel module loaded. On a cluster with no host ZFS, use the
  [mem-backend variant](#no-zfs-control-plane-only) below.
- A default StorageClass (k3s ships `local-path`), or a static PV you provide.

## Install

```bash
# 1. Namespace + the admin password (kept out of git).
kubectl create namespace nessie-store
kubectl -n nessie-store create secret generic nessie-store-admin \
  --from-literal=admin-password='choose-a-real-password'

# 2. Everything else in one shot.
kubectl apply -k deploy/k8s/

# 3. Watch it come up.
kubectl -n nessie-store rollout status deploy/nessie-store
kubectl -n nessie-store logs -f deploy/nessie-store
```

On first start the entrypoint creates `/data/vdev.img`, a pool (`ontap-sim`), and
a default `/data/config.toml` wired to the ZFS backend. Because all three live on
the PVC, a pod restart re-imports the pool and keeps the minted cluster identity.

## Reaching the API

```bash
kubectl -n nessie-store port-forward svc/nessie-store 8443:8443
curl -ku admin:choose-a-real-password https://localhost:8443/api/cluster
```

For a LAN-reachable endpoint (so Trident / the `netapp.ontap` Ansible collection
can drive it), change the Service `type` to `LoadBalancer` — on k3s, ServiceLB
surfaces it on the node IP.

## Storage: PV and PVC

`pvc.yaml` requests `20Gi` `ReadWriteOnce` from the **default StorageClass**. On
k3s that is `local-path`, which provisions a node-local directory automatically —
nothing else to do.

To control placement explicitly, apply the static PV instead:

```bash
# On the chosen node:
sudo mkdir -p /var/lib/nessie-store-data
# Edit pv.example.yaml: set the node hostname; then:
kubectl apply -f deploy/k8s/pv.example.yaml
# Edit pvc.yaml: set `storageClassName: nessie-store-local` so it binds the PV.
```

`ReadWriteOnce` + `replicas: 1` + the `Recreate` strategy are load-bearing: a ZFS
pool has exactly one owner, so two pods must never mount this volume at once.

## The NFS data plane

nessie-store serves NFS itself via an **embedded userspace NFSv3 server**
(`nfs_enabled = true`, port `2049`) — **no host kernel NFS server is needed on
the node**, which makes it far friendlier to run in a pod than the old
`exportfs`/`rpc.nfsd` model. Add the NFS port to the Service (or a second
`LoadBalancer`/`NodePort`) so clients can reach it:

```yaml
ports:
  - name: https        # ONTAP REST (control)
    port: 8443
    targetPort: 8443
  - name: nfs          # embedded NFSv3 (data)
    port: 2049
    targetPort: 2049
```

Pods mount it with the embedded-server options (no rpcbind/NLM):

```
nfsvers=3,proto=tcp,port=2049,mountport=2049,nolock,noacl
```

For Trident's ontap-nas backend, pin those via `nfsMountOptions` and
`nfsVersion: "3"`. The node still needs the NFS **client** (`nfs-common` + the
`nfs` kernel module) — that is the kubelet's mount client, not an NFS server.
This mirrors real ONTAP: control plane and data plane on separate ports.

## No ZFS? (control plane only)

To evaluate the ONTAP REST surface on a cluster without host ZFS, run the mem
backend — no privilege, no `/dev/zfs`, no pool. The backend is chosen in the
config (`backend = "mem"`), and the image entrypoint always tries to bootstrap a
pool first, so override the container `command` to go straight to the binary and
feed it a mem config from a ConfigMap. In `deployment.yaml`:

- drop `securityContext.privileged`, the `dev-zfs` volume + its mount, and the
  `NESSIE_ZFS_POOL` / `NESSIE_VDEV_SIZE` env;
- bypass the ZFS-bootstrapping entrypoint and point at the config:

  ```yaml
  command: ["/usr/bin/nessie-store"]
  args: ["serve", "--config", "/etc/nessie-store/config.toml", "--no-tls"]
  volumeMounts:
    - name: config
      mountPath: /etc/nessie-store
  ```

- supply that config (and drop the PVC — mem holds nothing on disk):

  ```yaml
  apiVersion: v1
  kind: ConfigMap
  metadata:
    name: nessie-store-config
    namespace: nessie-store
  data:
    config.toml: |
      listen = "0.0.0.0:8443"
      data_dir = "/data"
      backend = "mem"
      admin_username = "admin"
      admin_password = "admin"   # overridden by NESSIE_ADMIN_PASSWORD
      cluster_name = "nessie-store"
      svm_name = "svm0"
      node_serial_number = "SIM-1-0000000001"
      ontap_version = "9.14.1"
      data_lif = "0.0.0.0"
  ```

  …mounted via a `config` volume (`configMap: { name: nessie-store-config }`).

State is not durable across restarts in this mode — it is for kicking the tires,
not for storing data.

## Graduating to NetApp

Same promise as every other surface: this is an on-ramp, not a scale
replacement. When you outgrow it, the Trident/Ansible/Terraform workflows you
built against this endpoint carry over unchanged to a real NetApp filer.
