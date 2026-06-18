"""Smoke tests for the nessie_ontap_protocol PyO3 bindings."""

import nessie_ontap_protocol as proto


def test_iso8601_duration() -> None:
    assert proto.iso8601_duration(0) == "PT0S"
    assert proto.iso8601_duration(45) == "PT45S"
    assert proto.iso8601_duration(3 * 3600 + 27 * 60 + 45) == "PT3H27M45S"


def test_create_response_singular_record() -> None:
    rec = {"uuid": "v1", "name": "vol1"}
    resp = proto.create_response("job-7", rec)
    assert resp["num_records"] == 1
    assert resp["record"] == rec
    assert resp["job"]["uuid"] == "job-7"
    # the job carries a HAL self-link to the poll endpoint
    assert resp["job"]["_links"]["self"]["href"] == "/api/cluster/jobs/job-7"


def test_hal_collection_counts() -> None:
    coll = proto.hal_collection([{"a": 1}, {"a": 2}], "/x")
    assert coll["num_records"] == 2
    assert coll["_links"]["self"]["href"] == "/x"


def test_delete_and_job_status() -> None:
    assert proto.delete_response("j")["job"]["state"] == "success"
    js = proto.job_status("j")
    assert js["state"] == "success" and js["message"] == "Complete"


def test_error_envelope_shape_and_optional_target() -> None:
    e = proto.error_envelope("404", "nope", "volume")
    assert e == {"error": {"code": "404", "message": "nope", "target": "volume"}}
    # target omitted when None
    e2 = proto.error_envelope("500", "boom")
    assert "target" not in e2["error"]
