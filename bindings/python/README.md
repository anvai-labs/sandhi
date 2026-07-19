# sandhi-gateway

Python binding for [**Sandhi**](https://github.com/anvai-labs/sandhi) — *the metering layer
for AI agents*. The Rust core, in-process via PyO3: **virtual keys, budgets, and neutral
usage-event metering** with zero network hop. Keep making your own provider calls; hand the
response to Sandhi to meter it.

```bash
pip install sandhi-gateway   # import as: import sandhi_gateway
```

> The bare name `sandhi` on PyPI is an unrelated Sanskrit-linguistics library; this binding is
> published as `sandhi-gateway`. The crate and GitHub repo are `sandhi`.

## Usage

```python
import json
import sandhi_gateway as sg

gw = sg.Gateway(sink_path="usage.jsonl")           # events append as JSONL (+ in-memory)
gw.add_virtual_key("vk_alice", subject="alice", group="platform", upstream="anthropic")
gw.set_budget("group:platform", 1_000_000)

# ... you make your own provider call and get the raw response JSON ...
event = gw.meter(
    "vk_alice", "anthropic", "claude-x", response_json,
    session_id="conv_7",
)
# event["tokens_in"], event["cache_read_tokens"], event["subject_id"], ...
print(gw.spent("group:platform"))                  # budget recorded
print(gw.check_budget("group:platform", 5000))     # True/False

# Just parse usage (same Rust parsers as the proxy), no attribution:
sg.parse_usage("openai", response_json)            # {tokens_in, tokens_out, cache_*}
```

`meter()` parses the usage **at the source** (the same cache-split logic as the reverse
proxy), attributes it to the virtual key's subject/group, records the budget, emits the
neutral usage event (matching [`usage-event.v1.schema.json`](https://github.com/anvai-labs/sandhi/blob/main/schemas/usage-event.v1.schema.json)),
and returns it for local display. Unknown key → `KeyError`; bad JSON → `ValueError`.

Apache-2.0. See the [main README](https://github.com/anvai-labs/sandhi) and
[ADR-0001](https://github.com/anvai-labs/sandhi/blob/main/docs/adr/0001-sandhi-architecture-and-wire-contract.md).
