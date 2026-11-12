# Backends, logical routes and cache configuration

Status: workspace implementation candidate, 2026-11-11. These routes separate the
client protocol, upstream protocol and credential identity. Logical-model routing
and compatible affinity are implemented candidates; full fidelity qualification
and durable physical-dispatch accounting remain in progress in
[TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md).

Use the [operator console](dashboard.md) to inspect backend/route revisions and
token reuse. Define the targets explicitly, then expose only authorized aliases.

## Logical models and deterministic selection

Register explicit backends first. The following non-secret `coding-route.json`
maps one client model to two independently configured backend/model pairs:

```json
{
  "id": "coding",
  "selection": "priority",
  "affinity": "session",
  "affinity_ttl_seconds": 86400,
  "models": [{
    "id": "agent",
    "targets": [
      {"backend": "responses-primary", "model": "agent-model"},
      {"backend": "chat-secondary", "model": "local-model"}
    ]
  }]
}
```

```sh
sandhi routes set coding-route.json
sandhi routes list
sandhi routes set coding-route.json --revision 1
sandhi routes revoke coding --revision 2
```

The update example requires a changed definition at revision 1; inspect the
returned revision before revoking. Identical active registration is idempotent.
The corresponding API is `GET /admin/routes`,
`POST /admin/routes` with `{"definition":{...},"expected_revision":1}`, and
`DELETE /admin/routes/coding?revision=2`. Reads require read permission; writes
require administrator permission and cookie CSRF when applicable.

Bind a virtual key or identity profile's `upstream` to `route/coding` and authorize
`models:["agent"]`. Clients submit `agent` using any of the four supported ingress
protocols. Sandhi rewrites to the selected physical model, including Gemini's URL
path. The alias does not authorize arbitrary models or credentials. Content and
deadline policy use the actual `backend/<id>` and physical model. Discovery
returns authorized aliases with currently available targets; Gemini omits aliases
that cannot be represented by its supported model path.

`priority` selects the first eligible target; `rendezvous` selects deterministically
without depending on candidate order. Eligible targets must have current bound
credentials, declared models, compatible native state and a compilable cache plan.
The actual target codec/profile preflights encoding without starting inference.
For example, Chat `stop` excludes Responses candidates whose encoder refuses it.
A selected target's policy
denial is not a reason to route around that policy. Origin errors and ambiguous
inference are never silently replayed on another target.

Default session affinity persists a compatible binding for one day. TTL accepts
60–604800 seconds and does not renew on use. A live pin whose backend, model,
revision, credential identity, profile or cache policy becomes incompatible fails
closed. Send complete portable history in a new session to select another target;
do not reuse opaque continuation IDs or cache resources across bindings. Existing
session hints are scoped by authenticated key/subject/group and route. A client
can supply `x-sandhi-session`; it is a conversation hint, not identity authority.

Ordinary key rotation, including an externally changed secret detected at reload,
invalidates the old binding. A constrained Codex lease may retain affinity across
same-account access-token renewal; its account/profile/expiry fences still apply.
A pin proves session admission continuity, not provenance of an individual opaque
ID. `affinity:"none"` supports stateless deterministic selection and refuses
provider-bound state. Token-count calls do not establish generation pins.

The bounded registry admits 256 routes, 128 aliases per route, 16 targets per alias
and 16,384 live pins. Capacity and stale-revision conflicts refuse admission.
Existing fixed upstream names retain their meaning and cannot be shadowed.
See [ADR-0021](../adr/0021-logical-routing-and-compatible-affinity.md) for the
transaction, concurrency and compatibility rules.

