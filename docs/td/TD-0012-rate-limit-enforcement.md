# TD-0012: Rate-limit enforcement — closing a promise the API already makes

- **Status:** Accepted (2026-07-27). **P1 complete**; P2 (operator-surface honesty) and P3 (shared backend) open.
- **Relates to:** TD-0003 (operator surface: the field exists there), ADR-0005 (enforcement
  ordering and the lease ledger), TD-0007 (shared-backend contract), TD-0010 D2 (dialect-shaped
  errors), TD-0011 (bounded-label metrics)

## Why this exists

`rate_limit_per_min` is accepted by `sandhi vkeys share`, persisted in the `vkeys` table, carried on
every `VirtualKeyRecord`, and returned by the admin API. It is read **nowhere** in the request path:

```
$ grep -rn "rate_limit" crates/sandhi-proxy/src/lib.rs
$          # no output
```

So an operator can set a limit, see it echoed back, and be told nothing when it does not apply. That
is worse than an unimplemented feature — it is a **false affirmative**, and the operator only
discovers it when a runaway caller does not get throttled. Every other TD in this series has been
about making a claim true; this one is about a claim the product already makes.

## First principles

1. **A stored control that is not enforced is a defect, not a gap.** The honest options are to
   enforce it or to reject it at the API. Enforcing is the better product; either beats silence.
2. **Rate limiting and budgeting answer different questions.** A budget asks *how much has this
   subject spent* (tokens, durable, settled after the fact). A rate limit asks *how often is this
   key calling* (requests, in-memory, decided before dispatch). Conflating them would put
   high-frequency writes on the durable ledger's path.
3. **Cheapest rejection first.** A throttled request must never consume a lease, so the check runs
   after key resolution and before the budget reservation.
4. **Bounded state or it is a leak.** Same discipline as TD-0009 D2 and TD-0011 D2: per-key state
   needs eviction, decided up front rather than after the first OOM.

## Non-goals

- **Not a DoS defence.** The check runs *after* virtual-key resolution, so an unauthenticated flood
  never reaches it. Sandhi is an egress gate, not an edge proxy; that job belongs to whatever
  terminates the internet-facing connection, and pretending otherwise would be the same kind of
  false affirmative this TD exists to remove.
- **No token-per-minute limiting.** Tokens are the budget's unit and are only known *after* the
  call. Requests-per-minute is knowable before dispatch, which is why it can gate.
- No per-model or per-provider limits in this TD — the field is on the virtual key.

## Decisions

**D1 — Token bucket, keyed by virtual key.** Refill `limit/60` per second, capacity `limit` (one
minute of burst). Rejected: a fixed per-minute counter, which admits `2 × limit` across a window
boundary — the classic defect, and unacceptable for a control whose entire purpose is bounding a
runaway. A sliding-window log is exact but stores per-request state for no benefit at this
granularity. The bucket is O(1) state and O(1) per check.

**D2 — In-memory, behind the same trait shape the ledger uses.** Rate limiting is a per-request
decision at request frequency; putting it on the durable SQLite path would add a write per call to
the hot path for a control that is disposable by nature. **The consequence must be stated, not
buried: with N replicas the effective limit is N × `limit`.** That is the same single-node
limitation the enforcement ledger has (TD-0007), so it shares that TD's eventual shared backend
rather than inventing a second story.

**D3 — Bounded state with idle eviction.** One bucket per virtual key, evicted after an idle period
(default 10 minutes, ≫ one refill window so eviction can never grant extra budget). A revoked key's
bucket is dropped with the key. Cardinality is bounded by *live* keys, not by traffic.

**D4 — Reject in the caller's dialect, with `Retry-After`.** HTTP 429 rendered through the TD-0010
error renderer, and a `Retry-After` header carrying whole seconds until the next token. Both the
OpenAI and Anthropic SDKs honour `Retry-After` for backoff; a 429 without it makes a well-behaved
client retry immediately, converting a throttle into a hot loop.

**D5 — Ordering: auth → allowlist → **rate limit** → budget reserve → dispatch.** The throttled
request consumes no lease, records no spend, and emits no usage event — it never reached a provider.
It *is* counted in metrics (D6), because "requests that never happened" is exactly what an operator
needs when a caller reports failures.

**D6 — One bounded counter.** `sandhi_rate_limited_total{provider,model,dialect,plane,outcome}`
reuses the TD-0011 label set. Not labelled by virtual key, for the reason TD-0011 D2 gives: that is
unbounded. Per-key attribution belongs in the aggregate.

## Phases

| Phase | Scope | Acceptance |
|---|---|---|
| **P1** ✅ | Token bucket + enforcement at D5's position + dialect-shaped 429 with `Retry-After` + eviction + the D6 counter | A key with `rate_limit_per_min=N` admits N in a burst and refuses the N+1th; the refusal carries `Retry-After` and the caller's error shape; the refused call consumes no lease and emits no usage event; buckets for idle keys are evicted |
| **P2** | Honesty in the operator surface: `sandhi vkeys share --rate-limit` documents per-replica semantics; admin API and README say the same | The CLI help and README state the N-replica multiplication rather than implying a global limit |
| **P3** | Shared backend behind TD-0007's contract | A limit holds across two proxy instances against the shared store |

P1 makes the promise true on a single node, which is the deployment shape the ledger already
assumes. P2 is small and prevents the *next* false affirmative. P3 waits for TD-0007.

## Pressure test

1. **"Per-process limiting is a lie the moment you scale out."** Correct, and D2 says so in the
   product surface rather than only here (P2). The alternative — blocking the feature until a shared
   backend exists — leaves the current false affirmative in place indefinitely, which is strictly
   worse than a documented single-node limit. The ledger made the same trade for the same reason.
2. **"A token bucket permits a burst of `limit`."** Deliberate: a per-minute limit that refuses the
   second request of a burst would break every batching client. Capacity is the knob; if a
   deployment needs strict pacing, that is a different control and should be named differently.
3. **"Rate limiting before the budget check wastes the cheaper signal."** Inverted on purpose: the
   rate check is the cheap one (an in-memory bucket) and the reservation is the expensive one (a
   durable write). Ordering also matters for correctness — a refused request must not hold a lease.
4. **"Eviction could hand a caller a fresh bucket and extra headroom."** Only if the idle timeout
   were shorter than the refill window. D3 sets it an order of magnitude longer, and the test asserts
   an evicted-then-returning key cannot exceed its limit.
5. **"Why not reuse the ledger for this?"** Because the ledger is durable and settled-after-the-fact
   by design; rate limiting is decided before dispatch and is worthless after it. Sharing the trait
   for the *shared-state* problem is right; sharing the storage path is not.
6. **"A 429 will look like an upstream failure to a client."** Which is exactly why D4 renders it in
   the caller's dialect with `Retry-After` — the SDK then classifies it as throttling and backs off,
   instead of surfacing an opaque error or hot-looping.

## Open questions

- Should a rate limit honour the budget system's block/**warn** policy, so an operator can observe
  what *would* be throttled before enforcing? Attractive for rollout, but it doubles the state
  (a warn bucket still has to count) — worth it only if operators actually ask.
- Is `Retry-After` in whole seconds enough, or should a sub-second bucket report `0` and risk a
  tight retry? Leaning: floor at 1 second, since no SDK benefits from sub-second precision here.
- Does a revoked key's bucket need explicit teardown, or is idle eviction sufficient? Explicit is
  tidier; eviction is one less code path to get wrong.
