# Changelog

All notable changes to **Sandhi** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Sandhi is an **AI usage gateway** that emits neutral **units** (tokens, the
prompt-cache split, GPU-seconds) and never dollars. See
[ADR-0001](docs/adr/0001-sandhi-architecture-and-wire-contract.md) for the
architecture and the measure-vs-price boundary this changelog respects.

One tag `vX.Y.Z` releases everything together — the `sandhi-proxy` binaries, the
PyPI wheel (`sandhi-gateway`), the crates.io libs, and the npm package
(`@anvai-labs/sandhi`). Versions are derived from the tag at build time, never
hand-edited; see [RELEASING.md](RELEASING.md).

## [Unreleased]

_Nothing yet._

## [0.1.4] — 2026-07-26

The adoption release: all three major vendor SDKs now work against the proxy **unmodified**, proven in CI by driving the vendors' own clients (TD-0010), and the usage aggregate becomes a versioned contract type shared by the proxy, the store and both bindings (TD-0009).

### Added

- **Model discovery per dialect** (TD-0010 D3) — `GET /v1/models` in OpenAI *and* Anthropic
  shape (the credential presentation identifies the client, since both SDKs use that path) and
  `GET /v1beta/models` for Gemini. The listing is the key's **permitted** models: the upstream
  catalog intersected with the virtual key's allowlist, so a scoped key discovers exactly what it
  may call instead of meeting a 403 at call time. Discovery is authenticated, because it reveals
  which models a credential can use. `.models.list()` now works unmodified on all three SDKs
  (LangChain, LiteLLM health checks and most chat UIs call it before anything else).
- **Cross-family Gemini translation** (TD-0010 D4b) — a Gemini client may now resolve to *any*
  upstream. D4a admitted Gemini on the transparent plane only and refused cross-family with a 501,
  because its decode was accounting-grade and re-encoding from it would silently drop tools, inline
  media and safety settings. The decode is now faithful — the mirror of the adapter's request
  encoder — and responses are rendered back in Gemini's shape (`candidates[]`, `usageMetadata`,
  `functionCall.args` as an object rather than the OpenAI-family JSON string), so a `google-genai`
  client talking to an OpenAI upstream never learns the difference. Gemini fields with no neutral
  equivalent (`safetySettings`, `cachedContent`, `responseSchema`, `topP`) are preserved in
  `extensions` instead of being dropped.
  - **Known limitation:** a *streamed* tool call is not rendered in Gemini's shape. Gemini has no
    partial-function-call frame — it sends one complete `functionCall` part — while the canonical
    stream reports start / argument-deltas / end. Emitting those as text would corrupt the client's
    parse, so nothing is emitted; non-streaming tool calls translate fully.
