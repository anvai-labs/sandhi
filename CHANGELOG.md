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

### Added

- _Nothing yet._

## [0.2.0] — 2026-08-31

The resource-safety release. The proxy's runtime hardening ships — bounded request bodies,
bounded background queues, graceful draining, explicit transport timeouts, and declarative
desired-state configuration — and stream metering gets honest about its bounds: decoding is
bounded and linear on **both** planes (TD-0014 P1), admission slots are now held for the whole
stream body rather than released at first byte (TD-0014 P2), and three silent undercounting
paths are fixed. The default `SANDHI_MAX_IN_FLIGHT_AI_REQUESTS` changes from **64 to 128**, with
the reasoning recorded; operators who pinned the variable are unaffected by the default move.
Design record: [ADR-0006](docs/adr/0006-layer-boundary-and-protocol-scope.md) (layer boundary +
the G01–G30 gap register) and TD-0014…0021.

### Added

- **Declarative desired-state configuration** (#165). `SANDHI_CONFIG` points at a committed
  `sandhi.json`; `GET /admin/config` previews the diff and `POST /admin/config/apply` executes
  it. Additive-only by design — apply never deletes or revokes anything absent from the file —
  and no secrets in the file: credentials and webhook URLs are referenced by *environment
  variable name* (`secret_env`) and resolved at apply time. Ships with
  [`config/sandhi.json`](config/sandhi.json) and `scripts/quickstart.sh`.
- **Bounded background persistence** (#165): the usage sink and the alert `last_fired_at` mirror
  each sit behind fixed-capacity, single-writer queues that drop and count under pressure
  instead of allocating without bound or blocking a request task, with deadline-bounded drain
  on shutdown.
- **`sandhi_streams_open` gauge** (#169): streaming response bodies currently open, exported at
  `/metrics`. TD-0014 D6 — no resource bound ships unobservable.
- **Explicit transport timeout knobs** on the typed runtime and declarative config
  (`timeout_secs`, `stream_idle_timeout_secs`; #165), alongside the existing decorator
  resilience.
- **Design record for the data plane** (#166): ADR-0006 settles the layer-boundary question
  (Sandhi stays L7; breadth at L5/L6/L7, never L3/L4) and carries the **G01–G30 gap register**
  — one owning document per gap — with ADR-0007 (embeddings not admitted; the branch formally
  parked) and TD-0014…0021, each with phase tables written as failing tests.

### Changed

- **`SANDHI_MAX_IN_FLIGHT_AI_REQUESTS` now bounds what its name says** (TD-0014 P2, #169; gap
  G02). The old tower concurrency limit released its slot when the handler future resolved —
  at *first byte* for SSE — so every simultaneously open stream (upstream connection, budget
  lease, task, decoder buffers) ran outside the limit. Admission slots are now held by the
  response **body** via a custom admission layer, released on completion or client disconnect,
  and refused calls hold no slot. **Default 64 → 128**: 64 was calibrated when a slot meant a
  handler future; it now means a minutes-long stream. A refused call's failure mode is
  unchanged (dialect-shaped 429-class error); an unpinned deployment that relied on unbounded
  streams will now shed load at the limit instead of queueing without bound — which is the
  fix, not a regression.
- **Stream decoding is bounded and linear on both planes** (TD-0014 P1, #167; gap G01). The six
  typed streaming decoders shared the raw plane's two pre-TD-0006 defects — an unbounded line
  buffer and a quadratic rescan — so a newline-free upstream stream grew a per-request `Vec`
  without bound. All decoders (and the raw sniffer) now share one `LineSplitter` with an
  amortised-compaction head offset; total work per chunk is linear in bytes, asserted through
  a work counter rather than timing.
- **A hard ceiling on a single stream line**: a line exceeding `MAX_STREAM_LINE_BYTES` (8 MiB)
  with no newline is a last-resort guard — the typed plane errors loudly
  (`ProviderError::Transport`), the raw plane drops the pending line and keeps forwarding
  (#167).
- **Multi-replica topology is refused at startup** rather than silently multiplying limits
  (#165): `SANDHI_REPLICA_COUNT != 1` asserts with the reasoning inline, until a shared ledger
  backend passes the TD-0007 conformance suite.

### Fixed

- **Long generations were silently undercounted on the default transparent plane** (#167).
  The 64 KiB sniff budget truncated OpenAI Responses' `response.completed` SSE frame — the
  frame carrying the usage, which a long generation pushes past 128 KiB — after which nothing
  parsed and the lease settled on a byte estimate. Reproduced and fixed: the raw plane now
  shares the stream-line ceiling and still yields its usage. Any deployment metering long
  generations on `/v1/responses` (and large-frame families generally) was affected.
- **A newline-less upstream stream no longer grows memory without bound or scan quadratically**
  (#167). Reachable on the cross-family translation path; operators can register arbitrary
  `base_url`s through the vault, so upstream bytes are not trusted input.
- **Ollama's NDJSON `done` frame without a trailing newline now yields `Finish`** (#169). The
  trailing remainder was dropped silently; it is now flushed through the same decode path, with
  per-chunk event ordering preserved.
- **Streaming responses no longer escape the stream-line ceiling on the translation plane**
  (#169) — the bound is pinned end-to-end on both the transparent and translation planes, after
  adversarial review demonstrated the original test left the translation twin unpinned.

### Security

- `h2` updated for [RUSTSEC-2026-0258](https://github.com/advisories/GHSA) (#163).
- The `security` (cargo-deny advisories) and `codegen-drift` CI gates actually run on the
  self-hosted runners now — both had never executed there and failed on missing toolchain
  (#168). Owner PRs route to the private runners with an owner-approved gate (#163, #160, #161).

### Housekeeping

- The gap register's open items are resolved against cited code, with factual corrections found
  by an independent fact-check (#166): catalog scope in CLAUDE.md, the G22 node-error parity
  row, and the h2 feature-unification claim (closed: `h2` is not in the non-dev graph).
- Dependency bump: `time` 0.3.55, `async-trait`, `futures-core`, `futures-util` (#159).

## [0.1.6] — 2026-08-05

## [0.1.6] — 2026-08-05

Maintenance release: placeholder-version hygiene and repo cleanup. No API, wire-contract, or
runtime behavior change — the chat contract minor is unchanged from 0.1.5.

### Changed

- Reset the committed placeholder versions from `0.1.2` to `0.0.0` across the workspace package,
  both bindings, and the internal crate-dependency requirements (#153). Versions are derived from
  the git tag at release (`cargo set-version`), per [RELEASING.md](RELEASING.md); `0.0.0` is the
  documented set-at-release placeholder, so this aligns the code with the docs and stops local/dev
  builds from reporting a misleading `0.1.2`.

### Fixed

- Reconciled `bindings/python/Cargo.lock` with the manifests (the already-declared `tracing`
  dependency) so `--locked` builds stay green (#153).

### Housekeeping

- Ignore the runtime `usage.db` SQLite artifact (`usage.db*`) so it cannot be accidentally
  committed (#154).

## [0.1.5] — 2026-08-02

The observability + meter-trust release: first-party **OTel/OTLP export** of the GenAI semantic
conventions (TD-0011 P3) completes the telemetry story atop a **hardened, drift-defended meter**
(scopes 1–4), per-key rate limiting is enforced (TD-0012), and the **agent-run cost tree** ships
(#149).

### Added

- **The agent-run cost tree is persisted and queryable** (ADR-0005 D7, #149). The proxy had
  stamped `run_id`/`step_id`/`parent_id` on every usage event since D7 landed — and the durable
  store silently dropped them at insert. The three identity columns are now persisted (additive
  migration + `idx_usage_run`; rows written before this release have NULL identity forever — the
  data was never stored, so there is nothing to backfill), and the tree is served three ways:
  `GET /admin/usage/run/{run_id}` (admin-gated `RunCostTreeV1`: per-step own spend + subtree
  rollups, orphan parents surfacing as roots, cycle-safe), `?by=run` on the existing usage
  endpoint, and `sandhi usage --run <run_id>` in the CLI. The fold is defined once in
  `sandhi-core` (`run-cost-tree.v1.schema.json`, contract minor **6**); store SQL only filters,
  pinned by test.
- **Attribution is key-authoritative, fail-loud** (ADR-0004 D4, #149). The usage event has always
  carried the *resolved virtual key's* subject/group; `x-sandhi-subject-id`/`x-sandhi-group-id`
  request headers (never previously read) are now admitted only as a byte-exact echo of the key's
  binding — anything else is a dialect-shaped **403** placed before the rate limit and the
  reservation, so a spoof holds no lease, records no spend, and emits no usage event. The admin
  bearer compare now delegates to `subtle::ConstantTimeEq` (no new lockfile crate).

### Fixed

- **Docs no longer claim rate limits are unenforced** (TD-0012 P2). Six places still said "stored,
  not enforced" after P1 shipped — README (twice), SECURITY.md's out-of-scope list, CLAUDE.md, the
  `sandhi keys share --rate` help, and the `VirtualKey::rate_limit_per_min` field doc. All corrected,
  and each now carries the caveat that matters operationally: the limiter is **per process**, so with
  N replicas the effective limit is N × the configured value. An operator reads that in `--help` at
  the moment they set the limit, not afterwards in a design doc.
- **`rate_limit_per_min` is enforced** (TD-0012 P1). It had been accepted by `sandhi vkeys share`,
  persisted, and returned by the admin API since TD-0003 — and read nowhere in the request path. An
  operator could set a limit, see it echoed back, and be told nothing when it did not apply.

  Enforcement is a per-key **token bucket** (refill `limit/60` per second, capacity `limit`), not a
  per-minute counter — a fixed window admits `2 ×` the limit across a boundary. A throttled request
  is refused **before** the budget reservation, so it consumes no lease, records no spend and emits
  no usage event; the 429 is rendered in the caller's dialect and carries **`Retry-After`**, without
  which a well-behaved SDK retries immediately and turns a throttle into a hot loop. Buckets are
  evicted after 10 idle minutes — an order of magnitude past a full refill, so eviction can never
  grant extra headroom.

  **Single-node semantics:** the limiter is in-memory, so with N replicas the effective limit is
  `N × limit` — the same limitation the enforcement ledger has, and it shares TD-0007's eventual
  shared backend rather than inventing a second story.

- **Metering-correctness hardening (scopes 1–4, #134–#140)** — the meter the observability story
  rests on was made trustworthy. `billable_parts` saturates and a `u64_at` clamp bounds the
  reservation ceiling; the DeepSeek/Anthropic prompt-cache split is read at the source and an
  observable guards `cached > prompt`; `ParsedUsage::apply` is non-destructive so a partial can no
  longer overwrite a final; Cohere stops emitting zero-valued fields. Virtual-key expiry compares
  RFC-3339 instants, not strings. The durable store gained per-connection `busy_timeout` + WAL,
  observable emit-failure counters, and a background lease-reclaim sweep. A Block-capped scope's
  unbounded output is routed through the translation plane instead of passing through
  transparently. The Node budget API is widened u32 → i64. `hash_secret` is documented as a
  high-entropy index and the dashboard warns on its exposure. #141 aligns the decision log with
  what shipped.

### Added

- **Operator guidance for telemetry** (TD-0011 P4) — README gains an "Operating it" section: log
  filtering via `SANDHI_LOG`, a Prometheus scrape config including the admin bearer the endpoint
  requires, and the four alerts worth having (capacity leaking via settle failures, enforcement
  silently off via fail-open admissions, callers being refused, and upstream latency degrading).
  Each expression was checked against a series the code actually emits, rather than written from
  memory.

- **Release verification** — `scripts/verify-release.py` plus a `verify` job that runs after the
  publish steps and checks each **registry** for the tag's version rather than trusting job status.
  The publish steps `exit 0` when their credential is absent, so a job can report success while
  shipping nothing; that is intentional for an unconfigured target (npm, pending a Node client) but
  the same guard would hide a broken publish. Expectation is derived from which secrets are
  configured, so an intentional skip is *reported* rather than silently passing. Two failure modes
  it exists to prevent, both of which already produced wrong conclusions: crates.io rejects requests
  without a `User-Agent` and returns an error that reads like "not published", and PyPI's JSON API
  lags an upload by up to a minute — so the script sends a UA and retries before reporting absence.

- **`GET /metrics`** (TD-0011 P2) — Prometheus text exposition for the gateway's own behaviour:
  calls by provider/model/dialect/**plane**/outcome, neutral token counters per kind (including the
  settled `billable` quantity), duration and TTFT histograms with buckets chosen for model calls,
  and enforcement counters for denials, fail-open admissions, lease reclaims and durable-settle
  failures. Gated exactly like the dashboard (ADR-0004 D4) — admin bearer when a token is
  configured — because traffic shape and model mix are commercially interesting.

  Two deliberate properties: the label set is a **type**, so an unbounded dimension
  (`subject_id`, `session_id`, a `vk:*` budget scope) is *unrepresentable* rather than merely
  discouraged; and the `billable` counter is handed the same quantity the ledger settled rather
  than recomputed, so a dashboard cannot disagree with a charge. The registry is hand-rolled — a
  few atomics and a text formatter — adding **no new dependency**.
- **First-party telemetry** (TD-0011 P1) — `sandhi-core`, `-providers` and `-store` now emit through
  the `tracing` facade and install **no** subscriber; the `sandhi-proxy` binary installs one
  (stderr, filtered by `SANDHI_LOG` / `RUST_LOG`). A host that links the libraries in-process
  captures Sandhi's events in its own logging with no Sandhi-side configuration and no second
  runtime, which is what the boundary is for — a compile-time test asserts the libraries cannot
  depend on `tracing-subscriber`, so a future patch cannot hijack a host's logging.

  The events are chosen for what no sidecar could compute: **plane selection**
  (transparent vs translation — the ADR-0004 adoption signal), **reservation denials**,
  **fail-open admissions** (a `Warn`-policy admit during a ledger outage previously looked like
  ordinary traffic), **lease reclaims** (the crash-recovery signal), and **durable-settle
  failures** (which leave capacity reserved until the lease expires).

  Telemetry deliberately does **not** repeat caller attribution: per-subject accounting has a
  bounded home in the usage aggregate, and a test asserts the request path logs no virtual key,
  no upstream credential, and no `subject_id`/`session_id`/`virtual_key_id`.

- **OTLP export of `gen_ai.*` spans + metrics** (TD-0011 P3, #143) — a default-off `otel-otlp`
  cargo feature pushes the OpenTelemetry GenAI semantic conventions to a collector over OTLP/HTTP
  (opentelemetry 0.32): one `gen_ai` operation span per chat call (input / output /
  cache-creation / cache-read / reasoning tokens, provider, model, **finish reason**) plus the
  `gen_ai.client.token.usage`, `gen_ai.client.operation.duration`, and
  `gen_ai.server.time_to_first_token` metrics. It layers beside `/metrics` (both can run at once).
  Only the proxy binary takes the OpenTelemetry deps — the D1 compile-time guard now forbids them
  in the library crates, and a CI guard asserts the default build's `cargo tree` stays free of
  them.

  The attribution boundary is **stricter than `/metrics`**: exported spans and metrics never carry
  `subject_id`/`group_id`/`session_id`/`virtual_key_id`/`request_id` (OTLP sends them off-process,
  past the trust boundary), and never a cost. The `gen_ai` span is built directly via the OTel
  Tracer API through a closed attribute allowlist — the `tracing_opentelemetry` bridge is
  deliberately **not** installed, since it would bridge the proxy's `tracing::` events (which carry
  `scope` = `vk:<id>`) into exported spans. A red test drives a request carrying the full
  attribution set through the dispatch→finalize chokepoint and asserts none of it leaks. #144 adds
  the operator guidance + a collector config; #145 adds `gen_ai.response.finish_reasons`.

- **Live parser-conformance harness** (#142) — `#[ignore]`d, env-gated tests that make one real
  call per provider and run the result through the *shipped* parser, asserting the counts are
  sane. The drift detector: it surfaces a provider renaming or moving a usage field before users
  see wrong bills — the complement to the captured-corpus mock tests, which pin known shapes but
  cannot detect drift.

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
