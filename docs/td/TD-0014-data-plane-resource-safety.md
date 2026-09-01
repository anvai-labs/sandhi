# TD-0014: Data-plane resource safety — the bounds the proxy claims but does not hold

- **Status:** Draft (proposed), 2026-08-31. **P1, P2 (+P2b) and P3 landed**; P4 open. Owns gaps **G01, G02, G03, G04, G19, G28** (G01 ✅, G02 ✅, G03 ✅, G19 ✅).
- **Relates to:** [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md) (why these are fixed
  at L7 rather than by acquiring a transport layer), [TD-0006](TD-0006-two-plane-proxy-transparent-metering.md)
  (which fixed exactly G01 on the *raw* plane and never backported it), [TD-0012](TD-0012-rate-limit-enforcement.md)
  (the limiter this TD shards), [TD-0020](TD-0020-operational-readiness-and-transport-observability.md)
  (the gauges that make every fix here observable — build them in parallel).

## Why this exists

The proxy advertises four resource bounds. Three of them do not bound what the operator would
reasonably believe, and one class of bound is absent entirely. None of these is a design question;
each is a defect with a known fix and a writable failing test.

The pattern is the same one TD-0012 named: **a stated bound that does not bind is worse than an
absent one**, because the operator stops looking.

### G01 — the stream buffer fix was applied to one plane and not the other

TD-0006 explicitly fixed an unbounded line buffer and an O(n²) rescan in the metered pass-through,
and documented the fix at `sandhi-providers/src/lib.rs:299-315`:

> - **Bounded line buffer** — a single line exceeding `LINE_SNIFF_BUDGET` is flushed without
>   sniffing so memory stays bounded. […]
> - **O(n) scan** — tracks the last-scanned position […] The original rescanned the entire
>   accumulated buffer on every chunk (O(chunks²)).

Both defects remain, unmodified, in **all six typed streaming decoders**:

```
anthropic_typed.rs:320,330,331        cohere_typed.rs:259,267,268
gemini_typed.rs:304,313,314           ollama_typed.rs:297,304,305
openai_responses_typed.rs:512,521,522 typed.rs:950,958,959
```

Each is `let mut buffer = Vec::<u8>::new()` … `buffer.extend_from_slice(&chunk.data)` …
`while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n')`. A newline-free upstream
stream — hostile, buggy, or a single very large tool-call delta — grows a per-request `Vec` without
bound, and the rescan is quadratic in chunk count. The translation plane is the *cross-family* path,
so this is reachable by any client whose dialect differs from its upstream.

### G02 — `SANDHI_MAX_IN_FLIGHT_AI_REQUESTS` does not bound concurrent streams

`ConcurrencyLimitLayer` wraps the AI routes (`sandhi-proxy/src/lib.rs:207-209`). In `tower 0.5.3`,
the permit is held by `ResponseFuture` and dropped when **that future** resolves
(`tower-0.5.3/src/limit/concurrency/future.rs:14-22`). For a streaming call, `stream_response`
resolves as soon as upstream headers arrive and the body stream is constructed
(`sandhi-proxy/src/lib.rs:2349-2353`) — so the permit is released **at first byte** and the SSE body
runs outside the limit.

The doc comment at `lib.rs:72-75` is accurate about *buffering*. The bound simply does not apply to
the resource that dominates an SSE gateway: simultaneously open streams, each holding an upstream
connection, a lease, a task, and per-stream buffers.

### G03 — no slowloris defence

`axum::serve` is called with a default `hyper_util::server::conn::auto::Builder`
(`axum-0.7.9/src/serve.rs:254,423`): no `header_read_timeout`, no `max_buf_size`, no connection cap.
Idle and half-open connections are bounded only by the file-descriptor limit.

### G04 — no per-tenant bulkhead

Three global locks sit on the request path: the AI-route semaphore
(`lib.rs:206`), the enforcement ledger `Mutex` (`lib.rs:85`), and the rate limiter's single
`Mutex<HashMap>` (`ratelimit.rs:57-64`). A single tenant can saturate all three. Sandhi's product
claim is per-subject attribution and per-subject budgets; per-subject *isolation* is the missing
third leg.

### G19 — the peer is invisible

No `ConnectInfo`, `peer_addr`, or `x-forwarded-for` anywhere in `crates/`. Identity is entirely
bearer-token, resolved **after** the connection, the TLS-less handshake, and the body read. There is
no control that can act before a credential is presented.

### G28 — the limiter's own lock

`RateLimiter` is one `Mutex<HashMap<String, Bucket>>` taken on every request
(`ratelimit.rs:57-64`). It is the cheapest check in the pipeline and, at scale, will be the first
point of contention.

