"""Runnable example for nessie_ontap_protocol.

    python examples/usage.py

Assemble ONTAP-faithful response envelopes from plain dicts.
"""

import nessie_ontap_protocol as proto


def main() -> None:
    # A volume record (the caller's own dict) wrapped in the create envelope.
    volume = {
        "uuid": "11111111-1111-1111-1111-111111111111",
        "name": "build-cache",
        "state": "online",
    }
    created = proto.create_response("job-1", volume)
    assert created["num_records"] == 1
    assert created["record"]["name"] == "build-cache"
    assert created["job"]["uuid"] == "job-1"
    print(created)

    # A HAL collection of records.
    coll = proto.hal_collection([volume], "/api/storage/volumes")
    assert coll["num_records"] == 1
    print(coll)

    # The ONTAP-native error envelope + a delta duration.
    err = proto.error_envelope("404", "volume not found", "volume")
    assert err["error"]["target"] == "volume"
    assert proto.iso8601_duration(3 * 3600 + 27 * 60 + 45) == "PT3H27M45S"

    print("nessie_ontap_protocol example OK")


if __name__ == "__main__":
    main()
