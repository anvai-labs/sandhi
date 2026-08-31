# Full-integration handoff — InferFlux ↔ Sandhi ↔ Victor ↔ ProximaDB

Date: 2026-08-31 · Companion to [ADR-0008](../adr/0008-inferflux-admission-and-session-affinity.md)
and [inferflux-issue-drafts.md](inferflux-issue-drafts.md)

**Goal ("definition of done"):** a secured InferFlux with a loaded model, fronted by the
sandhi proxy, driven by victor agents in gateway mode — with transparent-plane forwarding,
session KV-reuse affinity, per-run cost trees, cache-split accounting, and the control-plane
consuming complete `UsageEvent`s. Everything below is what stands between the current state
(all core integration implemented and green) and that end state.

## Current state (all implemented, all green, **uncommitted**)

| Repo | Working tree | Verified by |
|---|---|---|
| sandhi | 14 files: catalog row + `session_header` fact, both-plane affinity injection, fixtures/corpus, proxy e2e, bindings matrices, ADR-0008, operator guide, issue drafts | fmt/clippy `-D warnings`/`test --workspace`, coverage ≥75%, zero schema drift, py 26/26, node 19/19 |
| victor | 9 files: `InferfluxProvider` + YAML policy + registry + key handling + `metadata.session_id`/`x-sandhi-session` stamping + tests + docs | inferflux/drift-guard/run-tree/binding-construction 21/21, config 394/394, resource-detector 20/20 |
| inferflux | untouched | live smoke: wiring verified to the auth boundary; **auth case-sensitivity bug found** |

The live green path is blocked only by InferFlux-side facts: the auth bug (I1) and no loaded
model. Sandhi/victor wiring is done and pinned by tests.

---

## LANES — launch in parallel

### Lane S — sandhi (hub repo, `anvai-labs/sandhi`)

**S1 · Land the integration PR** *(critical path, start now)*
Branch the current working tree → PR to `develop`. All CI gates already pass locally
(fmt, clippy `-D warnings`, workspace tests, coverage ≥75%, codegen-drift — schemas
regenerate byte-identical). No agent attribution in commits (hook + CI enforced).
*Done when:* `CI Success` green and merged.

**S2 · Cut the release** *(gated by S1)*
Follow the 0.2.0 runbook: CHANGELOG PR → tag → crates.io + PyPI + npm + GH binaries,
`scripts/verify-release.py`. Version stamping is external (workspace is `0.0.0`); the bump
**must be a minor** (0.3.0), never a patch: the stack makes compile-breaking changes to
`sandhi-providers`' public Rust API (`ChatProvider::complete`/`stream` gained a mandatory
`call_headers` parameter, `ProviderRequest` gained the public `extra_headers` field, and
`RawForwarder::forward_metered`/`forward_stream_metered` widened), so a consumer pinned to
`^0.2` must not auto-upgrade into it. Note for consumers: **`CHAT_CONTRACT_MINOR` is
unchanged** (zero schema drift) — no contract-handshake bumps needed downstream.
*Done when:* the wheel/carrier the consumers pin is live. **Unblocks V1, V2, P2.**

**S3 · Wire `derive_session_id` on the proxy** *(independent, small)*
Today the proxy reads only an explicit `x-sandhi-session` header
(`crates/sandhi-proxy/src/lib.rs` ~1618–1642); `derive_session_id(explicit, body)` exists in
core (`chat.rs:488-502`) but is never called by the proxy. A stock OpenAI/Anthropic SDK
client that sets `user` (and no sandhi header) therefore gets **no session id** → no KV
affinity, no session-grouped usage. Wire the derive (explicit header wins, then body
metadata, then derived). Tests for precedence + stability (mirror `chat.rs` unit tests).
*ADR-0008 D3 follow-up; noted in the ADR's consequences.*

