# Streaming route deadlines

Standalone operators can opt into `streaming_deadlines` in `SANDHI_CONFIG`.
The section is startup-only: config preview/apply reports active and desired
policies and `restart required`. Invalid configuration or an unregistered
credential endpoint fails startup. Without this section, setup remains 30 seconds,
idle remains 90 seconds, and the body remains pull-driven without a total lifetime
(unless a Rust embedder sets the existing body-only option).

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

Register `inferflux:local` before restarting with this example. Endpoint keys are
credential references, not provider aliases or URLs. Resolution is exact model →
endpoint default → global default, after identity, model and attribution checks.
Every override replaces the entire triple; partial inheritance, wildcard model
IDs, unknown fields, duplicate keys, zero and fractional milliseconds are rejected.
Caller headers cannot raise limits. Values are neutral time limits, unrelated to
pricing. OIDC roles, credential authorization and default zero POST retries remain
unchanged.

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
are reused. Unsupported custom transports fail before dispatch under this policy.

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
