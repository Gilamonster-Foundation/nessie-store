# nessie-ontap-protocol

ONTAP REST wire shapes — the HAL envelope, the `{job,record,num_records}` create
envelope, the simplified delete job, the always-success poll, and the
ONTAP-native `{error:{code,message,target}}` envelope — for
[nessie-store](https://github.com/Gilamonster-Foundation/nessie-store). The PyO3
bindings let you assemble ONTAP-faithful response bodies from plain Python dicts.

## Install

```bash
pip install nessie-ontap-protocol      # Python
cargo add nessie-ontap-protocol        # Rust
```

## Usage (Python)

```python
import nessie_ontap_protocol as proto

# Wrap your own record dict in ONTAP's create envelope.
volume = {"uuid": "…", "name": "build-cache", "state": "online"}
created = proto.create_response("job-1", volume)
#   {"job": {"uuid": "job-1", "_links": {...}}, "record": {...}, "num_records": 1}

# A HAL collection, an error envelope, a delta duration.
coll = proto.hal_collection([volume], "/api/storage/volumes")
err = proto.error_envelope("404", "volume not found", "volume")
assert proto.iso8601_duration(3 * 3600 + 27 * 60 + 45) == "PT3H27M45S"
```

## Usage (Rust)

```rust
use nessie_ontap_protocol::{CreateResponse, iso8601_duration};

let resp = CreateResponse::new("job-1", serde_json::json!({ "name": "vol1" }));
assert_eq!(iso8601_duration(45), "PT45S");
```

Dual-licensed [MIT](../../LICENSE-MIT) OR [Apache-2.0](../../LICENSE-APACHE).
