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

The second half is smaller and stranger. **Nobody knows which HTTP versions Sandhi's ingress
actually accepts.** `axum`'s enabled feature set is `form, http1, json, matched-path, original-uri,
query, tokio, tower-log, tracing` — `http2` is **not** enabled. But `axum::serve` dispatches through
`hyper_util::server::conn::auto::Builder` (`axum-0.7.9/src/serve.rs:254,423`), and `hyper-util`'s
`http2` feature **is** globally enabled by Cargo feature unification, pulled in by `reqwest` on the
client side. *Inference:* h2 prior-knowledge (h2c) ingress is probably compiled in and probably
works, entirely by accident, and is tested by nothing.

Either answer is actionable. If h2c works, Sandhi has an undeclared, untested protocol surface —
which is a correctness and security concern, not a feature. If it does not, the gap is real and
named. What is unacceptable is not knowing.

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

**D1 — Run the h2 experiment before writing any HTTP-version code.** A ~30-minute test:
`curl --http2-prior-knowledge` against a running proxy, plus an `h2` client request in a Rust test.
Three possible outcomes, each with a different consequence:

| Outcome | Consequence |
|---|---|
| h2c works | An undeclared surface exists. Declare it (enable `axum/http2` explicitly), test it against every ingress dialect, and add it to the TD-0010 parity matrix |
| h2c is cleanly refused | Enabling h2 is a deliberate, scoped feature, sequenced on merit |
| h2c half-works (accepts the preface, then misbehaves) | The worst case and the most likely to be sitting there unnoticed. Treat as a defect and fix by explicitly disabling or explicitly supporting |

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
| **P0** | D1 — the h2 experiment | A committed test asserting the *actual* h2 prior-knowledge behaviour, whatever it turns out to be. Result recorded in this TD and, if a surface is undeclared, treated as a defect |
| **P1** | D2 + D6 — TLS termination, opt-in | With cert and key configured, a TLS client completes a full request and a full SSE stream; without them, behaviour is byte-identical to today; binding a non-loopback address without TLS emits the startup warning |
| **P2** | D3 — rotation | A certificate is replaced while an SSE stream is in flight: the stream completes uninterrupted and settles `Final` (not `Partial`), and a *new* connection presents the new chain. This test is the whole point of the phase |
| **P3** | D4 + D5 — ALPN and `ConnCtx` | ALPN offers exactly the tested set; a client negotiating each offered protocol completes a request on every ingress dialect; `ConnCtx` carries ALPN/SNI and they appear in no usage event and no metric label |
| **P4** | HTTP/2 ingress, only if P0 says it is worth declaring | Every ingress dialect passes its full test suite over h2 as well as h1 — the TD-0010 parity matrix gains an HTTP-version axis |

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
3. **"The h2 experiment is a curiosity, not a gap."** An HTTP/2 implementation that is reachable but
   never exercised is a security surface with zero test coverage. If h2c is live, a malformed HPACK
   frame reaches a code path nothing in this repository has ever run. That is not a curiosity.
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

- **If P0 finds h2c already works, is declaring `axum/http2` a behaviour change or a formality?**
  Gated on P0 itself — the experiment has to run before this can be answered, and the P4 parity
  matrix runs either way. This is the single remaining unresolved question in
  [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md).