## First principles

1. **A bound that does not bind is a defect, not a gap** (TD-0012's framing). Every item here is
   either fixed or the claim is withdrawn from the operator surface.
2. **Bound the resource that dominates, not the one that is easy to count.** For an SSE gateway that
   is open streams, not buffered request bodies.
3. **Isolation is a product feature, not an implementation detail.** Sandhi already sells
   per-subject attribution and budgets; per-subject blast-radius containment is the same promise.
4. **The cheapest rejection first, and as early as possible.** A pre-authentication flood must be
   refused before it can allocate. That requires knowing the peer (G19).
5. **Fix it where TD-0006 already fixed it.** G01 has a correct, tested, in-repo implementation. The
   answer is to extract and reuse it, not to re-derive it six times.

## Non-goals

- **Not an edge DoS defence.** As TD-0012 states, Sandhi is an egress gate; an internet-facing flood
  is the fronting proxy's job. G19's per-IP cap is a blast-radius control for a *trusted* network,
  not a WAF, and must not be described as one.
- **No transport layer.** Per ADR-0006 D3, `ConnCtx` is a narrow metadata struct, not the seed of a
  `sandhi-transport` crate. Protocol metadata must not leak past it.
- **No cross-replica coordination.** Every limit here stays per-process, sharing TD-0007's eventual
  shared-backend story rather than inventing a second one.

## Decisions

**D1 — Extract the bounded line-splitter to one place and use it on both planes.** `metered_passthrough`'s
buffer discipline (O(n) via a `searched_to` cursor, bounded by a caller-supplied budget) becomes a
reusable `LineSplitter` in `sandhi-providers`. All six typed decoders consume it. Rejected: patching
six copies — that is how the drift happened.

**D1a — One ceiling, two policies.** This decision has been corrected twice, and both corrections
are recorded because the chain is the lesson:

1. The first draft shared the raw plane's 64 KiB sniff budget as the typed bound, "so raising it is
   one decision." Implementation review falsified that: OpenAI's Responses API puts the entire final
   response object — all generated output included — in the single `response.completed` SSE line,
   and that is also the line carrying the usage. A long generation is ~130 KiB, so 64 KiB killed
   working streams and destroyed their metering.
2. The first correction split into two ceilings: a small sniff budget for the raw plane, an 8 MiB
   ceiling for the typed plane. **A second adversarial review falsified that too** — by the same
   argument, since the raw plane's drop-and-continue policy truncates that same 130 KiB usage frame
   mid-line, after which nothing parses and the terminal item carries *no* usage at all. Reproduced:
   the lease then settles on a byte estimate, silently undercharging every long generation on the
   DEFAULT transparent plane. The small budget was wrong on both planes, for the same reason.
3. The landing point: **one ceiling** — `MAX_STREAM_LINE_BYTES` (8 MiB), sized far above any frame
   we can name — with the **policy chosen at the call site**: the raw plane drops the pending line
   and keeps streaming (bytes were already forwarded; only usage accuracy is at risk), the typed
   decoders raise `ProviderError::Transport` (they emit decoded content; dropping silently would
   corrupt the response with no signal).

Two numbers never fixed anything here; one number and two policies did.

**D1b — Test the legitimate case, not only the pathological one.** The bug above survived a full
green suite because every test used either tiny frames or multi-megabyte filler with no newline at
all. Two properties are needed, and the second is the one that was missing: a stream with *no line
boundary* must be refused, and a stream with *one very large but terminated line* must pass intact.
The second only reproduces under **chunked** delivery — a single-chunk push drains the complete line
before the budget is ever consulted — so any test of a size bound must feed bytes the way a socket
does.

**D2 — The concurrency permit is held by the response body, not the response future.** A small `Body`
newtype owns the `OwnedSemaphorePermit` and releases it when the stream terminates (including on
drop/cancellation). This makes `SANDHI_MAX_IN_FLIGHT_AI_REQUESTS` mean what its name says. Rejected:
a second semaphore for streams — two knobs for one resource, and the arithmetic for operators gets
worse, not better.

**D3 — Explicit connection-level limits at the listener.** `header_read_timeout`, a maximum
concurrent connection count, and a maximum per-IP connection count, each with an env knob and a
documented default. Implemented by replacing bare `axum::serve` with an explicit accept loop over
the same `auto::Builder`, so the change is additive and reversible.

**D4 — `ConnCtx`, and nothing more.** Peer address, `forwarded_for` (populated **only** when the peer
is in a configured trusted-proxy allowlist — otherwise the header is attacker-controlled and worse
than nothing), and later ALPN/SNI from TD-0017. It is request-scoped metadata available to admission
checks; it does not enter `RequestMetadataV1`, the usage event, or any metric label (TD-0011 D2 —
an IP is unbounded cardinality *and* personal data).

**D5 — Per-tenant bulkheads over the three global locks.** A per-scope semaphore with a per-scope
cap alongside the global one (a tenant may not consume more than its share of in-flight capacity),
and sharded maps for the limiter (G28). The ledger mutex is **out of scope here** — it is TD-0016's
subject, and splitting it needs the ledger's own correctness argument.

**D6 — Every bound added here ships with its gauge.** A limit that cannot be observed cannot be
tuned, and today none of G02/G03/G04/G19 is visible at all. The gauges live in TD-0020; this TD does
not land a phase whose effect an operator cannot see.

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| **P1** ✅ | D1 — shared `LineSplitter` on all six typed decoders **and** the raw sniffer | A 16 MiB newline-free upstream stream is refused loudly on the typed plane (peak buffer ≤ `MAX_STREAM_LINE_BYTES` + one chunk) while a 200 KiB *terminated* frame passes intact on every decoder — both directions asserted, and the guards verified to fail against the old 64 KiB bound; total work (search **and** compaction) stays linear in bytes; a long `response.completed` frame still yields its usage on the raw plane; all existing per-family stream tests stay green byte-for-byte |
| **P2** ✅ | D2 — permit held by the body, via a custom `AdmissionLayer` (tower's permit cannot be moved into a body) | An end-to-end test over real loopback HTTP with a hanging upstream: with the limit at 2, two streams open at first byte make a third request **wait**; aborting one client admits it. Shipped with the `sandhi_streams_open` gauge (D6) and the default raised 64 → 128 with the reasoning stated. Both planes pinned end-to-end — the review demonstrated the transparent-plane test alone stayed green under a first-byte-release mutation of the translation twin, so the translation plane got its own pin |
| **P2b** ✅ | trailing-remainder flush (the asymmetry recorded in Still open, now closed) | Ollama's NDJSON `done` frame without a trailing newline yields `Finish`; every P1 byte-boundary and usage-ordering test stays green, pinning the restructure as behaviour-preserving for well-formed streams |
| **P3** ✅ | D3 + D4 — connection limits and `ConnCtx`, via an explicit hyper-util accept loop (replacing `axum::serve`) | Slowloris connections are closed by a **guaranteed** header-read timeout with the required Tokio timer installed (hyper's default was documented "do not depend on that"); the total connection cap sheds without a response and recovers on close; the **per-IP cap is opt-in (default off)** because it counts the peer at accept time; `X-Forwarded-For` is believed only from CIDR-allowlisted peers, resolved **per request** by middleware (an earlier draft built it at accept time where headers do not exist — dead wiring caught by review); the drain at grace expiry **aborts** hung connections — closing the detached-task leak the #169 review surfaced, with an upstream-EOF regression test. Served with hyper's **http1 builder directly**: hyper-util's auto builder sniffs the h2c preface before arming any timer, exempting zero-byte connections from every timeout — with the sniffing removed, silent connections are closed by the header timeout too (finding-3 regression test) |
| **P4** | D5 — per-tenant bulkheads + sharded limiter | Tenant A saturating its share leaves tenant B's admission latency within a recorded bound (fairness measured, not asserted qualitatively); the sharded limiter reproduces every TD-0012 P1 semantic test unchanged |

P1 and P2 are independent and can land in either order. P3 depends on nothing here but touches the
same `build_app`/serve path as P2 — sequence them to avoid a conflict. P4 wants TD-0020's gauges to
be meaningful.

## Pressure test

1. **"G01 is theoretical — no real provider sends a newline-free stream."** Gemini's non-`alt=sse`
   transport is a single JSON array with no line boundaries, which is why `metered_passthrough`'s
   final-flush handling exists at all (`providers/src/lib.rs:383-390`). The raw plane handles it; the
   typed plane, which is the *cross-family* path, does not. This is reachable today.
2. **"D2 will reduce throughput — streams are long, so the semaphore will be held for seconds."**
   Correct, and that is the point: the resource genuinely is held for seconds. The current behaviour
   does not make the cost disappear, it makes it invisible. Operators will need to raise the default;
   the default should therefore be raised in the same PR, with the reasoning stated.
3. **"Per-IP limits break every deployment behind a load balancer."** Which is why D4 makes
   `forwarded_for` conditional on a trusted-proxy allowlist and the per-IP cap opt-in with a
   permissive default. A per-IP cap keyed on the LB's address would be a self-inflicted outage.
4. **"Bulkheads add a lock to remove contention."** Per-scope semaphores are sharded by construction —
   the contention that matters is *cross-tenant*, and that is what disappears. The measurement in P4
   is the check on this claim; if it does not hold, P4 does not land.
5. **"This is six unrelated fixes in one TD."** They share one failure mode (a claimed bound that
   does not bind), one test harness, and one file footprint. Splitting them would serialise four
   independent phases behind four review cycles.
6. **"Why not just put a real proxy in front and skip all of it?"** A fronting proxy solves G03 and
   G19 and none of G01, G02, G04, G28 — those are internal to Sandhi's own accounting and memory
   behaviour. It also cannot be assumed: the single-node dev posture is a supported deployment.

## Resolved

**R1 — Per-IP caps key on `peer`; `forwarded_for` is honoured only from an allowlisted peer.**
Never trust a header you cannot attribute: `x-forwarded-for` is caller-controlled, so keying a cap
on it unconditionally lets any client evict every other client's budget by rotating a header. There
is no trusted-proxy configuration anywhere in the repo today, so D4 introduces the first one, and
the cap's meaning is therefore deployment-dependent — that must be stated in
`docs/operator/proxy-guide.adoc`, not left for an operator to infer.

**R2 — D2's permit-holding body is safe across graceful drain, but P2 must assert it.** Drain drops
the server future at grace expiry (`sandhi-proxy/src/lib.rs:301-347`), which drops response bodies,
which drops the `OwnedSemaphorePermit` they hold. Safe by construction. It is nonetheless the P2
acceptance criterion that must prove it: a leaked permit would leave a process that never finishes
draining, and "safe by construction" is exactly the class of claim TD-0014 P1 falsified once
already.

## Still open

- **Is `MAX_TYPED_LINE_BYTES` = 8 MiB the right number?** Gated on
  [TD-0015](TD-0015-performance-baseline-and-fault-injection.md), which should record the largest
  frame each provider family actually emits. 8 MiB was chosen to sit far above the largest frame we
  can *name* (OpenAI Responses' `response.completed`, Gemini `inlineData`), not measured. The
  direction of error is deliberate — too high merely delays an OOM that P2's stream bound also
  guards, while too low kills working streams, which is the defect D1a exists to fix.
- **Request bodies have no read timeout (pre-existing, surfaced by P3).** The
  header deadline stops at head completion; a client that dribbles a 2 MiB
  body at 1 B/s holds its connection slot, admission slot, and lease
  indefinitely while staying under every timer shipped here. Bounded in
  aggregate by the connection and admission caps; a per-body deadline is
  TD-0020/TD-0017 scope if measurement shows it matters.
- **`ClientAddr` has no production consumer yet.** The middleware resolves
  and inserts it on every AI request (pinned by the wiring test), but no
  handler or metric reads it — its consumers are per-IP rate limits (P4's
  natural shape) and TD-0020's per-client visibility. The `#[allow(dead_code)]`
  comes off the day a consumer lands.
- **Frames near the ceiling are real, not hypothetical.** Review named a frame within ~1.5× of the
  ceiling: OpenAI Responses' `response.image_generation_call.partial_image` carries a base64
  gpt-image-1 PNG at roughly 5.5–6.7 MB, and Gemini native-audio `inlineData` runs ~64 KB/s, so two
  minutes of audio in one part is ~8 MB. This sharpens TD-0015 R5's deliverable: record the largest
  real frame per family *including multimodal* frames before trusting 8 MiB.
- ~~Grace expiry does not cancel in-flight streams~~ **Closed in P3.** The explicit hyper-util
  accept loop owns every connection task in a `JoinSet` and uses `GracefulShutdown` signalling: at
  the grace deadline the stragglers are **aborted**, and an upstream-EOF regression test proves a
  hung stream no longer outlives `serve_with_shutdown_timeout` (the #169 review's finding 2).
- **`sandhi_streams_open` covers streams only (review finding 3).** Unary calls hold admission
  slots invisibly, so the semaphore can be exhausted at gauge 0. The gauge is honestly named; the
  complete answer — semaphore utilization plus wait-queue depth — is TD-0020 D2 scope.
- ~~Typed decoders discard a trailing newline-less remainder~~ **Closed in P2b.** `LineSplitter::flush_newline`
  terminates the remainder and each decoder pumps one synthetic empty chunk through its existing
  loop body, so per-chunk event ordering is preserved without duplicating the body. (An intermediate
  transform gated the drain on non-empty chunk data and silently skipped the synthetic chunk — the
  motivating test caught it, which is why the test exists.)
