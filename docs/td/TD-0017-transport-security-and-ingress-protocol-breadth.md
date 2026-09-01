# TD-0017: Transport security and ingress protocol breadth — TLS, and finding out what HTTP versions we actually speak

- **Status:** Draft (proposed), 2026-08-31. Owns gaps **G05, G06**.
- **Relates to:** [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md) D2 (protocol breadth
  belongs at L5/L6/L7, and this TD is what that decision authorises),
  [TD-0014](TD-0014-data-plane-resource-safety.md) D4 (`ConnCtx`, which this TD populates with ALPN
  and SNI), [TD-0018](TD-0018-duplex-session-metering.md) (which depends on this),
  [TD-0010](TD-0010-ingress-dialect-parity.md) (dialect parity, which HTTP-version parity now joins).

## Why this exists

**Sandhi terminates plaintext HTTP and nothing else.** There is no TLS anywhere in the listener path:
no `rustls` server configuration, no `TlsAcceptor`, no certificate loading, no SNI, no ALPN, no
rotation. `rustls 0.23.42` appears in `Cargo.lock` solely as a `reqwest` *client* dependency
(`sandhi-providers/src/lib.rs:66-71`). The server is `tokio::net::TcpListener` handed to
`axum::serve` (`sandhi-proxy/src/lib.rs:262-265,296-299`).

For a component whose entire purpose is **holding the real upstream credential so clients never see
it**, and which receives a bearer virtual key on every request, this is the highest-severity gap in
the audit. Every deployment therefore has a mandatory fronting proxy — and that dependency is
currently *incidental* rather than documented, which means some deployment somewhere does not have
one.

The second half was, until this TD was fact-checked, an open question — and the answer turned out
to be the opposite of the draft's inference. **Sandhi's ingress speaks HTTP/1.1 only, and the `h2`
crate is not linked into the binary at all.**

The draft claimed `hyper-util`'s `http2` feature was "globally enabled by Cargo feature unification,
pulled in by `reqwest`," and inferred that h2 prior-knowledge (h2c) ingress probably worked by
accident. That was wrong on both counts:

```
$ cargo tree -p sandhi-proxy -e features,no-dev
hyper-util features: client, client-legacy, client-proxy, default, http1, server, service, tokio
hyper       features: client, default, http1, server
$ cargo tree -p sandhi-proxy -e normal | grep -c '^h2 v'
0
```

`reqwest` does not enable `http2` (its resolved features are `json, stream, rustls-tls, blocking`
plus the `__rustls` chain). The **only** enabler anywhere in the graph is `wiremock`, a
*dev*-dependency — and the workspace declares `resolver = "2"` (`Cargo.toml:4`), under which
dev-dependency features are not unified into normal builds.

**This matters beyond the fact being wrong.** The draft's experiment proposed proving it with
"`curl --http2-prior-knowledge` … plus an `h2` client request in a Rust test." A Rust *test* binary
**does** get wiremock's features, so h2 would compile there while being absent from the shipped
binary. The two halves would have disagreed, and the draft's own "h2c half-works" row — which it
called "the worst case and the most likely to be sitting there unnoticed" — is exactly what a
careless reading of that split result produces. The experiment as designed could have confirmed a
surface that does not exist in production.

## First principles

1. **A credential-holding proxy that cannot terminate TLS has an unstated hard dependency.** Either
   it terminates TLS, or the fronting requirement is a documented, loudly-warned precondition. It is
   currently neither.
2. **An undeclared protocol surface is worse than an absent one.** Anything reachable must be tested;
   anything untested must be unreachable. G06 is currently in the gap between those.
3. **Do not implement TLS; configure it.** ADR-0006 D5 forbids owning cryptography. This TD wires
   `rustls` and owns exactly one thing beyond configuration: certificate rotation, which is an
   operational lifecycle concern rather than a cryptographic one.
4. **Protocol breadth is only worth it when it unlocks a named use case.** HTTP/2 unlocks gRPC-shaped
   upstreams and multiplexed clients. HTTP/3 currently unlocks nothing anyone has asked for, so it is
   a non-goal until it does.
5. **Rotation without dropping streams.** A model stream can run for minutes. A certificate reload
   that kills in-flight streams would settle every one of them as `Partial` — turning a routine
   operational event into a metering anomaly.