**Accounting boundary:** selected-plan details stay in request memory on the
default legacy path. Already supported tracked surfaces also persist a bounded
selection/cache/policy context atomically with admission; see
[ADR-0022](../adr/0022-durable-execution-context.md). This does not expand tracked
protocol support or prove a physical send. Persisted legacy usage dimensions do
not prove the logical route's physical backend/profile. Upstream budget authority is an immutable namespace stored separately from the
provider label and endpoint family. Existing `upstream:<provider-slug>` scopes
(including native family slugs) are preserved rather than replaced by backend IDs
or new vendor labels. Inventory exposes `upstream_budget_scope` read-only. Ordinary
backend updates and restart cannot change it; see the
[identity and namespace migration](../reviews/provider-identity-and-budget-namespace-2026-11-12.md). Uniform durable dispatch evidence and recovery are
M5 work; do not infer them from the route inventory or a successful SDK response.
New supported tracked admissions also
[link and settle both budget scopes atomically](../adr/0023-paired-budget-settlement.md),
retaining unknown liability through expiry/restart.
Their [single-attempt transport evidence](../adr/0024-durable-physical-attempt-evidence.md)
now commits atomically with qualified usage. Authorization alone leaves send status
unknown. The [M5d buffered matrix](../adr/0025-qualified-buffered-protocol-accounting.md)
now supports Chat, Messages, Responses and Gemini clients/backends with strict
protocol-specific qualification. Tracked route selection checks this capability
before pinning; an existing incompatible pin refuses. Bounded transparent standard
[Responses streaming](../adr/0026-qualified-responses-stream-accounting.md) is now
qualified alongside Chat. Constrained [Codex buffered aggregation](../adr/0027-qualified-codex-buffered-accounting.md)
also qualifies through its typed owner. [Tracked translated streams](../adr/0028-qualified-translated-stream-accounting.md)
now connect four client formats to qualified Chat/Messages/Responses/Gemini/Codex sources.
[Messages streaming](../adr/0031-qualified-messages-stream-ownership.md) preserves
native state or explicitly refuses unsupported projection; signed thinking is not
portable by default. [Gemini streaming](../adr/0033-qualified-gemini-stream-ownership.md)
requires a complete final usage frame and clean HTTP EOF; interrupted Gemini
streams retain partial evidence. Native state is preserved or translation refuses.
Tracked stream selection checks delivery compatibility and
deadline configuration before confirming a pin.

[Buffered output fidelity](../adr/0034-buffered-source-evidence-and-output-fidelity.md)
keeps source token evidence independent of client translation. Compatible native
output preserves its envelope; opaque state, unrepresentable ordering and invalid
tools refuse across protocols. Such a delivery error can still settle measured
source usage. It must not trigger an inference replay to obtain another format.

## Usage qualification profiles

Cohere v2 and Ollama are supported backend families behind the four client
dialects. Buffered and bounded streaming delivery use qualified source counts:
Cohere `usage.tokens` is distinct from `billed_units`; Ollama cache-read input,
when reported, must be a valid subset of prompt evaluation input. Missing counts
remain unavailable. Invalid or incomplete evidence retains both budget holds and
does not trigger another inference. See the
[integration evidence](../reviews/stable-namespaces-native-sources-and-gemini-images-2026-11-12.md)
for the tested scope and remaining release gates.

Chat-compatible endpoints that place request-total stream usage on their single
final choice can explicitly declare `endpoint_profile: openai_terminal_chunk_usage`.
Standard Chat retains its empty-choices usage contract. This profile accepts only
known terminal reasons with validated totals; it does not accept progress usage,
infer counts from a finish reason, alter cache policy or enable retries. It works
on transparent and translated tracked streams and survives restart. Changing the
profile invalidates an established route binding instead of switching it silently.
See [the placement contract](../adr/0029-explicit-terminal-chat-usage-profile.md).

```bash
sandhi backends set coding-chat --provider coding-models --credential-ref coding:default \
  --endpoint-family openai_chat_completions --endpoint-profile openai_terminal_chunk_usage \
  --base-url https://api.z.ai/api/coding/paas/v4 --model glm-5.3
```

Compatible Messages endpoints can declare `endpoint_profile: anthropic_implicit_cache`
when their input counter includes implicit cache construction and they meter no
separate write category. For example, the scoped Z.AI qualification requires this
profile because that endpoint omits `cache_creation_input_tokens`:

```sh
sandhi backends set zai-messages --provider zai --credential-ref zai:live \
  --endpoint-family anthropic_messages --endpoint-profile anthropic_implicit_cache \
  --base-url https://api.z.ai/api/anthropic --model glm-5.3
```

