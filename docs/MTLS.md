# mTLS with agent-mesh keys

Secures the CAS peer API, REAPI and the S3 face with the keys you already use for
agent-mesh. nessie mints no identity of its own: a node is an agent-mesh `AgentKey`
issued from your `UserKey`, and trust is an SSH `authorized_keys` file.

The ONTAP REST listener is unaffected (HTTP Basic auth, no client certificate).
`mtls` secures the faces you have already enabled; it does not enable any. For a
complete config, start from [`deploy/config.example.toml`](../deploy/config.example.toml).

## 1. Bind your user key to GitHub

```bash
amesh bind github
```

Reads `<amesh-home>/user.key` and writes `<amesh-home>/user.github.sig` (the
GitHubBinding JSON), signed by your local SSH ed25519 key. Those two files feed step 2.

## 2. Issue the node identity

```bash
nessie-store identity issue --config nessie-store.toml \
  --user-key "$AMESH_HOME/user.key" --binding "$AMESH_HOME/user.github.sig"
```

Set `AMESH_HOME` to your amesh home directory. Optional: `--role` and `--host`
(labels in the signed chain; both default to `nessie`), e.g. `--host nessie-1`.
Writes `agent.seed`, `agent.chain.json` and `binding.json` under
`<data_dir>/identity/`. The seed is never printed. Repeat per node.

## 3. Say whom to trust: `authorized_keys`

`<data_dir>/authorized_keys`, or `[tls] authorized_keys = "..."`. One line per key:

```
ssh-ed25519 AAAA... alice@laptop
```

A peer or client is admitted when its certificate chain verifies and its operator's
GitHub binding is signed by a key listed here. Lines carrying options, other key
types or junk are skipped with a warning; blank and `#` lines are ignored. A missing
file trusts **nobody**.

**The file is read once, at startup.** Adding or removing a line has no effect on a
running daemon: restart every affected node after editing it. A removed operator can
still complete new handshakes until that restart. The same goes for any change to
`[tls]`. To cut off one agent (or a generation) without touching the operator, use
revocation (step 7).

## 4. Turn it on

A minimal, startable config for a node serving the peer API, REAPI and mTLS:

```toml
[tls]
mtls = true
peer_listen = "0.0.0.0:8443"     # mTLS peer API (default)
names = ["nessie-1.example.net"] # extra DNS names/IPs in the S3/REAPI certificate

[cas]
byte_budget = 107374182400       # required
peer_api = true
peers = [{ url = "https://nessie-2.example.net:8443", durable = true }]

[reapi]
enabled = true                   # gRPC on 0.0.0.0:8980 by default
```

- `/cas/v1/*` is then served only on `peer_listen`, over mutual TLS (TLS 1.3).
- Peer URLs must be `https://`; startup fails otherwise. The peer client never
  follows redirects and never speaks plain HTTP.
- REAPI requires a client certificate from an admitted operator.
- S3, if you enable it (`[s3]`, with a non-default `NESSIE_S3_SECRET_KEY`), serves
  HTTPS with the node certificate and keeps SigV4.
- The S3/REAPI certificate always carries `localhost`, `127.0.0.1`, `::1` and `names`.
  Peers verify each other by key and chain, not by DNS name.

## 5. Bazel clients

```bash
nessie-store identity export-client-cert --config nessie-store.toml --out creds
```

`client.crt`/`client.key` are a delegated sub-agent of the node (fresh key, same
operator); the node seed never leaves `identity/`. Existing files are not overwritten.

Bazel also needs the node's **public** certificate as its trust root. Fetch it from
the running REAPI port and check it against `identity show` (step 6) before trusting it:

```bash
openssl s_client -connect nessie-1.example.net:8980 </dev/null 2>/dev/null \
  | openssl x509 -out node.pem
openssl x509 -in node.pem -noout -fingerprint -sha256
```

The fingerprint (colon-separated, upper case) must equal `certificate sha256` from
`identity show`. Then:

```bash
bazel build --remote_cache=grpcs://nessie-1.example.net:8980 \
  --tls_client_certificate=creds/client.crt \
  --tls_client_key=creds/client.key \
  --tls_certificate=node.pem //...
```

Never use `client.crt` as `node.pem`, and never export the node's seed.

## 6. S3 fingerprint pinning

```bash
nessie-store identity show --config nessie-store.toml
# agent <fingerprint>
# certificate sha256 <hex>
```

Pin that SHA-256 in S3 clients that support it. The daemon logs it at startup too.

## 7. Revocation

Revoke one agent, or bump the generation, with `amesh`; nessie reads the resulting
log and honours it without a restart.

```toml
[tls]
revocation_log = "/etc/nessie-store/revocation.json"   # `amesh revocation export`
```

- Every handshake (peer API, REAPI, and the peer client judging servers) is checked
  against the log as it stands then: a revoked agent, or a chain outside the current
  generation, is refused.
- **Open connections are rechecked on every request** (peer API and REAPI): the next
  request after the log gains a revocation gets `403`, with no reconnect.
- **The peer client does not reuse connections** and TLS session resumption is off, so
  every outgoing request re-judges the server against your log: revoking a server there
  stops the next fetch or push, without any change on that server. The price is one
  handshake per peer request.
- **Fail closed.** With `revocation_log` set, a log that is missing, unreadable,
  invalid, another operator's, or older than the head last seen refuses every
  handshake and request until fixed. At startup that stops the daemon. The last head
  seen is kept in `<log name>.seen.json` beside the log (agent-mesh's rule), so
  swapping in an older copy cannot un-revoke anyone.
- Unset, nothing is checked and startup logs a warning.
- `nessie-store identity show` prints the loaded log's counter and generation, or
  `no revocation log configured`.
- The log is the **operator's own**: it applies to agents of that operator. S3 has no
  client certificates, so there is nothing to revoke there; it stays on SigV4.
