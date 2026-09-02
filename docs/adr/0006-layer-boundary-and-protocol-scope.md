# ADR-0006: Layer boundary and protocol scope — Sandhi stays L7, and what it would take to change that

Date: 2026-08-31

## Status

**Accepted** (promoted 2026-09-01). TD-0015 P1 confirmed the durable ledger's
directional premise: in the corrected paired run, reserve-and-settle measured
195.6 µs with `synchronous=FULL` versus 43.2 µs with `NORMAL` (~4.5×), while
four threads sharing one ledger produced only ~1.10× the single-thread
throughput. Absolute FULL results ranged from 196–881 µs on macOS/APFS, so
these are architectural signals rather than production SLOs or an end-to-end
throughput ceiling. Numbers and caveats are in
[TD-0015](../td/TD-0015-performance-baseline-and-fault-injection.md) P1. They support prioritizing
ledger and end-to-end measurement before lower-layer work; this ADR is binding as both a process
gate (§D4) and a scope verdict.

Relates to [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) (what Sandhi measures),
[ADR-0002](0002-provider-transport-scope-and-modality-admission.md) (the admission discipline this
ADR generalizes from modalities to layers), and
[ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md) (the two-plane split, which is the
existing statement of *how much* of a payload Sandhi must see). Does not touch the measure-vs-price
boundary.

## Why this exists

The question "should Sandhi become a protocol-neutral policy and data plane, with AI gateways as
one use case?" is asked roughly once per planning cycle, gets re-derived from scratch each time, and
has never been settled against evidence. Adjacent forms of the same question — *should we own an L4
data plane? io_uring? kTLS? eBPF? our own HTTP parser?* — recur for the same reason.

This ADR settles it once, records the reasoning so it can be **attacked rather than repeated**, and
defines the quantitative bar a future proposal must clear to reopen it. It exists to make the answer
cheap to look up and expensive to hand-wave past.

## Context — where Sandhi actually sits today

A read-only audit of `bf8df6b` produced the following. Each is a repository fact with a citation;
the inferences are labelled.

> **Implementation reconciliation (2026-09-02).** The table below is the evidence baseline at
> `bf8df6b`, not current-state documentation. TD-0014 P1–P3 subsequently added bounded linear
> stream splitting, body-held admission permits, an explicit HTTP/1 accept loop, connection/header
> limits, peer context, and trusted-proxy handling. TD-0017 P1 added opt-in TLS. The current delta
> for every audit item is maintained in the gap register at the end of this ADR.

**The OSI map is lopsided by construction.**

| Layer | What Sandhi owns | Evidence |
|---|---|---|
| L3 | Nothing. | — |
| L4 | One `TcpListener`. `SANDHI_BIND` parses as `SocketAddr`, so no UDS. The peer address is available from axum's `IncomingStream` and **discarded** — `build_app` never calls `into_make_service_with_connect_info`. | `sandhi-proxy/src/lib.rs:262-263,296-299`; `main.rs:292-295` |
| L5 | A session *string* (`x-sandhi-session` → `RequestMetadataV1.session_id`), never a transport object. | `sandhi-proxy/src/lib.rs:1507-1510` |
| L6 | **No TLS in the listener, at all.** `rustls` appears solely as a `reqwest` *client* dependency (`cargo tree -i rustls -e no-dev` reaches it only via `reqwest → hyper-rustls/tokio-rustls`). No SNI, no ALPN, no cert loading, no rotation. | `Cargo.lock:1690` |
| L7 | Everything: key resolution, allowlist, attribution binding, rate limit, budget lease, dialect transcoding, stream normalization, usage extraction, error redaction, admin API. | `sandhi-proxy/src/lib.rs:1451-1739` |

Sandhi is not an L7 gateway *with* a thin lower stack. It is an L7 application that delegates all of
L3–L6 to `hyper`/`reqwest` and to whatever fronts it. **There is no lower layer to evolve down
from** — descending would mean building one.

**F1 — The throughput ceiling is above L7, not below it.** **On the durable arm** — `SANDHI_STORE`
set, which is every deployment that wants budgets to survive a restart — each request takes **two
`fsync`-durable, globally-serialized SQLite write transactions**, reserve and settle, through **one
mutex** over **one connection**. (Without `SANDHI_STORE` the ledger is `ProxyLedger::Memory` and
touches no SQLite at all; that arm is not what this finding is about.)