> **Update 2026-09-01 — the h2c question is resolved, and a listener decision
> landed ahead of this TD.** TD-0014 P3 found that hyper-util's auto builder
> (what `axum::serve` used) sniffs for an h2c preface before arming any
> timeout, exempting zero-byte connections from every defence — demonstrated
> as a full-traffic wedge. The listener now binds hyper's **http1 builder
> directly** and cleartext h2c is refused outright: recorded as
> [ADR-0009](../adr/0009-http1-only-listener.md). Consequence for this TD's
> h2 path: h2 returns only over TLS/ALPN, and **its builder sets
> `header_read_timeout`, `max_buf_size`, and a timer in the same commit** —
> with the silent-connection regression test extended to the h2 path. The
> P0 experiment below is retained as the record of how the question was
> originally (wrongly) framed.

## Non-goals

- **No HTTP/3 or QUIC.** No named use case, and it would add `quinn`/`h3` plus a UDP path to a
  system with no UDP story. Revisit when a customer requirement exists.
- **No mutual TLS in P1.** Client certificates are a plausible zero-trust follow-up, but virtual keys
  are the identity mechanism and mTLS would be a second one. Sequenced behind, and only if asked for.
- **No TLS passthrough or SNI-based routing.** Structurally impossible: Sandhi must read the JSON
  body to meter (ADR-0006 §Context F3). Named here only because it is a recurring request.
