"""End-to-end tests for the `sandhi_gateway` in-process middleware.

Runnable with pytest OR directly (`python tests/test_gateway.py`). Requires the wheel to be
built/installed first (`maturin develop -m bindings/python/Cargo.toml`).
"""

import json

import sandhi_gateway as sg


def test_wire_contract_version():
    assert sg.wire_contract_version() == "1"


def test_parse_usage_openai_cache_split():
    resp = json.dumps(
        {"usage": {"prompt_tokens": 100, "completion_tokens": 20,
                   "prompt_tokens_details": {"cached_tokens": 60}}}
    )
    u = sg.parse_usage("openai", resp)
    assert u["tokens_in"] == 40  # 100 total - 60 cached
    assert u["cache_read_tokens"] == 60
    assert u["tokens_out"] == 20
    assert u["cache_creation_tokens"] == 0


def test_parse_usage_anthropic_direct_split():
    resp = json.dumps(
        {"usage": {"input_tokens": 12, "output_tokens": 5,
                   "cache_creation_input_tokens": 3, "cache_read_input_tokens": 7}}
    )
    assert sg.parse_usage("anthropic", resp) == {
        "tokens_in": 12, "tokens_out": 5,
        "cache_creation_tokens": 3, "cache_read_tokens": 7,
    }


def test_gateway_meters_attributes_and_budgets():
    gw = sg.Gateway()
    gw.add_virtual_key("vk_alice", subject="alice", group="platform", upstream="anthropic")
    gw.set_budget("group:platform", 1000)

    resp = json.dumps(
        {"usage": {"input_tokens": 220, "output_tokens": 80,
                   "cache_creation_input_tokens": 0, "cache_read_input_tokens": 40}}
    )
    ev = gw.meter("vk_alice", "anthropic", "claude-x", resp, session_id="conv_7")

    assert ev["subject_id"] == "alice"
    assert ev["group_id"] == "platform"
    assert ev["virtual_key_id"] == "vk_alice"
    assert ev["session_id"] == "conv_7"
    assert ev["provider"] == "anthropic"
    assert ev["backend"] == "external"
    assert ev["schema_version"] == "1"
    assert ev["tokens_in"] == 220
    assert ev["cache_read_tokens"] == 40
    assert ev["gpu_seconds"] is None

    # billable = 220 + 80 = 300, recorded on the group scope
    assert gw.spent("group:platform") == 300
    assert len(gw.events()) == 1
    # a big next call is now over the 1000 budget (300 + 800 > 1000)
    assert gw.check_budget("group:platform", 800) is False
    assert gw.check_budget("group:platform", 700) is True


def test_unknown_virtual_key_raises_keyerror():
    gw = sg.Gateway()
    try:
        gw.meter("vk_nope", "openai", "m", json.dumps({"usage": {}}))
        raise AssertionError("expected KeyError")
    except KeyError:
        pass


def test_jsonl_sink_writes_events(tmp_path=None):
    import tempfile
    import os

    d = tempfile.mkdtemp()
    path = os.path.join(d, "usage.jsonl")
    gw = sg.Gateway(path)
    gw.add_virtual_key("vk", subject="s", group="g", upstream="openai")
    resp = json.dumps({"usage": {"prompt_tokens": 10, "completion_tokens": 5}})
    gw.meter("vk", "openai", "gpt-x", resp)
    gw.meter("vk", "openai", "gpt-x", resp)
    with open(path) as fh:
        lines = [json.loads(x) for x in fh if x.strip()]
    assert len(lines) == 2
    assert all(line["schema_version"] == "1" for line in lines)


def test_meter_tokens_bypasses_parsing():
    gw = sg.Gateway()
    gw.add_virtual_key("vk", subject="s", group="g", upstream="x")
    ev = gw.meter_tokens("vk", "custom-provider", "m", 11, 7, 0, 2, "sess")
    assert ev["tokens_in"] == 11
    assert ev["tokens_out"] == 7
    assert ev["cache_read_tokens"] == 2
    assert ev["provider"] == "custom-provider"
    assert ev["session_id"] == "sess"
    assert gw.spent("group:g") == 18  # 11 + 7


def test_register_parser_host_callback():
    gw = sg.Gateway()
    gw.add_virtual_key("vk", subject="s", group="g", upstream="x")
    calls = []

    def my_parser(response_json):
        d = json.loads(response_json)
        calls.append(d)
        return {"tokens_in": d["in"], "tokens_out": d["out"],
                "cache_creation_tokens": 0, "cache_read_tokens": 5}

    gw.register_parser("weirdprovider", my_parser)
    ev = gw.meter("vk", "weirdprovider", "m", json.dumps({"in": 30, "out": 12}))
    assert ev["tokens_in"] == 30
    assert ev["tokens_out"] == 12
    assert ev["cache_read_tokens"] == 5
    assert len(calls) == 1  # the host callback was invoked


if __name__ == "__main__":
    for _name, _fn in list(globals().items()):
        if _name.startswith("test_") and callable(_fn):
            _fn()
            print(f"ok {_name}")
    print("ALL PASS")