**S4 · Request-correlation header fact** — **DONE** (2026-08-31, PR #178, stacked):
`client_request_id_header` catalog fact + caller-owned injection on both planes. The proxy
now mints the request id **at admission** (RequestAccounting) instead of lazily at event
assembly; the same string rides the vendor's declared header (`x-inferflux-client-request-id`)
and becomes the event's `request_id` — upstream logs and sandhi events correlate 1:1
(ADR-0008 D6). Decision recorded for the seam TD-0021 G20 shares: mint-early (uniform
coverage) over idempotency-key-only; the G20 dedup lookup itself stays TD-0021's item.

**S5 · `traceparent` response passthrough** *(independent, small; pre-existing gap, all providers)*
`RawForwarder::filter_response_headers` (`raw.rs:404+`) drops the child `traceparent`
InferFlux echoes on responses. Add it to the curated allowlist + test. Closes W3C trace
correlation end-to-end through the transparent plane.

**S6 · Per-request metadata in the bindings** *(independent, medium; pre-existing ecosystem backlog — TD-SANDHI-3 candidate)*
FFI surface to pass per-request metadata (not fixed-at-handle-construction wire headers).
Unblocks victor's `x-sandhi-step-id` → step-level cost-tree nodes under `run_id`. See the
ecosystem backlog (this is the same item tracked since PR #149).

**S7 · Fixture refresh after I2/I3 land** *(gated by InferFlux I2/I3, trivial)*
Add a second corpus fixture pair captured from a real InferFlux carrying
`prompt_tokens_details.cached_tokens` (+ timings) and assert the split meters
(`cache_read_tokens > 0`, fresh = prompt − cached) with **zero code changes** — the
regression proof of the ADR-0008 "lights up automatically" claim.

**Discovery note (no work item):** `/v1/models` on the proxy lists `catalog ∩ vkey
allowlist` **plus any allowlisted ids not in the catalog** (`permitted_models`,
`lib.rs:1465-1485`) — so for InferFlux (empty catalog) discovery works exactly when the
operator mints the vkey with an explicit `--models` list, which they must anyway since ids
are their own config. Optional polish, not a gap: live upstream `/v1/models` passthrough for
catalog-empty providers (TD-0010 D3 extension) — file only if an operator asks.

### Lane I — InferFlux (`~/code/inferflux`, C++; independent of Lane S — start now)

Issue bodies ready to paste: [inferflux-issue-drafts.md](inferflux-issue-drafts.md).

**I1 · Case-insensitive `Authorization` (P0 — blocks every secured deployment from Rust/HTTP2 clients)** *(small)*
Verified live 2026-08-31: title-case `Authorization` authenticates; lowercase/uppercase →
`401 {"error":"unauthorized"}` on **both** `/v1/models` and `/v1/chat/completions`.
reqwest/hyper (sandhi, and most modern clients) always send lowercase → any InferFlux with
`api_keys` configured is unreachable. Fix the header lookup in the request parser
(`server/http/http_server.cpp`, auth path ~1374–1417 + the header map access), and **sweep
all parsed headers** (`x-inferflux-session-id`, `x-inferflux-client-request-id`,
`traceparent`, `content-type`) for the same class — RFC 9110 §5.1 field names are
case-insensitive. *Done when:* a sandhi-proxied call to a secured InferFlux succeeds
end-to-end (with a model loaded). **Unblocks the live green path.**

**I2 · Per-request prompt-cache split in `usage`** *(medium)*
Thread the per-request radix-prefix-cache hit count the scheduler already computes
(`InferenceResult` / `runtime/prefix_cache/`) into the usage object — OpenAI nested form
preferred: `"prompt_tokens_details": {"cached_tokens": N}` with `prompt_tokens` staying
inclusive; DeepSeek top-level hit/miss also parses. Both the JSON body
(`http_server.cpp` ~728-730) and the streaming terminal usage frame (~3277-3289), on
`/v1/chat/completions` and `/v1/completions`. Emit explicit `0` on miss (not omitted).
*Done when:* a sandhi `UsageEvent` shows `cache_read_tokens > 0` with no sandhi change.

**I3 · `duration_ms` + `time_to_first_token_ms` in `usage`** *(small)*
Already measured by the scheduler; expose on the same usage frame (TTFT streaming-only).
Sandhi keeps gateway-measured latency; these enable server-side correlation and close the
gateway-vs-server timing discrepancy.

**I4 · Ops guidance for gateway-fronted deployments** *(docs/config, small)*
Recommended production posture when fronted by sandhi:
`runtime.scheduler.session_handles.enabled: true` (the affinity header sandhi sends is
otherwise a no-op), plus capacity sizing — session leases cap at 1024 with TTL 300 s, so
size for peak concurrent agent *conversations* (not requests); document the eviction
behavior. Also correct the CLAUDE.md wire-surface claim (REST/gRPC/WebSocket — only
HTTP/1.1 exists).

**I5 · (Optional, perf) keep-alive across streams** — currently every stream closes the
connection (`Connection: close` post-stream), costing a TCP handshake per streaming call
through the gateway. Low priority; document if not fixed.

### Lane V — victor (`~/code/victor`; gated by S2)

**V1 · Land the provider PR** *(after S2's wheel is on PyPI)*
Current working tree (9 files). Tests already pass against a binding built from sandhi
develop; CI needs the published release. Do **not** add `inferflux` to
`LOCAL_OPENAI_COMPAT_NO_DESCRIPTOR` (drift guard enforces the descriptor exists).
*Done when:* victor CI green on its default branch.

**V2 · Pin bump** — `sandhi-gateway==0.1.6` → the S2 release version in `pyproject.toml`.
`KNOWN_CONTRACT_MINOR` needs **no** change (contract minor untouched by the integration).

**V3 · Dev-env hygiene** — the anaconda-base env currently carries a force-installed
`0.0.0-dev` local wheel (was `0.1.2`) used to verify the drift guard. After S2:
`pip install sandhi-gateway==<release>` to restore a clean pin.

**V4 · (Optional) end-user runbook entry** — a `~/.victor/profiles.yaml` example block for
inferflux in both modes (direct FFI; gateway with `providers.inferflux.gateway.{url,
virtual_key}`), linking the provider guide section added in this change.

### Lane P — proximaDB (`~/code/proximaDB`; P1 independent, P2 gated by S2)

**P1 · Populate the new usage fields at emit sites** *(medium; pre-existing backlog)*
Its emit seam doesn't measure latency — `duration_ms`/`time_to_first_token_ms`/
`usage_completeness`/`usage_basis` are parsed but never populated. Not inferflux-specific;
completes ecosystem-wide meter hygiene.

**P2 · `cargo update -p sandhi-core`** to the S2 release once published (mirror of the
0.1.5 refresh in #1394).

### Lane A — AnvaiOps control plane (parent org; out of these repos — interface note only)

Consumes neutral `UsageEvent`s and owns pricing. Nothing blocking from this integration;
when I2 lands, self-hosted InferFlux fleets gain cache-read discounts as a pricing-policy
input (`cache_read_tokens` already rides the wire contract). No dollars ever originate in
sandhi/inferflux/victor (measure-vs-price line, ADR-0001).

---

## Dependency graph

```
S1 (land PR) ──► S2 (release) ──► V1 (victor PR) ──► V2/V3
                       │
                       └──► P2 (proximaDB pin)

I1 (auth fix)   ─ independent, start now ─ after S1+I1: live secured e2e possible
I2 (cache split)─ independent ──► S7 (sandhi fixture refresh, trivial)
I3 (timings)    ─ independent
S3/S4/S5/S6     ─ independent sandhi follow-ups, any order
P1              ─ independent
I4/I5, V4, Lane A ─ docs/ops, anytime
```

**Can start today, fully parallel:** S1, S3, S4, S5, S6, I1, I2, I3, I4, P1.
**Wait on S2:** V1, V2, V3, P2. **Wait on I2/I3:** S7. **Wait on S1+I1:** the live
green-path e2e below.

## Full-stack acceptance run (the last mile — after S1+S2+V1+I1, ideally I2)

1. `inferfluxd` (secured: real key; a model loaded in its registry) with
   `runtime.scheduler.session_handles.enabled: true`.
2. sandhi proxy: `keys add inferflux default` (catalog default base URL) + real key;
   `keys share inferflux:default --subject … --models <real ids> --rate …`.
3. victor agent in gateway mode (`providers.inferflux.gateway`) runs a multi-turn session.
4. Assert: transparent plane selected (debug log `plane=transparent`), verbatim SSE to the
   client, `UsageEvent` with `provider=inferflux` + key-authoritative subject/group,
   `session_id` set, run cost tree populated (`sandhi usage --run`), `x-inferflux-session-id`
   observed by InferFlux with a KV lease reused across turns, and — once I2 lands —
   `cache_read_tokens > 0` with fresh input = prompt − cached.
5. Dashboard/alerts over the store show the session and run rollups.
