# TD-0010: Ingress dialect parity — drop-in compatibility as a release gate

- **Status:** Proposed (2026-07-25). **P1 complete** (D1 in #84, D5 in the SDK-conformance
  suite); **P2, P3 and P4a complete** (errors; discovery; Gemini on the transparent plane); **D2 complete** — the
  401/403 paths now render per dialect and name each vendor's own scheme; this document was corrected
  during that implementation — see the error-shape bullet and D2.
- **Relates to:** ADR-0004 (two-plane proxy), TD-0006 (transparent metering), TD-0002 (typed
  runtime), ADR-0001 (wire contract), TD-0003 (operator surface: the model allowlist)
- **Companion changes:** #61 (transparent plane), #77 (error redaction — this TD must compose
  with it, not undo it)

## Why this exists

Sandhi's entire adoption story is one sentence from the README: *internal clients point their
`base_url` at Sandhi with a virtual key and never see the real key.* If that requires editing
client code, the product is a library with a proxy attached rather than a gateway.

Measured against `develop` at `6c8bcca`:

| client | drop-in? | evidence |
|---|---|---|
| **OpenAI SDK** → `/v1/chat/completions`, `/v1/responses` | ✅ | SDK sends `Authorization: Bearer`, which is what `bearer()` reads |
| **Anthropic SDK** → `/v1/messages` | ❌ **401** | the SDK authenticates with `x-api-key`; the proxy never reads that header |
| **Gemini SDK** | ❌ | no ingress dialect exists — Gemini is reachable only as an *upstream* |

`bearer()` (`sandhi-proxy/src/lib.rs`) is the single credential path for every dialect, and
`x-api-key` appears in this repo only *outbound* to Anthropic (`providers/src/anthropic.rs`,
`raw.rs`). The proxy's own Anthropic ingress test posts to `/v1/messages` with
`authorization: Bearer vk_demo` — so the Anthropic path was built and verified against a client
that authenticates the OpenAI way, which no stock Anthropic SDK does. **The test suite passes
precisely because it never points a real SDK at the proxy.**

Two more surfaces are missing for the same reason:

- **No model discovery.** There is no `GET /v1/models`. SDK `.models.list()`, LangChain, LiteLLM
  health checks and most chat UIs call it. `/catalog/models` exists but is a Sandhi-shaped
  endpoint at a Sandhi-shaped path.
- **Errors are dialect-shaped *after* the credential check, and flat before it.**
  `ingress_error()` already renders per dialect (`{"error":{…}}` for OpenAI/Responses,
  `{"type":"error","error":{…}}` for Anthropic) — an earlier draft of this TD claimed otherwise
  and was wrong. What is still flat is `error()`, used by the pre-dialect failures: the auth
  paths (`lib.rs` 522/527/530) and several admin/dashboard rejections. So the *first* response a
  misconfigured client sees is the one least likely to be parseable by its SDK, and its text —
  `"missing bearer virtual key"` — now gives an Anthropic user actively wrong advice, since
  `x-api-key` is that dialect's native scheme (D1).

## First principles

1. **A dialect is the whole client-facing contract**, not just a body schema: path shape,
   credential presentation, request body, stream framing, error envelope, and discovery.
2. Sandhi's `IngressDialect` today owns **two** of those six (path, body). The other four were
   implemented once, in the OpenAI way, and shared by everyone — which is why the Anthropic path
   is broken in exactly the places OpenAI and Anthropic differ.
3. **Compatibility is a property you test against the real client, or you do not have it.**
   Hand-rolled requests encode the author's assumptions; an SDK encodes the vendor's.
4. A gateway may translate *between* dialects (ADR-0004 plane 2), but it must never require the
   client to speak a dialect its vendor's SDK does not.

## Non-goals

- Non-chat vendor surfaces (embeddings, files, batches, assistants). Chat + discovery is the
  adoption surface; the rest is scope creep until asked for.
- Changing how Sandhi authenticates **upstream** — that already works per family.
- Pricing/cost fields in any new endpoint (ADR-0001; the boundary holds here too).

## Decisions

**D1 — The dialect owns credential extraction.** Replace the single `bearer()` call with
`IngressDialect::extract_credential(&HeaderMap, &Uri)`:

| dialect | accepts |
|---|---|
| OpenAI / Responses | `Authorization: Bearer <vk>` |
| Anthropic | `x-api-key: <vk>` **and** `Authorization: Bearer <vk>` |
| Gemini | `x-goog-api-key: <vk>` **and** `?key=<vk>` |

Accepting Bearer everywhere as a secondary form costs nothing and helps curl users and proxies
that only forward `Authorization`.

**D2 — Extend dialect-shaped errors to the pre-dialect paths, and keep redaction owning the
content.** Scope corrected after implementation: body-level errors already render per dialect via
`ingress_error()`, so D2 is *narrower* than first written — route the auth/admin rejections
through the same renderer and make the message dialect-accurate (an Anthropic client should be
told about `x-api-key`, not "bearer"). Content is unchanged from #77 (redacted by default,
`SANDHI_ERROR_DETAIL=full` opts in). Shape is compatibility; content is confidentiality. They are
independent, and this TD must not quietly widen the second.

> **Resolved while implementing P4a:** there was no ordering problem at the `handle()` call
> sites — the dialect is already a parameter there, so the auth failures simply had not been
> routed through the dialect renderer.
>
> **D2 closed, narrower than first written.** Every remaining flat `error()` turned out to be on
> Sandhi's **own operator surface** — `/catalog/models` and `/dashboard/api/*`, which share the
> flat `{"error": "<string>"}` contract with `/admin/*` via `operator::err` and are consumed by
> the `sandhi` CLI, not by any vendor SDK. Rendering those "in a dialect" is meaningless: there
> is no client dialect to render. Converting them would be churn against a working contract with
> real CLI-compatibility risk. **D2 therefore covers the client-facing paths only**, and those
> are done: auth rejections, missing-upstream, and both transparent-plane failures now render in
> the caller's dialect. If the operator API's envelope is ever revisited, that is its own
> decision about Sandhi's admin contract — not part of vendor parity.