```
ProxyState.ledger : Mutex<ProxyLedger>       sandhi-proxy/src/lib.rs:86
SqliteLedger      : one Connection           sandhi-store/src/ledger.rs:48
synchronous=FULL  (fsync every commit)       sandhi-store/src/lib.rs:59-61
BEGIN IMMEDIATE per reserve                  sandhi-store/src/ledger.rs:165-167
reserve → spawn_blocking                     sandhi-proxy/src/lib.rs:1969
settle  → block_in_place                     sandhi-proxy/src/lib.rs:1997-2007, 2155-2159
```

*Inference:* at 0.1–1 ms per `fsync`, this alone caps a budgeted deployment in the low thousands of
requests per second, serialized, regardless of anything at L4. `io_uring`, `splice`, and `kTLS`
cannot move a number set by a `COMMIT`. **This inference is exactly what TD-0015 must confirm before
this ADR is Accepted.**

**F2 — The transparent plane is already near the floor for byte movement.** Chunks are forwarded as
refcounted `Bytes` with a bounded 64 KiB sniff buffer and an O(n) incremental scan
(`sandhi-providers/src/lib.rs:302,328-395`). There is no meaningful copy left for a lower layer to
eliminate on the path that matters.

**F3 — Every enforcement decision requires the JSON body.** Model allowlist, token estimate, budget
ceiling, attribution binding, dialect transcoding, and usage extraction all read the decoded
envelope (`sandhi-proxy/src/lib.rs:1504,1560-1586,2359-2380`). This is not incidental; it is the
product.

## Decision

### D1. Sandhi remains an L7 system. It does not acquire an L3/L4 data plane.

Three reasons, in order of weight:

1. **The bottleneck is above L7 (F1).** Optimizing byte movement in a system whose limit is a
   serialized durable commit is a category error.
2. **Nothing Sandhi sells can be enforced below L7 (F3).** Neutral units, the cache-creation /
   cache-read split, key-authoritative attribution, the ceiling lease, run cost trees, dialect
   translation — an L4 plane can enforce none of them. A split data plane yields a forwarder that
   cannot make a single decision, plus the gateway that already exists.
3. **The L4 market is commodity and entrenched.** Envoy, HAProxy, nginx, and the cloud L4 services
   do it better, and none of them meter. Competing there dilutes the one scarce thing.

### D2. "Protocol-neutral" means breadth at L5/L6/L7, not depth at L3/L4.

The real protocol gaps are **upward and sideways**: TLS termination
([TD-0017](../td/TD-0017-transport-security-and-ingress-protocol-breadth.md)), HTTP/2 ingress
(same), and duplex sessions for realtime voice and agent traffic
([TD-0018](../td/TD-0018-duplex-session-metering.md)). Each is a session- or presentation-layer
capability that unlocks a named use case. None requires owning a socket.

### D3. The abstraction to generalize is the **metered session**, not the transport.

A `Listener`/`DuplexStream` trait abstracts a socket, and sockets are not what Sandhi's invariants
are about. What must generalize is that `RequestAccounting` currently hard-assumes **one request →
one lease → one usage event** (`sandhi-proxy/src/lib.rs:2010-2220`). *That* assumption — not
`TcpListener` — is what blocks voice, gRPC streaming, and agent duplex traffic. TD-0018 owns the
`MeteredSession` design and must prove it by spike before any transport refactor is scheduled.

Corollary: one small piece of transport metadata is genuinely missing — a `ConnCtx` carrying peer
address, forwarded-for (only when the peer is a trusted proxy), ALPN, and SNI. It is roughly forty
lines, it closes the pre-authentication abuse gap
([TD-0014](../td/TD-0014-data-plane-resource-safety.md) G19), and **protocol metadata must not leak
past it** — that is the invariant the boundary exists to protect.

### D4. Lower-layer work is gated, not forbidden. Here is the bar.

A proposal to own any of L3/L4/L6 machinery, or to introduce OS-primitive hot-path facilities
(`io_uring`, `splice`/`sendfile`, `kTLS`, `SO_REUSEPORT`, eBPF/XDP), is admitted only when a
disposable spike demonstrates **all four**:

1. **A material win:** ≥30% p99 latency reduction **or** ≥30% CPU-per-request reduction **or** a
   protocol capability unreachable otherwise — measured against a published TD-0015 baseline on the
   same hardware, not against intuition.
2. **Zero enforcement regression:** the TD-0007 C1–C6 ledger conformance suite stays green.
3. **No unfuzzed unsafe surface:** any new `unsafe`, FFI, or hand-rolled parsing ships with a fuzz
   target in the same PR ([TD-0019](../td/TD-0019-ingress-codec-untrusted-input-hardening.md)).