The credential reference must already exist. Configure request cache controls
separately through `--cache-policy`; `implicit_only` there does not grant this
usage convention. Standard Messages accounting remains strict. Profiles are
revisioned/pinned and survive restart; choose a new materialized session after a
profile change. See [qualified usage semantics](../adr/0025-qualified-buffered-protocol-accounting.md#qualified-token-semantics).

## Cache declarations

Backend definitions accept `cache_policy`; CLI registration accepts
`--cache-policy path/to/policy.json`. This is non-secret JSON, bounded to 64 KiB.
For a qualified Anthropic endpoint and an exact declared model:

```json
{
  "default_intent": {"mode": "auto_stable_prefix", "lifetime": "1h"},
  "allow_request_override": true,
  "namespace": "provider_default",
  "models": {
    "agent-model": {
      "profile": "anthropic_breakpoints",
      "lifetimes": ["5m", "1h"],
      "max_breakpoints": 4
    }
  }
}
```

The model map must exactly match the backend's declared model IDs. These are
operator capability assertions, not proof that a compatible server implements a
Platform feature. Use `native_only` until qualified. A present policy without a
default selects `preserve_native`; only an absent policy and absent request intent
retain the legacy Anthropic automatic-placement/empty-extras-bucket convention.

All four HTTP ingress protocols accept the reserved top-level directive
`"sandhi_cache":{"mode":"auto","lifetime":"1h"}`. It is inspected under policy,
consumed by the gateway and removed before upstream dispatch. Direct neutral/FFI
requests use `cache_intent`. Available modes are `preserve_native`, `auto`,
`auto_stable_prefix`, `auto_conversation_prefix` and `suppress_gateway`. Suppression
adds no gateway markers and preserves native controls; provider-internal caching
can still occur. A fixed policy (`allow_request_override:false`) accepts its own
intent and refuses a different override. Unsupported requirements fail before
reservation or dispatch. Native controls win over automatic placement; conflicting
lifetimes fail rather than moving an existing boundary.

| Profile | Automatic mechanism and declared lifetimes |
| --- | --- |
| `native_only` | No synthesized controls; automatic intent refuses. |
| `implicit_only` | `auto` adds no controls; explicit lifetimes/boundaries refuse. |
| `anthropic_breakpoints` | Stable or conversation block marker; `5m`, `1h`. |
| `openai_legacy` | `auto` can emit retention; `in_memory`, `24h`; precise boundaries refuse. |
| `openai_explicit` | Automatic/conversation implicit options or eligible stable text marker; `30m`, `12h`. |

Declare a one-to-four `max_breakpoints` budget only for Anthropic/OpenAI explicit
synthesis. The compiler's conservative automatic budget does not redefine native
API lookup limits. Codex permits only `native_only`; generic Responses declarations
do not relax its subscription fences. Gemini cache resource creation, expiration
and deletion remain separate lifecycle work; no generated resource reference is
implied by `auto`.

`namespace:"scoped_key"` is available only for declared OpenAI legacy/explicit
profiles. Version 2 hashes authenticated scope, backend revision, credential
generation, a private identity of the actual bound credential, model, policy and
bounded native key/session hint. Raw identity/key hints are not
forwarded as namespace values. Registered rotation/restart tests verify namespace
changes and stability. The registered generation does not observe unreported
external changes, but reconstructed handles fence changed actual credentials;
old compiled plans and namespaces cannot be reused after such a reload.
`provider_default` makes no tenant-isolation guarantee. Neither option guarantees
a provider cache hit, transfers KV state or proves cache usage from latency.
See [ADR-0020](../adr/0020-cache-intent-compilation.md) for implementation boundaries.

Set `SANDHI_STORE` and configure a vault. First provision/register an exact
credential such as `openai:production` using the existing credential API or
`POST /admin/keys/reference` for a supported read-only vault. The credential's
declared base URL determines the allowed origin. Never put secret material in a
backend definition.

## CLI and keys

For an existing credential whose origin is `https://models.example.com`:

~~~sh
sandhi backends set responses-primary \
  --provider private-models \
  --credential-ref openai:production \
  --endpoint-family openai_responses \
  --base-url https://models.example.com/v1 \
  --model agent-model

sandhi backends list
~~~

Use the ordinary admin URL/token configuration. `set` prints the persisted record,
revision, `upstream_ref` and `ready`. Bind virtual keys or gateway profiles to
`backend/responses-primary`; existing `provider:label` routes still work. A key's
model allowlist and the backend's declared models both apply. Discovery exposes
their intersection. The provider label is descriptive and does not choose the
protocol when the endpoint is explicit.

Select `openai_chat_completions` for compatible Chat/open-weight servers,
`anthropic_messages` for Messages, or `openai_responses` for standard Responses.
Implemented Gemini, Cohere and Ollama families also remain available. A Codex
binding requires `--endpoint-family openai_responses --endpoint-profile chatgpt_codex`
and an existing OAuth lease credential; it retains the fixed Codex URL, account,
access-only envelope, expiry and constrained translation rules. Generic OAuth or
an Anthropic subscription lifecycle is not implemented by choosing a family.

Changes require `--revision <current-revision>`. An identical active `set` replay
without a revision is idempotent; a different definition fails with HTTP 409.
Revocation also requires the current revision:

~~~sh
sandhi backends revoke responses-primary --revision 1
~~~

Revocation advances the revision and disables the backend without revoking its
credential or other backends. Reactivation uses `set --revision` with the revoked
record's revision. Credential revocation disables every dependent backend.
Credential re-registration/rotation rebuilds dependents with the new credential.

## Config

The existing config file accepts additive `backends` and `routes` arrays:

~~~json
{
  "backends": [{
    "id": "responses-primary",
    "provider": "private-models",
    "credential_ref": "openai:production",
    "endpoint": {
      "endpoint_family": "openai_responses",
      "endpoint_profile": "standard"
    },
    "base_url": "https://models.example.com/v1",
    "models": [{"id": "agent-model", "capabilities": {"tools": true}}]
  }],
  "vkeys": [{
    "upstream": "backend/responses-primary",
    "subject": "agent",
    "models": ["agent-model"]
  }]
}
~~~

Preview with `GET /admin/config` and apply with `POST /admin/config/apply` through
the existing CLI/admin workflow. A credential may already be registered or be
declared in the same file's `providers` array. Provider apply writes credentials;
for a read-only vault, provision externally and use reference registration first,
then omit provider write intent.

All backend intent is checked before credential writes. Apply uses revision
snapshots and applies providers before backends, routes and dependent keys/profiles.
Route definitions use the same shape as `coding-route.json` above. All route
references are preflighted against planned or active persisted backends before
credential writes. A failed dependency suppresses downstream grants.
Component failures are reported; committed items are not rolled back. Inspect the
results before retrying an ambiguous failure. Omission is additive and does not
revoke existing entries. Startup reconstructs previously applied definitions from
the store; it does not implicitly apply the file.

Model IDs are enforced. Capability facts are declarations, with full feature
negotiation still pending. Backend-local model aliases, per-model endpoint
overrides and extensions are rejected; aliases belong in logical routes above.
The backend registry holds at most 256 definitions
(including revoked records), each at most 64 KiB and 128 models.

## API and readiness

- `GET /admin/backends`: requires operator read access; lists non-secret records.
- `POST /admin/backends`: requires admin access; body is
  `{"definition": {...}, "expected_revision": 1}`. Omit the revision for creation.
- `DELETE /admin/backends/<id>?revision=1`: requires admin access.

`ready: true` means that the listed active definition has a matching loaded
handle/revision. It does not contact a model or prove upstream availability.
Missing, locked, invalid or expired credentials can leave active metadata dormant.
Invalid durable metadata stops startup. No alternate account is selected and no
inference is replayed to recover a missing route.

The URL must be HTTP(S), without user information, query or fragment, and share
the credential's declared origin. Paths may differ; Codex applies its stricter
fixed URL. Configure a separate credential reference for another origin.

Synthetic Rust and real-process SDK tests cover persistence, rotation, revocation,
model scoping and CLI/config behavior. They do not qualify live provider caches or
subscription entitlement.
