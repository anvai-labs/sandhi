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

### Typed persistent provider runtime (0.1.2+)

New integrations should reuse a typed provider handle. Its inputs and outputs are Sandhi's
versioned neutral chat documents; provider-native JSON is encoded and decoded in Rust.

```python
runtime = sg.ProviderRuntime()
provider = runtime.provider("openrouter", "openai/gpt-4o", api_key)
request = {"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "hello"}]}
response = json.loads(await provider.complete_json(json.dumps(request)))

async for event_json in provider.stream_json(json.dumps(request)):
    event = json.loads(event_json)  # response_start, text_delta, tool_call_*, usage, finish
```

The JSON bridge is ABI-stable typed v1 data, not provider-native JSON. The handle retains its HTTP
pool, circuit breaker, retry policy, and timeouts. Invalid documents fail before network I/O;
runtime failures use the serialized `ProviderErrorV1` shape in the exception message.
`runtime.provider()` resolves a known endpoint from Sandhi's catalog;
`runtime.openai_compat()` is the explicit custom-endpoint escape hatch.

### Legacy provider-native transport (0.1.2+)

Sandhi also owns the provider wire layer: endpoint routing, headers, HTTP/SSE,
resilience, wire errors, and neutral usage extraction. Callers keep model policy,
prompt/tool assembly, and their framework-facing response types.

```python
import asyncio
import json
import sandhi_gateway as sg

async def main():
    api_key = "..."
    spec = sg.provider_spec("kimi", model="kimi-k3")
    body = {"model": "kimi-k3", "messages": [{"role": "user", "content": "hello"}]}
    result = await sg.complete(
        spec["slug"], "kimi-k3", spec["base_url"], api_key, json.dumps(body),
        max_retries=3,
    )
    # result = {"status": ..., "body": raw_json, "usage": neutral_cache_split}

    openrouter_model = "meta-llama/llama-3.3-70b-instruct"
    openrouter_body = {**body, "model": openrouter_model}
    async for item in sg.stream(
        "openrouter", openrouter_model, sg.provider_spec("openrouter")["base_url"], api_key,
        json.dumps(openrouter_body), max_retries=3,
        headers_json=json.dumps({"HTTP-Referer": "https://example.app", "X-Title": "My App"}),
    ):
        print(item["data"])
        # The terminal item carries finalized neutral usage.

asyncio.run(main())
```

`provider_spec()` exposes stable Rust-owned wire facts (canonical slug, aliases,
base URL, and model endpoint routing), not a model/capability catalog. Custom
`Authorization`, `Content-Type`, and `Host` values are ignored so callers cannot
override transport-owned headers.

The OpenAI-compatible transport accepts the Chat Completions roles `developer`,
`system`, `user`, `assistant`, `tool`, and legacy `function`. A `tool` result must
carry its `tool_call_id`; a legacy `function` result must carry `name`. Sandhi
validates these wire invariants before HTTP but deliberately does not rewrite roles:
whether a specific compatible model accepts `developer`, for example, is caller-owned
model policy.

### Custom / unknown providers (host escape hatch)

```python
# (a) register a host parser callback for a provider Sandhi doesn't know:
gw.register_parser("myprovider", lambda body: {"tokens_in": 30, "tokens_out": 12,
                                               "cache_creation_tokens": 0, "cache_read_tokens": 0})
gw.meter("vk_alice", "myprovider", "model", response_json)   # uses your callback

# (b) or skip parsing and pass counts directly:
gw.meter_tokens("vk_alice", "myprovider", "model", tokens_in=30, tokens_out=12)
```

`meter()` parses the usage **at the source** (the same cache-split logic as the reverse
proxy), attributes it to the virtual key's subject/group, records the budget, emits the
neutral usage event (matching [`usage-event.v1.schema.json`](https://github.com/anvai-labs/sandhi/blob/main/schemas/usage-event.v1.schema.json)),
and returns it for local display. Unknown key → `KeyError`; bad JSON → `ValueError`.

Apache-2.0. See the [main README](https://github.com/anvai-labs/sandhi) and
[ADR-0001](https://github.com/anvai-labs/sandhi/blob/main/docs/adr/0001-sandhi-architecture-and-wire-contract.md).