4. **A named owner for CVE response** on anything that replaces a maintained library.

This is deliberately the same shape as ADR-0002's modality gate: *capability claims are proven, not
asserted.* "More advanced" is an outcome to demonstrate, never a design rationale.

### D5. The do-not-build list.

Do not build, and do not spike:

1. A TCP, TLS, QUIC, HTTP-parser, HPACK/QPACK, or cryptography implementation. Non-negotiable —
   `hyper`/`rustls` carry a CVE apparatus, fuzzing corpus, and interop matrix a metering project
   cannot staff.
2. A generic L4 load balancer or TLS-passthrough proxy. Commodity, and it cannot meter (F3).
3. eBPF/XDP — requires an L4 concept Sandhi does not have, is Linux-only, and optimizes a cold path.
4. `io_uring` / `splice` / `sendfile` — blocked behind TD-0015 showing `fsync` is *not* dominant.
5. `kTLS` — native TLS termination has since shipped in TD-0017 P1, but profiling has not shown TLS
   CPU above the 15% admission threshold; do not spike it without that evidence.
6. A database-protocol proxy (Postgres/MySQL wire) or a Kafka/NATS/MQTT gateway. No token concept,
   no cache split, no provider dialect: zero reuse of the actual differentiator.
7. A first-class `sandhi-transport` crate, before TD-0018 proves the *session* abstraction is the
   real need. Building the transport layer first is architecture-first, which is the failure mode
   this ADR exists to prevent.
8. A second process for an L4 data plane — see D1.

## Consequences

- **Positive.** The scope question stops consuming planning cycles. Effort concentrates on measured
  ledger, resource-safety, observability, and protocol-lifecycle gaps, none of which requires a
  general L4 data plane. The gate (D4) keeps the door open on evidence.
- **Negative.** Sandhi deliberately remains an application-layer gateway rather than a
  general-purpose service mesh. The shipped listener is HTTP/1-only; protocol breadth and duplex
  sessions remain explicit, separately tested work.
- **Evidence result.** TD-0015 P1 confirmed the durability/serialization cost inside the ledger,
  not system-wide dominance. The remaining end-to-end profile can still reopen the decision, but
  lower-layer work must clear D4's quantitative gate rather than appeal to a presumed bottleneck.
- **Nothing unresolved in this ADR.** The last open question — whether HTTP/2 ingress was already
  compiled in by feature unification — was **answered in the negative** during fact-check: the `h2`
  crate is not in the non-dev dependency graph, and the `http2` feature's only enabler is the
  `wiremock` dev-dependency, which `resolver = "2"` does not unify into normal builds. See
  [TD-0017](../td/TD-0017-transport-security-and-ingress-protocol-breadth.md) D1. Every other item
  raised across TD-0014…0021 is resolved on evidence, gated on a named experiment, or (TD-0018's two
  product questions) awaiting an owner this document cannot assign.

## Appendix — the gap register (G01–G32)

The architecture audit produced thirty items; the 2026-09-01 design audit (patterns +
data structures, adversarially verified) added G31–G32. This table is the **coordination and status
index**: each gap has exactly one owning document. Sources:
**R** = architecture audit, **C** = co-design seam (TD-0008 / bindings), **FP** = first principles.

