"""ADR-0010: identical observation input/output semantics across native bindings."""

import json
from pathlib import Path

import pytest
import sandhi_gateway as sg

CORPUS = json.loads(
    (
        Path(__file__).resolve().parents[2]
        / "fixtures/cache-read-observation-parity.json"
    ).read_text()
)


@pytest.mark.parametrize("case", CORPUS["manual"], ids=lambda case: case["name"])
def test_manual_observation_is_optional_and_lenient(case):
    gateway = sg.Gateway()
    gateway.add_virtual_key("vk")
    kwargs = {}
    if "observation" in case:
        kwargs["cache_read_observation"] = case["observation"]
    event = gateway.meter_tokens("vk", "custom", "model", 10, 2, **kwargs)
    assert event.get("cache_read_observation") == case["expected"]
    assert event["cache_read_tokens"] == 0
    assert event["tokens_in"] == 10
    assert gateway.spent("vk:vk") == 12
    assert gateway.events()[0].get("cache_read_observation") == case["expected"]
    coverage = json.loads(gateway.usage_snapshot_json("total"))[0][
        "cache_read_coverage"
    ]
    status = case["expected"]["status"] if case["expected"] else "unknown"
    assert coverage == {
        key: int(key == status)
        for key in ("reported", "absent", "malformed", "unsupported", "unknown")
    }


@pytest.mark.parametrize("case", CORPUS["manual"], ids=lambda case: case["name"])
def test_custom_parser_preserves_only_valid_observations(case):
    gateway = sg.Gateway()
    gateway.add_virtual_key("vk")
    payload = {"tokens_in": 10, "tokens_out": 2, "cache_read_tokens": 0}
    if "observation" in case:
        payload["cache_read_observation"] = case["observation"]
    gateway.register_parser("custom", lambda _response: payload)
    event = gateway.meter("vk", "custom", "model", "{}")
    assert event.get("cache_read_observation") == case["expected"]
    assert event["tokens_in"] == 10


@pytest.mark.parametrize("case", CORPUS["parsed"], ids=lambda case: case["name"])
def test_parser_event_and_snapshot_observation_parity(case):
    response = json.dumps(case["response"])
    expected = {"status": case["status"], "source": "origin_usage"}
    parsed = sg.parse_usage(case["provider"], response)
    assert parsed["cache_read_tokens"] == case["cached"]
    assert parsed["cache_read_observation"] == expected
    gateway = sg.Gateway()
    gateway.add_virtual_key("vk")
    event = gateway.meter("vk", case["provider"], "model", response)
    assert event["cache_read_tokens"] == case["cached"]
    assert event["cache_read_observation"] == expected
    row = json.loads(gateway.usage_snapshot_json("total"))[0]
    assert row["cache_read_coverage"][case["status"]] == row["calls"] == 1


def test_observation_schema_is_optional_and_declares_coverage():
    schema = json.loads(sg.chat_contract_schema_json("usage.v2"))
    assert "cache_read_observation" in schema["properties"]
    assert "cache_read_observation" not in schema.get("required", [])
    aggregate = json.loads(sg.chat_contract_schema_json("usage-aggregate.v1"))
    assert "cache_read_coverage" in aggregate["properties"]
    assert "cache_read_coverage" not in aggregate.get("required", [])
