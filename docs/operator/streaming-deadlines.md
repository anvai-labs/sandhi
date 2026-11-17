# Streaming route deadlines

The current standalone candidate always selects a bounded streaming policy.
Without an explicit section, its source defaults are 30 seconds for setup,
90 seconds for idle gaps and 180 seconds for the body, within a 240-second
ceiling. Operators can override them through `streaming_deadlines` in
`SANDHI_CONFIG`. These are configured source defaults, not proof of a particular
process's effective override or a physical token guarantee.

The section is startup-only: config preview/apply reports active and desired
policies and `restart required`. Invalid configuration or an unregistered
transport reference fails startup. The pure provider FFI has no ledger owner;
Rust library body-only options are described separately in
[streaming body ownership](stream-body-lifetime.md).

```json
{
  "streaming_deadlines": {
    "ceiling_ms": 600000,
    "default": {"setup_ms": 30000, "idle_ms": 90000, "body_ms": 120000},
    "endpoints": {
      "inferflux:local": {
        "default": {"setup_ms": 60000, "idle_ms": 90000, "body_ms": 180000},
        "models": {
          "qwen3-coder-30b": {"setup_ms": 90000, "idle_ms": 120000, "body_ms": 300000}
        }
      }
    }
  }
}
```

Register the transport reference before restarting with an override. A direct
credential uses its `provider:label`; an explicit backend route uses
`backend/<backend-id>`. References are opaque registered keys, not provider
aliases or URLs. Resolution is exact actual model → endpoint default → global
default, after identity, logical-route selection and attribution checks.
Every override replaces the entire triple; partial inheritance, wildcard model
IDs, unknown fields, duplicate keys, zero and fractional milliseconds are rejected.
Values are neutral time limits, unrelated to
pricing. OIDC roles, credential authorization and default zero POST retries remain
unchanged.

Long-form planning should use an exact actual-model override instead of widening a
whole route. For the DS3 `route/coding` Qwen target whose explicit backend ID is
`qwen-coding-plan`, the reviewed operator intent is:

```json
{
  "streaming_deadlines": {
    "ceiling_ms": 840000,
    "default": {"setup_ms": 30000, "idle_ms": 180000, "body_ms": 420000},
    "endpoints": {
      "backend/qwen-coding-plan": {
        "models": {
          "qwen3.7-plus": {
            "setup_ms": 30000,
            "idle_ms": 180000,
            "body_ms": 600000
          }
        }
      }
    }
  }
}
```

This is startup-only and requires a deliberate service restart plus a sanitized
active-policy receipt. The old default remains in force until both are recorded.

Authenticated OpenAI-shaped `GET /v1/models` responses include a
`sandhi_streaming` object with the sanitized ceiling and per-model effective
limits. It omits URLs, credential references and backend names. If compatible
route targets have different limits, the model reports `targets:
"multiple_targets"` instead of claiming one value before affinity chooses a
target.

Streaming callers may send `x-sandhi-stream-body-budget-ms` once with the body
duration they need. The header is an advisory preflight signal: it cannot raise a
Sandhi limit or select a policy. A missing or within-policy value proceeds normally;
malformed, duplicated, zero, out-of-range, and above-policy values are rejected. An
above-policy value is refused before rate admission, provider dispatch, source egress
and reservation.

`ceiling_ms` may not exceed 840000: the 900-second reservation TTL minus 60 seconds
of settlement headroom. Both `setup_ms + body_ms` and `idle_ms` must fit the ceiling.
The actual remaining lease must cover setup plus body plus headroom before dispatch;
a second check after successful setup verifies body plus headroom. Expiry is checked
at persisted-second precision. Insufficient lifetime is refused, never silently
clamped or renewed. A nominal maximum may fail the actual check after admission
consumes time.

One absolute setup deadline is captured before the lease check and reused by nested
built-in transports and any explicitly enabled setup retry/backoff. It includes
rejected response-body collection. Idle is captured into the returned stream;
background body tasks do not depend on task-local inheritance. Clients and pools
are reused. Built-in Codex subscription transports support these limits through
the constrained typed codec; raw forwarding remains disabled and credential
expiry is still checked before dispatch. Unsupported custom transports fail
before dispatch under this policy, even when declaring a built-in provider family.

The [body owner](stream-body-lifetime.md) enforces body duration after successful
upstream headers, independently of downstream polling, on both proxy planes.
Route policy takes precedence over the Rust body-only setting. Its queue, failure,
usage conservation and shutdown semantics are unchanged.

These are transport deadlines, **not bounded HTTP error delivery or durable
settlement guarantees**. Setup-error paths still use synchronous legacy settlement;
ledger contention can delay the HTTP error. The body owner retains its admission
and lifecycle guards while settlement runs. DONE is not a settlement receipt.
Neither local timeout nor a closed connection proves origin GPU cancellation.
No lease renewal, durable pending-work recovery or automatic upstream replay is
introduced. Live InferFlux/mixed-team acceptance remains a separate gate.