**D3 — The dialect owns discovery, filtered by the virtual key.** `GET /v1/models` (OpenAI
shape), the Anthropic equivalent, and Gemini's `ListModels`, all sourced from the existing
transport catalog **intersected with that virtual key's model allowlist** (TD-0003 P4). A key
that may call two models lists two models — more honest than a static catalog, and it makes the
allowlist discoverable instead of a surprise 403 at call time.

**D4 — Gemini becomes a first-class ingress dialect.** `/v1beta/models/{model}:generateContent`
and `:streamGenerateContent`, including its non-SSE streaming framing. This is the largest piece
and depends on D1–D3 existing as a trait rather than as three copies of an `if`.

**D5 — Conformance is proven with the vendors' own SDKs.** A CI suite that starts the proxy, mocks
the upstream, and drives it with **openai-python, anthropic-python, and google-genai** — no
hand-rolled requests. Acceptance for a dialect is "the vendor SDK works unmodified", which is the
only statement the README's promise can be checked against. This suite is what would have caught
the `x-api-key` defect on day one.

**D6 — The compatibility matrix is checked in and gated.** The table at the top of this TD lives
in the README as the public claim and in CI as a test. A dialect that is not drop-in is not
advertised as one — no aspirational rows.

## Phases

| Phase | Scope | Acceptance |
|---|---|---|
| **P1** ✅ | D1 credential extraction per dialect + the real-SDK conformance suite for OpenAI and Anthropic | **Met.** `tests/sdk-conformance/` starts the real proxy against a mock upstream and drives `openai-python` + `anthropic-python` unmodified; verified to fail (`401 missing bearer virtual key`) when Anthropic's scheme list is reverted to Bearer-only |
| **P2** ✅ | D2 dialect-shaped errors on the **client-facing** paths | **Met.** Auth rejections, missing-upstream and transparent-plane failures render per dialect (OpenAI `{error:{…}}`, Anthropic `{type:"error",…}`, Gemini numeric `code` + canonical `status`); redaction from #77 unchanged. Sandhi's operator API keeps its flat envelope deliberately — see the note under D2 |
| **P3** ✅ | D3 discovery endpoints, allowlist-filtered | **Met.** `client.models.list()` works unmodified on all three SDKs; a scoped key lists exactly its allowlist, an unscoped key gets the upstream catalog, and discovery is authenticated because it reveals what a credential may call |
| **P4a** ✅ | Gemini ingress on the **transparent plane only**, `x-goog-api-key` header auth | **Met.** `google-genai` completes streaming + non-streaming calls unmodified; `?key=` is refused; cross-family is refused rather than translated from the accounting-grade decode |
| **P4b** | Cross-family translation for Gemini ingress (a faithful Gemini ↔ `ChatRequestV1` codec) | A Gemini client resolving to a non-Gemini upstream round-trips tools, inline media and safety settings without loss |

P1 is the adoption unblocker and should land alone. P4 is the largest and is deliberately last —
it is also the phase that proves the trait from D1–D3 was the right shape, because adding a
dialect should be filling in an implementation rather than touching every handler.

## Pressure test

1. **"Accepting `?key=` puts a credential in the URL."** True, and it is Gemini's documented
   scheme, so refusing it means refusing the SDK. Mitigation: prefer the header when both are
   present, and never log the query string (an access log or a crash report would otherwise
   capture a live virtual key). If that guarantee cannot be made end-to-end, D4 should ship
   header-only and accept partial Gemini compatibility — a deliberate, stated gap beats a leaked
   key.
2. **"Dialect-shaped errors will leak upstream detail."** Only if D2 is implemented as
   *pass-through* rather than *re-render*. The envelope is constructed by Sandhi from its own
   redacted error; the upstream body enters only under `SANDHI_ERROR_DETAIL=full`, exactly as
   today. The test in P2 exists to keep that honest.
3. **"Discovery invites clients to believe every listed model is available."** That is precisely
   why D3 filters by the key's allowlist rather than serving the catalog wholesale.
4. **"Six surfaces × N dialects is combinatorial."** It is 6 × 4 with defaults, expressed as a
   trait — versus today's implicit 6 × 1 that silently mis-serves three of the four. The
   combinatorial risk is the argument *for* the trait, not against the parity.
5. **"Real-SDK tests are heavy and flaky in CI."** They run against a local axum server with a
   mocked upstream — no network, no vendor credentials. The cost is three dev dependencies in a
   test-only harness; the benefit is the only test that can falsify the README.
6. **"Anthropic's `anthropic-version` header is ignored at ingress."** Tolerated: Sandhi
   constructs its own upstream request and sets the version there. Revisit if Anthropic ever
   makes ingress semantics depend on it — noted here so the next reader does not rediscover it
   as a bug.

## Open questions

- Does `/v1/messages` need to honour Anthropic's `anthropic-beta` headers for features Sandhi
  passes through on the transparent plane (#61), or does the byte-exact path already carry them?
  Measure before deciding.
- Should an unknown-but-well-formed dialect path (e.g. `/v1/embeddings`) return a dialect-shaped
  `not_implemented` rather than a bare 404, so clients fail with a readable vendor-native error?
- Is there a case for a `/v1/models` entry that advertises *Sandhi's* virtual model routing
  (policy-selected model) once TD-0005 lands, or does that break the drop-in illusion?
