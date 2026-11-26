# Streaming route deadlines

The current standalone candidate always selects a bounded streaming policy.
Without an explicit section, its source defaults are 30 seconds for setup,
180 seconds for idle gaps and 540 seconds for the body, within an 840-second
ceiling. Operators can override them through `streaming_deadlines` in
`SANDHI_CONFIG`. These are configured source defaults, not proof of a particular
process's effective override or a physical token guarantee.

The section hot-applies (ADR-0047): `POST /admin/config/apply` validates the
declared endpoint references against the registry as of that apply and activates
the policy live — no restart. Config preview reports the active and desired
policies with the `applies on apply` mode. An unregistered reference refuses the
apply (accumulated failure, active policy untouched); invalid configuration
still fails startup. Apply is additive-only, like every config section: deleting
the section from the file never changes the active policy. Reverting to the
built-in defaults is itself a declared policy — set the section to the default
values and apply, or restart. The pure provider FFI has no ledger owner;
Rust library body-only options are described separately in
[streaming body ownership](stream-body-lifetime.md).

## Token-scaled body estimate

A fixed body limit is too short for legitimately long completions (a 32K-token
reply at ~50 tok/s needs ~650 s). When a streaming request resolves to the
*global default* — no explicit endpoint default and no exact-model override —
the effective body limit is raised to
`max(resolved body, min(max_output_tokens × ms_per_output_token × headroom_factor, max_body_ms))`.
Defaults: `ms_per_output_token` 20, `headroom_factor` 2.0, `max_body_ms`
`ceiling_ms − default.setup_ms`. Explicit overrides are always authoritative:
the estimate never applies to them, and it never shrinks a limit. Setting
`max_body_ms` at or below the default body disables scaling (escape hatch).
`GET /v1/models` advertises the raw policy limits; the effective body can be
higher, up to `max_body_ms`.

```json
{
  "streaming_deadlines": {
    "ceiling_ms": 600000,
    "default": {"setup_ms": 30000, "idle_ms": 90000, "body_ms": 120000},
    "estimate": {"ms_per_output_token": 20, "headroom_factor": 2.0},
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

Register the transport reference before applying an override. A direct
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
    "default": {"setup_ms": 30000, "idle_ms": 180000, "body_ms": 540000},
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

This applies on the next `POST /admin/config/apply`; the apply response's
deadline status (active converging to desired) is the sanitized active-policy
receipt. The old policy remains in force until the apply is accepted.

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
settlement guarantees**. [ADR-0050](../adr/0050-structured-owner-origin-timeout-frame.md) adds an in-band,
dialect-specific error for an owner-enforced **body** deadline when a complete SSE
event boundary is available and the origin has not supplied a terminal event.
Measured non-final usage stays partial in both the error evidence and accounting,
with both holds unresolved under the existing recovery and retention policy.
Setup-error paths still use synchronous legacy settlement;
ledger contention can delay the HTTP error. The body owner retains its admission
and lifecycle guards while settlement runs. DONE is not a settlement receipt.
Neither local timeout nor a closed connection proves origin GPU cancellation.
No lease renewal, durable pending-work recovery or automatic upstream replay is
introduced. Live InferFlux/mixed-team acceptance remains a separate gate.