| ID | Gap | State | Src | Sev | Owner |
|---|---|---|---|---|---|
| G01 | Typed stream decoders were unbounded and rescanned quadratically | **Complete** | R | P1 | TD-0014 |
| G02 | Admission permit was released at first byte | **Complete** | R | P1 | TD-0014 |
| G03 | Header timeout and connection bounds were absent | **Complete** | R | P1 | TD-0014 |
| G04 | No per-tenant bulkhead | Open | FP | P1 | TD-0014 |
| G05 | No TLS termination | **Complete** | R | P0 | TD-0017 |
| G06 | Possible undeclared HTTP/2 ingress | **Closed: surface absent** | R | P2 | TD-0017 |
| G07 | Durable writes serialize within one ledger shard | Partial: cross-scope sharding shipped | R | P0 | TD-0016 |
| G08 | `SANDHI_REPLICA_COUNT != 1` prevents HA/rolling replicas | Open | R | P0 | TD-0016 |
| G09 | No performance baseline | Partial: durable-ledger P1 published | R | P0 | TD-0015 |
| G10 | No unified fault-injection suite | Partial: slowloris/drain regressions shipped | R | P1 | TD-0015 |
| G11 | No WebSocket ingress | Open | R | P2 | TD-0018 |
| G12 | One-request:one-lease assumption blocks session metering | Open | FP | P2 | TD-0018 |
| G13 | No fuzz/property testing on ingress and stream decoders | Open | R | P1 | TD-0019 |
| G14 | Cross-dialect fidelity is example-pinned, not property-pinned | Open | FP | P2 | TD-0019 |
| G15 | No drain-aware `/readyz` | Open | FP | P1 | TD-0020 |
| G16 | Missing resource/admission observability | Partial: connection/stream/connection-shed signals shipped | R | P1 | TD-0020 |
| G17 | Provider handles create unshared client pools | Open | R | P2 | TD-0020 |
| G18 | No queue-delay load shedding | Open | FP | P2 | TD-0020 |
| G19 | Peer address was discarded | **Complete** | R | P1 | TD-0014 |
| G20 | `idempotency-key` was inert | **Complete** | R+C | P2 | TD-0021 |
| G21 | No HTTP contract-version discovery | **Complete** | C | P2 | TD-0021 |
| G22 | Node lacked typed provider-error parity | **Complete** | C | P3 | TD-0021 |
| G23 | Embedding modality was left in limbo | **Closed: not admitted** | C | P1 | ADR-0007 |
| G24 | `input_estimate` undercounts CJK | Open | R | P3 | TD-0016 |
| G25 | `Warn` metadata differs between memory and durable ledgers | Open | FP | P3 | TD-0016 |
| G26 | An unbounded, hard-capped call loses transparent forwarding | Partial: operator trade documented | FP | P2 | TD-0016 |
| G27 | No DNS cache/resolver tuning control | Open | R | P3 | TD-0020 |
| G28 | Rate limiter is one global `Mutex<HashMap>` | Open | R | P3 | TD-0014 |
| G29 | Lower-layer proposals lacked a decision gate | **Complete** | FP | Gov | ADR-0006 D4 |
| G30 | Client-facing error construction was fragmented | **Complete** | FP | P3 | TD-0021 |
| G31 | Settled reservation history grows without bound | Open | R | P1 | TD-0024 |
| G32 | Family/codec binding has a broad co-edit surface | Open | R | P2 | TD-0025 |

**Current sequencing.** TD-0015's remaining measurements inform TD-0014 P4, TD-0016 P2–P6, and
TD-0020. TD-0018 remains spike-gated and follows the relevant TD-0017 protocol/session context.
TD-0019, TD-0024, and TD-0025 have independent starting points.

## Pressure test

1. **"You are rationalizing not doing the hard thing."** The bar in D4 is quantitative and
   falsifiable, and D1's premise is labelled an inference with a named experiment that can overturn
   it. A rationalization would not schedule its own refutation.
2. **"Owning L4 would let you do per-tenant connection isolation and fairness."** True in principle,
   and unnecessary in practice: `ConnCtx` (D3) plus per-tenant bulkheads (TD-0014 G04) get the same
   isolation at L7 for a fraction of the surface area. If measurement later shows L7 fairness is
   structurally insufficient, that is a D4 case.
3. **"Envoy also started as a proxy and grew a policy plane."** Backwards. Envoy grew *upward* from
   L4 toward L7 because that is where the value was. Sandhi already occupies the destination;
   walking down would be retracing a path in the losing direction.
4. **"Protocol neutrality is a real customer ask — you are refusing it."** D2 accepts it and
   redefines where it lives. Customers asking for "protocol neutral" want WebSocket voice, gRPC
   embeddings, and TLS — all of which this ADR schedules. None want Sandhi to own a TCP stack.
5. **"The `fsync` claim is an inference; the whole ADR rests on it."** Correct, stated in the Status
   line, and the reason this is Proposed rather than Accepted. Note also that D1's reasons 2 and 3
   stand independently of F1 — even if byte movement mattered more than expected, an L4 plane still
   could not enforce a budget.
6. **"A do-not-build list will age badly."** Every entry in D5 is either a correctness/security
   argument that does not age (1, 2, 6) or is explicitly conditional on a measurement (3, 4, 5, 7).
   D4 is the mechanism for reopening any of them.
7. **"Ten tracking documents is overhead, not progress."** The alternative is one document that
   serializes ten independent workstreams. The register above exists precisely so the overhead buys
   parallelism rather than coordination cost.

## References

- [ADR-0002](0002-provider-transport-scope-and-modality-admission.md) — the admission-gate pattern
  this ADR generalizes from modalities to layers.
- [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md) D1 — the existing statement of how
  much payload visibility Sandhi requires.
- [TD-0015](../td/TD-0015-performance-baseline-and-fault-injection.md) — the measurement this ADR's
  status is gated on.
