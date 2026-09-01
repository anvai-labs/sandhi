# ADR-0009: The listener speaks HTTP/1 only — no h2c sniffing, no cleartext h2

Date: 2026-09-01

## Status

Accepted (implemented in TD-0014 P3, #181 — the serve loop binds
`hyper::server::conn::http1::Builder` directly). Revises the implicit posture
inherited from `axum::serve`; shapes TD-0017's HTTP/2 path.

## Context

The P3 connection-defence work exposed a timeout bypass with an adversarial
review behind it: hyper-util's `auto::Builder` — what `axum::serve` uses —
buffers up to 24 bytes of every new connection sniffing for an **h2c
prior-knowledge preface** before choosing a protocol, and hyper's header-read
timer arms only inside HTTP/1 head parsing. A connection that sends **zero
bytes** therefore never starts any timer. Demonstrated against the shipped
binary: three silent TCP connections held the entire connection budget
indefinitely at `SANDHI_HEADER_READ_TIMEOUT_SECS=2` — the cheapest possible
denial of service (zero payload, ~16 source IPs to exhaust the total cap),
on a gateway whose product is *bounded, metered* transport.

Two mitigations existed: hyper-util's `http1` builder (no sniffing — the
timer arms at head start) with the cleartext-h2 surface simply removed; or a
custom pre-first-byte timeout wrapper around the sniffing builder. The
decision: the former.

## Decision

### D1. The listener is HTTP/1-only. Cleartext h2c prior-knowledge ingress is refused.

The serve loop binds `hyper::server::conn::http1::Builder` directly. There is
no protocol sniffing: the timer arms when the head read starts, so silent
connections are closed by the same `SANDHI_HEADER_READ_TIMEOUT_SECS` deadline
as partial heads (regression-tested both ways). A complete h2c preface is
rejected immediately — hyper http1 refuses the request line — so there is no
h2 slot-holding surface at all.

Cleartext h2 was never a supported ingress: no client, no documentation, no
test exercised it; it existed only as an accident of the auto builder's
sniffing, reachable but untested — precisely the "undeclared protocol
surface" ADR-0006's review warned about.

### D2. HTTP/2 returns only through TD-0017's path: TLS (ALPN-negotiated) with timeouts configured from day one.

h2 over TLS via ALPN is the supported shape everywhere h2 matters, and ALPN
negotiation happens *inside* the TLS accept — before any application-layer
sniffing — so the bypass class does not return. When TD-0017 lands h2, its
builder sets `header_read_timeout`, `max_buf_size`, and a timer in the same
commit, with the silent-connection regression test extended to the h2 path.

### D3. Websocket-over-h1 upgrades are deferred with the same discipline.

hyper 1.x upgrades ride `with_upgrades()`, whose upgradeable connection
wrapper is not covered by hyper-util's `GracefulConnection` (the drain
aborts). No route upgrades today; TD-0018's duplex work will reintroduce
upgrades together with a drain story (tracked in TD-0014's Still-open list).

## Consequences

- **Positive.** The slowloris defence is unconditional: every connection,
  silent or dribbling, is bounded by the header deadline. One protocol on the
  listener means one timeout/buffer/drain story. The auto builder's sniffing
  class (and any future pre-protocol buffering) is structurally excluded.
- **Negative.** Clients that used h2c prior-knowledge against a plaintext
  Sandhi now fail immediately. None were supported or documented — but any
  that existed silently will notice. h2-over-TLS clients are unaffected once
  TD-0017 ships TLS.
- **Risk accepted.** If a deployment needed h2c (e.g., a gRPC-sidecar mesh on
  a trusted network), TD-0017 is the place to bring it back — with the
  pre-first-byte timeout as a named requirement, not an emergent property.

## Pressure test

1. **"Refusing h2c is a capability regression."** It refuses a capability that
   did not exist, was not documented, and carried an unconditional DoS. The
   regression list is empty; the DoS list had a live demonstration.
2. **"Wrap the sniffing builder with a first-byte timeout instead."** Viable,
   and the recorded fallback — but it preserves two protocol paths (one
   timerless) to serve a cleartext h2 nobody uses. Revisit only if a real h2c
   requirement appears, with the wrapper as a named requirement in its ADR.
3. **"TD-0018 needs upgrades now."** No shipped route upgrades; the drain gap
   for upgradeable connections is documented in TD-0014's Still-open list and
   gated on TD-0018's own design.

## References

- [TD-0014](../td/TD-0014-data-plane-resource-safety.md) P3 — the
  implementation, regression tests, and review arc that produced this
  decision.
- [TD-0017](../td/TD-0017-transport-security-and-ingress-protocol-breadth.md)
  D1 — the h2 experiment result (no undeclared h2 surface; HTTP/2 ingress is
  a deliberate feature), which this ADR resolves definitively.
- hyper-util `server::conn::auto` — the sniffing behaviour (24-byte preface
  buffer before protocol selection) that motivates D1.