- **Gemini ingress dialect** (TD-0010 D4a) — `POST /v1beta/models/{model}:generateContent` and
  `:streamGenerateContent`, authenticated with the `x-goog-api-key` header. A `google-genai`
  client now points its `base_url` at Sandhi unmodified. Gemini is the first dialect whose model
  and streaming choice live in the **path** rather than the body, which reaches the model
  allowlist, the reservation, and the upstream URL.
  - Admitted on the **transparent plane only**: a Gemini client must resolve to a Gemini upstream,
    where the body is forwarded byte-for-byte and metered in flight. Cross-family is **refused**
    (501, Google's error shape) rather than translated from an accounting-grade decode that would
    silently drop tools, inline media and safety settings (D4b will lift this).
  - The documented `?key=` query form is **not** accepted — it would put a live virtual key into
    URLs, access logs and crash reports. Stated as a gap in the README matrix instead.
  - `SANDHI_GEMINI_KEY` / `SANDHI_GEMINI_BASE` register a Gemini upstream.

- **SDK-conformance suite** (TD-0010 D5) — `tests/sdk-conformance/` starts the real `sandhi-proxy`
  binary against a mock upstream and drives it with the **vendors' own clients**
  (`openai-python`, `anthropic-python`), asserting that pointing `base_url` at Sandhi with a
  virtual key needs no other client change. It also asserts the virtual key never reaches the
  provider and that the real upstream credential is substituted server-side. Every other test in
  this repo hand-rolls the request, which is how an Anthropic ingress that no stock SDK could
  authenticate against shipped unnoticed; this suite is verified to catch that defect by reverting
  the fix and watching it fail. Gated in CI as a required check, and the README's compatibility
  matrix is now backed by it.
- **`include_native_response` request gate** — opt out of native-body emission per request
  (G8, additive within v1). ([#90](https://github.com/anvai-labs/sandhi/pull/90))
- **Upstream request ids from more vendors** — Moonshot's `Msh-Request-Id` with a `cf-ray`
  fallback, then generalised so the request-id header is a **spec transport fact** rather than a
  vendor branch in shared code. ([#91](https://github.com/anvai-labs/sandhi/pull/91),
  [#92](https://github.com/anvai-labs/sandhi/pull/92))
- **`SANDHI_ANTHROPIC_BASE`** — base-URL override for the Anthropic upstream, symmetric with the
  long-standing `SANDHI_OPENAI_BASE`. Without it the Anthropic upstream could only ever be the
  public API: no Anthropic-compatible gateway, no local mock, and no way to test that path.


- **In-process usage snapshots on both bindings** (TD-0009 P2) — `Gateway.usage_snapshot_json()`
  (Python) / `Gateway.usageSnapshotJson()` (Node) fold the events the gateway recorded into
  `UsageAggregateV1` rows for one attribution dimension (`subject`/`user`, `group`, `provider`,
  `model`, `key`/`virtual_key`, `session`, `total`), closing the gap where the in-process path —
  the one Victor uses — had no aggregation surface at all. The fold is `sandhi-core`'s, the same
  one the proxy, the CLI and the dashboard read, so the two shapes cannot disagree; no binding
  links `sandhi-store`, so the wheels stay SQLite-free. An optional `cap` bounds distinct keys
  before the rest fold into `"(overflow)"` (default 1024) — per-key detail is lost, the sum never
  is. Python and Node assert byte-identical snapshots against one shared corpus
  (`bindings/fixtures/usage-snapshot-parity.json`), so parity fails in CI rather than in review.

### Fixed

- **Client-facing failures are rendered in the caller's dialect** (TD-0010 D2, completed). Beyond
  the auth paths below, a missing upstream registration and both transparent-plane failures
  returned a bare `{"error": "<string>"}` on vendor routes — unparseable by the SDK that made the
  call, which defeats its own error handling. All client-facing paths now use the dialect
  renderer. Sandhi's **operator** API (`/admin/*`, `/catalog/models`, `/dashboard/api/*`) keeps
  its flat envelope on purpose: it is consumed by the `sandhi` CLI, not by a vendor SDK, and has
  no client dialect to render.
- **Auth failures are rendered in the caller's dialect** (TD-0010 D2, auth slice). A missing,
  expired or unknown virtual key returned a flat `{"error": "<string>"}` that two of the three
  SDKs cannot parse, and its text told every client to send `Authorization: Bearer` — advice that
  is wrong for Anthropic (`x-api-key`) and Gemini (`x-goog-api-key`). The 401 now uses each
  dialect's envelope and names that vendor's own scheme.


- **The Anthropic SDK works unmodified against `/v1/messages`** (TD-0010 D1). The proxy read the
  client's virtual key from exactly one place — `Authorization: Bearer` — while the official
  Anthropic SDK authenticates with `x-api-key`, so `anthropic.Anthropic(base_url=…,
  api_key="vk_…")` got a **401** and the drop-in promise held only for OpenAI clients. Credential
  extraction now belongs to the **ingress dialect**, which owns the whole client-facing contract
  rather than just the body schema: `/v1/messages` accepts `x-api-key` (preferred) **and**
  `Authorization: Bearer`; `/v1/chat/completions` and `/v1/responses` stay Bearer-only, so no
  cross-vendor auth scheme is invented. Existing Bearer clients are unaffected, and missing or
  malformed credentials still fail closed with a 401.

## [0.1.3] — 2026-07-24

The enforcement release: budget enforcement moves off the volatile in-memory
ledger onto a **durable lease ledger** (crash-safe, calendar-windowed), and the
proxy gains a **transparent-metering plane** that forwards same-family traffic
byte-for-byte instead of re-encoding it.

> Tagged from `main` after the `develop → main` promotion PR merges.

### Added — enforcement (ADR-0005)

- **Lease-based enforcement ledger** — the trait plus an in-memory
  implementation: reserve a conservative *ceiling*, settle by lease id.
  ([#54](https://github.com/anvai-labs/sandhi/pull/54))
- **Enforcement integration in the proxy** — ceiling reservation, `billable()`
  settle, partial-on-disconnect, and identity (D1/D4/D7).
  ([#57](https://github.com/anvai-labs/sandhi/pull/57))
- **Durable SQLite enforcement ledger** — spend, caps, and in-flight leases
  survive a restart; dangling leases are reclaimed (D2/D5, Phase 3).
  ([#58](https://github.com/anvai-labs/sandhi/pull/58))
- **Ledger windows + policy + windowed spend** — spend measured over
  calendar-aligned daily/monthly/total windows, block/warn policy, and per-tier
  fail-open/closed on a backend error (D5/D6).
  ([#62](https://github.com/anvai-labs/sandhi/pull/62))

### Added — data plane (ADR-0004 / TD-0006)

- **Data-plane raw forwarder** — `RawForwarder`, bounded `metered_passthrough`
  (O(1) memory), and a `ProviderFamily` accessor.
  ([#56](https://github.com/anvai-labs/sandhi/pull/56))
- **Same-family `RawForwarder` on `ProviderHandle`** and a public metered
  raw-forward entry point (TD-0006 enablers).
  ([#59](https://github.com/anvai-labs/sandhi/pull/59),
  [#60](https://github.com/anvai-labs/sandhi/pull/60))
- **Transparent-metering plane** — same-family byte passthrough in the proxy,
  metered without re-encoding (ADR-0004 D1).
  ([#61](https://github.com/anvai-labs/sandhi/pull/61))

### Added — metering

- **Latency + reasoning-token fields on `UsageEvent`** — `duration_ms`,
  `time_to_first_token_ms`, and `reasoning_tokens`, measured at the adapter
  boundary by `MeteredProvider` (TTFT on the first delivered stream item,
  duration at the Drop-guarded emit). Reasoning tokens are parsed where a
  provider reports them separately (OpenAI Chat + Responses
  `*_tokens_details.reasoning_tokens`, Gemini `thoughtsTokenCount`); Anthropic
  folds thinking into `output_tokens`, matching the `billable()` folding
  invariant. **Additive within wire-contract v1** — the fields are optional and
  skipped when absent, so existing consumers stay byte-identical; `sandhi-store`
  migrates with an idempotent `ALTER`.
  ([#68](https://github.com/anvai-labs/sandhi/pull/68))

### Added — catalog

- **Seed compat vendor lineups + Node catalog parity** (TD-0004).
  ([#49](https://github.com/anvai-labs/sandhi/pull/49))

### Added — project & community

- **CHANGELOG.md** (Keep a Changelog), **SECURITY.md** (private reporting via
  GitHub Security Advisories), **CODE_OF_CONDUCT.md** (Contributor Covenant
  2.1), GitHub issue templates, and a pull-request template.

### Changed

- **Neutral identity + one `billable()`** — contract-level identity and a single
  cache-inclusive billable primitive that budgets meter on, plus security
  quick-wins. **Wire-affecting:** the `usage-event.v1` and `chat-request.v1`
  schema digests changed (ADR-0005 D4/D7, ADR-0004 D4). The later
  `usage-event.v1` additions in [#68](https://github.com/anvai-labs/sandhi/pull/68)
  are optional-and-skipped, so they are **not** breaking.
  ([#55](https://github.com/anvai-labs/sandhi/pull/55))
- **Enforcement repointed onto the durable lease ledger** — `ProxyLedger` is
  durable and crash-safe when `SANDHI_STORE` is set, volatile in-memory
  otherwise (ADR-0005 step 2).
  ([#63](https://github.com/anvai-labs/sandhi/pull/63))

### Documentation

- **ADR-0005** — pressure-tested enforcement-correctness design (lease ledger,
  reserve-ceiling → settle-`billable`, calendar windows, dangling-lease reclaim,
  per-tier fail-open/closed).
  ([#52](https://github.com/anvai-labs/sandhi/pull/52))
- **TD-0007** — enforcement-ledger backends: contract, conformance suite, and
  backend choice. ([#64](https://github.com/anvai-labs/sandhi/pull/64))
- TD-0004 B–D converged with TD-0005/TD-0006 into one execution plan, then
  stitched to the ADR-0005 corrections; ADR-0004/TD-0005/README/CLAUDE.md
  reconciled with TD-0003 P2+P4 landed.
  ([#50](https://github.com/anvai-labs/sandhi/pull/50),
  [#51](https://github.com/anvai-labs/sandhi/pull/51),
  [#53](https://github.com/anvai-labs/sandhi/pull/53))

### Known limitations

- Per-minute **rate limits** are stored but **not enforced**; enforcement is
  **proxy-only** (the in-process bindings do not enforce).
- No shared/HA (Redis) ledger backend yet — a single-node SQLite ledger only.
- Ingress dialects are `/v1/chat/completions`, `/v1/messages`, and
  `/v1/responses`; there is no Gemini or Cohere ingress dialect.
- The dashboard read endpoints are **unauthed by design** (masked values only,
  self-hosted trust).

### Also in this release (documented after the tag was cut)

These landed before `v0.1.3` was tagged but were still filed under `[Unreleased]` at the time; recorded here so the release notes match what shipped.

### Added

- **Contract governance guards** (TD-0008 A) — `chat_contract_version()` exported from both
  bindings, a `stream_event_variant_tag()` exhaustive match so adding a `ChatStreamEventV1`
  variant fails compilation until a consumer decision is recorded, a census test cross-checking
  the tag list against the checked-in schema, and a test pinning the chat/usage version equality
  that consumer handshakes rely on. ([#73](https://github.com/anvai-labs/sandhi/pull/73))
- **Upstream request id on provider errors** — `ProviderErrorV1.request_id` was permanently
  `None`, dropping the identifier provider escalations are keyed on. It is now extracted from
  `x-request-id` / `request-id` / `anthropic-request-id` and appended to `Display`, so existing
  consumer logs quote it with no consumer change.
  ([#75](https://github.com/anvai-labs/sandhi/pull/75))
- **`SandhiProviderError` for Node** — Node consumers had to string-sniff `Error` messages to tell
  a provider error from a binding failure (the gap #69 closed for Python). The shim now raises a
  typed class carrying the parsed `ProviderErrorV1` at both provider-error surfaces, while
  binding-internal errors pass through untouched. Shim re-exports resynced with the addon.
  ([#76](https://github.com/anvai-labs/sandhi/pull/76))


- **`sandhi_core::billable_parts()`** — the single D4 formula over raw components, shared by
  `billable()`, `UsageEvent::billable_tokens()`, and the store's SQL.
- **Aggregates expose the full split.** `Bucket` gains `cache_creation_tokens`,
  `reasoning_tokens`, and an exact `billable_tokens`, surfaced in the dashboard table and
  `sandhi usage`. The SQL sums the D4 quantity **per row** — the reasoning fold is a per-call
  decision, so summing the columns first and folding afterwards gives a different, wrong answer;
  a conformance test pins the SQL against the Rust formula and asserts the naive form differs.

### Fixed

- **One billable definition everywhere.** ADR-0005 D4 defines the billable quantity as fresh
  input + the cache split + output (+ unfolded reasoning), and the proxy settled on it — but two
  other paths still used a narrower `tokens_in + tokens_out`, so the same call was counted
  differently depending on who asked:
  - the **in-process bindings** recorded the narrow number into the budget ledger, under-counting
    every cache read (a call with 40 fresh-input / 60 cache-read / 20 output recorded 60 while
    the proxy charged 120 — **2× under-count** on cache-heavy traffic);
  - the **dashboard and `sandhi usage`** ranked and displayed the narrow number, so an operator
    reconciling against a cap saw less than the ledger had actually charged.

  `UsageEvent::billable_tokens()` now returns the D4 quantity, and both it and `billable()`
  route through one shared `billable_parts()` so they cannot drift.

  > **Behaviour change:** in-process spend recorded via the Python/Node `Gateway` increases for
  > any call with cache reads/writes or separately-reported reasoning tokens. Budgets tightened
  > accordingly — a cap that was silently admitting more than it should now enforces as written.
  > Proxy enforcement is unchanged; it was already correct.

### Security

- **Dashboard reads are gated by default** (ADR-0004 D4). When an admin token is configured,
  `/dashboard` and `/dashboard/api/*` now require the admin bearer; `SANDHI_DASHBOARD_PUBLIC=1`
  restores the previous open, masked-only behaviour, and endpoints stay open when no admin token
  exists (there is no credential to present).
  ([#77](https://github.com/anvai-labs/sandhi/pull/77))
- **Client-facing provider errors are redacted by default** — code, HTTP status, request id, and a
  canonical short message. An upstream body can echo prompt fragments or infrastructure detail, so
  it is no longer returned to the client unless `SANDHI_ERROR_DETAIL=full` opts a single-tenant
  deployment in. Server-side logs always carry the full error.
  ([#77](https://github.com/anvai-labs/sandhi/pull/77))

## [0.1.2] — 2026-07-23

The operator release: keys, budgets, and attribution become operable from a CLI,
an admin API, and a dashboard.

### Added

- **Typed provider runtime** — `ProviderRuntime` / `ProviderHandle` normalize the
  neutral `ChatRequestV1` through per-family codecs (TD-0002).
  ([#38](https://github.com/anvai-labs/sandhi/pull/38))
- **`/v1/responses` ingress** over the canonical `ChatRequestV1`.
  ([#39](https://github.com/anvai-labs/sandhi/pull/39))
- **Gemini OAuth/ADC** bearer auth scheme.
  ([#40](https://github.com/anvai-labs/sandhi/pull/40))
- **Operator P1** — credential vault, virtual keys, admin API, and the `sandhi`
  CLI (TD-0003). ([#42](https://github.com/anvai-labs/sandhi/pull/42))
- **Operator P2** — budget windows, warn policy, reservation, and alerts.
  ([#46](https://github.com/anvai-labs/sandhi/pull/46))
- **Operator P4** — dashboard (keys / budgets / attribution / alerts) and
  **model-allowlist enforcement** (`vk.permits_model` in the request path).
  ([#47](https://github.com/anvai-labs/sandhi/pull/47))
- **Catalog TD-0004 Phase A** — curated model data + discovery surface.
  ([#44](https://github.com/anvai-labs/sandhi/pull/44))

### Fixed

- **`crates publish`** — `allow-dirty` (`set-version` dirties the tree) and
  dispatchable crates re-publish.
  ([#37](https://github.com/anvai-labs/sandhi/pull/37))

### Documentation

- **TD-0003** operator-surface spec (keys, virtual keys, budgets, attribution)
  and **TD-0004** catalog + unified governance dual-mode core; ADR/TD/README
  reconciled with code reality, adding the two-plane and declarative-policy
  designs. ([#41](https://github.com/anvai-labs/sandhi/pull/41),
  [#43](https://github.com/anvai-labs/sandhi/pull/43),
  [#45](https://github.com/anvai-labs/sandhi/pull/45))

## [0.1.1] — 2026-07-21

The decorator + QA release: metering and resilience become composable decorators
over any `Provider`, and the usage parsers gain a fixture-backed test corpus.

### Added

- **`MeteredProvider` decorator** — metering with Drop-guarded stream metering;
  the caller still assembles the `UsageEvent` (adapters never fabricate
  attribution). ([#35](https://github.com/anvai-labs/sandhi/pull/35))
- **Per-call timeouts** — `Timeout` taxonomy, `TimeoutConfig`, and idle policing
  in the resilience decorator.
  ([#31](https://github.com/anvai-labs/sandhi/pull/31))
- **Binding transport parity** — async `complete()` and `stream()` for Python and
  Node, plus the host-language provider escape hatches (`register_provider` /
  `registerProvider`) (ADR-0047 D10 steps 3a–3d).
- **TD-0001 QA corpus** — Anthropic usage-extraction fixtures, an
  OpenAI/Gemini/Cohere/Ollama usage corpus, a differential test oracle, and the
  `typify` narrow-model pilot.
  ([#18](https://github.com/anvai-labs/sandhi/pull/18),
  [#19](https://github.com/anvai-labs/sandhi/pull/19),
  [#20](https://github.com/anvai-labs/sandhi/pull/20),
  [#21](https://github.com/anvai-labs/sandhi/pull/21))
- **Binding FFI-glue coverage** — `scripts/coverage-bindings.sh` gates
  `bindings/*/src/lib.rs` at ≥85% lines (Python + Node).
- **Path-filtered CI** — docs-only changes skip the compile/coverage/binding
  jobs. ([#17](https://github.com/anvai-labs/sandhi/pull/17))

### Changed

- **Proxy metering via `MeteredProvider`** — the proxy adopts the decorator
  stack; the Python transport gains resilience.
  ([#33](https://github.com/anvai-labs/sandhi/pull/33))

### Fixed

- **Release pipeline** — pre-create the GitHub Release before uploading binaries
  (fixes "release not found").
  ([#12](https://github.com/anvai-labs/sandhi/pull/12))
- **Binding coverage** — requires a pyo3-compatible venv (guard + `COV_PYTHON`),
  builds with `maturin build` + `pip install`, and the binding jobs now trigger
  on a coverage-script change (paths-filter gap).

### Documentation

- **ADR-0002** — `sandhi-providers` scope + modality-admission discipline (the
  ≥2-consumer bar applies to *new* modalities, not chat adoption).
- **ADR-0003** — provider-adapter authoring + codegen, with the TD-0001 QA
  tracker. ([#16](https://github.com/anvai-labs/sandhi/pull/16))

## [0.1.0] — 2026-07-19

The first published release: the metering core, live provider transport, the
inline reverse-proxy, the durable store, and both language bindings.

### Added

- **`sandhi-core` metering engine** — usage accounting with the prompt-cache
  split, the `UsageEvent` wire type + `Sink`, virtual-key resolution, and
  budget/rate-limit types. Attribution (`subject_id` / `group_id` /
  `session_id`) rides outside the cached prompt.
- **Live HTTP transport** — OpenAI-compatible and Anthropic adapters with
  streaming SSE pass-through.
- **Resilience decorator** — retry + circuit breaker wrapping any `Provider`.
  ([#2](https://github.com/anvai-labs/sandhi/pull/2))
- **Gemini / Cohere / Ollama adapters** — plus the `FnProvider` escape hatch and
  more usage parsers. ([#6](https://github.com/anvai-labs/sandhi/pull/6))
- **`sandhi-proxy`** — the inline reverse-proxy gate: virtual keys, budgets,
  metering, and streaming pass-through.
- **`sandhi-store`** — durable SQLite sink plus the self-hosted usage
  **dashboard** at `/dashboard`.
  ([#8](https://github.com/anvai-labs/sandhi/pull/8))
- **Python in-process middleware** (`sandhi_gateway`) — the usage parsers moved
  into `sandhi-core` (they are metering primitives, not transport).
  ([#1](https://github.com/anvai-labs/sandhi/pull/1))
- **Node napi binding** (`@anvai-labs/sandhi`).
  ([#3](https://github.com/anvai-labs/sandhi/pull/3))
- **Host escape hatch** — `register_parser` callback (Python) + `meter_tokens`
  (both). ([#7](https://github.com/anvai-labs/sandhi/pull/7))
- **Unified tag-driven release pipeline** — binaries + PyPI + crates.io + npm.
  ([#4](https://github.com/anvai-labs/sandhi/pull/4))
- **CI gates** — a ≥75% line-coverage gate (`cargo-llvm-cov`) folded into
  `CI Success`, the no-attribution commit hooks, and `CODEOWNERS`.

### Changed

- **Python wheels target CPython 3.11+** (`abi3-py311`); EOL 3.9/3.10 dropped.
  ([#9](https://github.com/anvai-labs/sandhi/pull/9),
  [#10](https://github.com/anvai-labs/sandhi/pull/10))

[Unreleased]: https://github.com/anvai-labs/sandhi/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/anvai-labs/sandhi/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/anvai-labs/sandhi/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/anvai-labs/sandhi/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/anvai-labs/sandhi/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/anvai-labs/sandhi/releases/tag/v0.1.0