- **Sandhi does not become the edge.** TLS termination here is for the trusted-network hop and for
  deployments with no fronting proxy — not a claim to be internet-facing (TD-0012's non-goal stands).

## Decisions

**D1 — Settled: there is no undeclared h2 surface. HTTP/2 ingress is a deliberate feature, to be
sequenced on merit.** The dependency graph answers this without a running proxy (see above), so the
outcome the draft feared — a live, untested HPACK path — does not exist. Two consequences:

- The TD-0010 parity matrix does **not** need an HTTP-version axis today.
- If h2 ingress is later wanted, it is a scoped change (enable `axum/http2`, add the parity runs),
  not a cleanup of something already reachable.

Any future test of this must assert against a **non-dev** build. A Rust test binary links
`wiremock`'s features and will happily speak h2 while the shipped binary cannot — the trap the
draft's own experiment would have walked into.

**D2 — TLS is opt-in configuration over `rustls`, never a reimplementation.** `SANDHI_TLS_CERT` /
`SANDHI_TLS_KEY` enable it; absent, the listener stays plaintext exactly as today. Rejected: TLS on
by default with a self-signed certificate — it trains operators to ignore certificate errors, which
is a worse security outcome than plaintext on a trusted network.

**D3 — Certificate rotation reloads without dropping connections.** Rotation swaps the
`rustls::ServerConfig`'s certificate resolver behind an `ArcSwap`-style handle, so new handshakes use
the new chain and established connections — including multi-minute model streams — are untouched.
Triggered by an explicit signal (SIGHUP) and/or a file-mtime watch. Rejected: process restart as the
rotation mechanism, which would settle every in-flight stream as `Partial` and corrupt the metering
record for a routine event.

**D4 — ALPN advertises exactly what is tested.** The ALPN list is derived from the tested protocol
set, not hand-written. If D1 concludes h2 is supported, ALPN offers `h2` and `http/1.1`; otherwise
`http/1.1` alone. A protocol advertised in ALPN and untested is D1's failure mode with extra steps.

**D5 — Peer and TLS metadata flow into `ConnCtx`, and no further.** Negotiated ALPN and SNI join
TD-0014 D4's `ConnCtx`. They are available to admission decisions and **must not** reach
`RequestMetadataV1`, the usage event, or any metric label — same bounded-cardinality discipline as
TD-0011 D2, and SNI is caller-controlled.

**D6 — Until TLS ships, the plaintext posture is loud.** The proxy already warns when `SANDHI_STORE`
is set without an admin token (`main.rs`, the ADR-0004 D4 footgun warning). A startup warning when
binding a non-loopback address without TLS follows the same established pattern: fail loudly, not
silently, and do not fail-closed on a posture that many dev setups legitimately rely on.

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| ~~**P0**~~ | ~~D1 — the h2 experiment~~ | **Done during fact-check, by dependency-graph inspection rather than a running proxy.** `h2` is not in the non-dev graph; there is no undeclared surface. No code needed |
| **P1** | D2 + D6 — TLS termination, opt-in | With cert and key configured, a TLS client completes a full request and a full SSE stream; without them, behaviour is byte-identical to today; binding a non-loopback address without TLS emits the startup warning |
| **P2** | D3 — rotation | A certificate is replaced while an SSE stream is in flight: the stream completes uninterrupted and settles `Final` (not `Partial`), and a *new* connection presents the new chain. This test is the whole point of the phase |
| **P3** | D4 + D5 — ALPN and `ConnCtx` | ALPN offers exactly the tested set; a client negotiating each offered protocol completes a request on every ingress dialect; `ConnCtx` carries ALPN/SNI and they appear in no usage event and no metric label |
| **P4** | HTTP/2 ingress, only if P0 says it is worth declaring. **Carried requirement from ADR-0009:** the h2 builder sets `header_read_timeout`, `max_buf_size`, and a timer in the same commit, and the silent-connection regression test extends to the h2 path — the sniffing-bypass class must not return with the new protocol | Every ingress dialect passes its full test suite over h2 as well as h1 — the TD-0010 parity matrix gains an HTTP-version axis |

P0 is 30 minutes and gates P4 entirely. P1 is the P0-severity item and should not wait for anything.

## Pressure test

1. **"Everyone fronts this with nginx or a service mesh — TLS here is redundant."** Most do. The ones
   that do not are the single-node deployments the README explicitly supports, and they are running a
   bearer credential over plaintext without being told. D6 makes the posture visible even before D2
   makes it fixable; the combination is the actual fix.
2. **"Adding TLS puts Sandhi on the CVE treadmill."** It puts `rustls` there, which is already in
   `Cargo.lock` and already covered by `cargo audit`/`cargo deny` in CI. ADR-0006 D5 draws the line
   at *implementing* cryptography, not depending on it — the h2 RUSTSEC response at `8bc9d20` shows
   the process works.
3. **"The h2 experiment is a curiosity, not a gap."** It would have been a real gap had the draft's
   inference held — an HPACK path reachable but never exercised. It did not hold, and the cost of
   finding out was one `cargo tree` invocation. The lesson kept here is the *method*: a feature-tree
   question is answered by querying the feature tree with `--no-dev`, not by reasoning about
   unification rules from memory, and not by a test binary whose feature set differs from the
   binary that ships.
4. **"Certificate rotation without dropping connections is over-engineering for v1."** The
   alternative is that every rotation corrupts the metering record for every in-flight stream, in a
   product whose sole promise is accurate metering. It is cheap with `rustls`'s resolver hook and
   expensive to retrofit after the first rotation-day incident.
5. **"Opt-in TLS means nobody turns it on."** Correct incentive problem, wrong lever. D6 addresses it
   with a loud startup warning rather than a self-signed default, because a self-signed default
   teaches operators to click through certificate errors — a habit that outlasts the deployment.
6. **"This contradicts ADR-0006's 'stay L7'."** ADR-0006 D2 explicitly authorises L5/L6/L7 breadth
   and names TLS as the example. Terminating TLS is not acquiring a data plane; it is closing the
   hole that forces someone else to.

## Resolved

**R1 — TLS certificate and key *paths* live in `SANDHI_CONFIG`, not env vars.** The file is
git-committed and must never carry secrets (`sandhi-proxy/src/config.rs` module docs), and a path is
not a secret — the same reasoning that already puts `secret_env` *names* in the file while the
secrets themselves stay outside it. This also makes TLS reviewable in the same change as the rest of
the declarative operator surface.

**R2 — Certificate and key are loaded and validated as a pair before any swap.** A rotation that
reads the two files independently can observe a new certificate against an old key mid-write and
serve a chain that no client will accept. Load both, verify they match, then swap — or keep the
previous pair and log loudly. A rotation must never be able to make things worse than not rotating.

**R3 — SIGHUP in P2; the file-mtime watcher is deferred and optional.** SIGHUP is simpler, has no
new dependency, and composes with the cert-manager sidecar pattern that most deployments already
use. The watcher is friendlier for bare-metal and can be added behind the same reload path once
something asks for it.

## Still open

- ~~Is declaring `axum/http2` a behaviour change or a formality?~~ **Moot.** There is no existing
  h2 surface to declare, so enabling it would be a plain feature addition. This was the single
  unresolved question in [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md), and it is now
  closed on evidence.
