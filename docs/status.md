
## CAS peer plane (static peers)

`[cas] peers = [{ url = "http://host:port", durable = true }]` turns on fetch-through
(`SwarmCas`), a HEAD-backed durability oracle and a PUT replicator; `peer_api = true`
mounts `GET|HEAD|PUT /cas/v1/blobs/{algo}:{hex}` so others can reach this node.
**Lab only: the routes are unauthenticated, and the peer client does not trust the
daemon's self-signed certificate, so peers must reach each other over plain `http`
(run the daemon with `--no-tls`). mTLS is the fast-follow.** Integrity never depends on the wire: every byte is verified against
its digest before it is stored or served.
