# ADR-0020: Cache intent compilation by backend and model

Date: 2026-11-11

## Status

Implemented workspace candidate for TD-0034 M3 (contract minor 13). The compiler,
backend declarations, request directive, both forwarding planes and namespace
generation are implemented and covered by synthetic tests. ADR-0013 applies only
when both policy and intent are absent. M4 target/account selection and M5 durable
execution attribution remain unfinished; an in-memory cache plan is not durable
dispatch evidence. Provider qualification is recorded separately.

## Problem

The Anthropic encoder chooses automatic placement by the presence of its entire
extras bucket. An unrelated native option therefore suppresses caching, while an
empty bucket doubles as a documented opt-out. Cache controls from another ingress
can also disappear during translation. Protocol compatibility alone does not prove
that an OpenAI-shaped server implements a particular cache control or token split.

Provider APIs now expose different mechanisms. OpenAI documents model-dependent
explicit breakpoints and request options as well as older retention controls.
Anthropic supports block breakpoints and request-level automatic caching. Gemini
generateContent has implicit caching and separately managed cached-content
resources on supported models. These facts motivate capability-specific
compilation; they do not establish support on Codex or a third-party server.
Sources: [OpenAI](https://developers.openai.com/api/docs/guides/prompt-caching),
[Anthropic](https://platform.claude.com/docs/en/build-with-claude/prompt-caching),
[Gemini generateContent](https://aistudio.google.com/docs/caching?generate-content=true).

## Decision

### Separate policy, intent, native controls and observations

The backend definition declares an optional cache policy and capabilities for its
exact model IDs. Absence preserves legacy behavior. Config, admin registration,
revision CAS, credential reconstruction and restart must all carry the declaration.
Do not infer a cache feature from provider slug, model-name prefix or credential
scheme. A declaration is an operator assertion; evidence separately records
synthetic wire validation and live qualification.

The neutral request gains optional cache intent. Native HTTP clients can supply
the same intent through a gateway-reserved field supported by all four ingress
codecs. Consume it before upstream forwarding, inspect original bytes under policy,
and reject malformed/unknown directives. Model, backend, credential and namespace
authority remain server-owned. Existing native cache fields continue to mean their
native semantics rather than being reinterpreted as a portable intent.

The compiled plan freezes mode, capability revision, requested and selected
lifetime, control placement, namespace policy and transformation requirement.
Observations remain independent: a requested cache write is not a measured write;
an absent read field is not reported zero; a fast request is not proof of a hit.

### Modes and precedence

| Mode | Meaning |
| --- | --- |
| Legacy, only when policy/intent is absent | Preserve ADR-0013 defaults, including the empty Anthropic-bucket opt-out. |
| Preserve native | Retain exact compatible native controls and boundaries; add no gateway placement. Reject nonportable required controls on another target. |
| Automatic | Choose the declared profile's automatic mechanism; implicit-only profiles add no wire controls. |
| Automatic stable prefix | Add controls only at the declared eligible tools/instructions boundary. Unrelated extras do not disable this explicit selection. |
| Automatic conversation prefix | Add controls at the declared eligible growing-history boundary. Preserve source order and avoid moving a boundary into opaque or unsupported blocks. |
| Suppress gateway placement | Add no gateway cache controls. Preserve explicit native controls; do not promise to disable provider-internal caching. |

An explicit request directive overrides an allowed backend default. Native explicit
boundaries remain authoritative: automatic policy does not silently replace or
move them. Contradictory lifetime or placement requirements fail before reservation
and dispatch. Required unsupported intent fails; any future best-effort downgrade
must be separately explicit and visible in the compiled plan. No implicit downgrade
is introduced in the first implementation. A present backend policy with no default
selects `preserve_native`, including a `native_only` declaration. A fixed policy
accepts an identical request intent and rejects a different override.

The old empty-bucket signal remains meaningful only in legacy mode. A separately
selected automatic policy is not cancelled by unrelated extras or an empty bucket;
use `suppress_gateway` for the explicit opt-out. The operator guide includes
migration examples and the integration suite covers both cases.

### Capability profiles and wire compilation

| Declared capability | Compiler behavior |
| --- | --- |
| Native-only/unqualified | Preserve compatible native controls. Reject requested synthesis. |
| Anthropic block breakpoints | Mark the furthest eligible system/tool block for stable-prefix mode, or the last eligible conversation block for automatic/conversation mode. Keep native markers; enforce declared lifetime, marker budget and long-before-short ordering. |
| OpenAI legacy prefix caching | A scoped cache key and an explicitly declared supported retention value may be emitted. Stable/conversation placement requirements that the endpoint cannot express fail. |
| OpenAI explicit prefix caching | Automatic/conversation mode uses implicit request options. Stable mode marks the last eligible initial system/developer text block. Top-level instructions and tool definitions cannot receive an invented marker. Preserve native ordering and caller boundaries. |
| Implicit caching only | Do not invent a cache-write request, breakpoint or lifetime. Suppression/preservation can be no-ops; required explicit placement or lifetime fails. |
| Gemini cached-content reference | Preserve a compatible authorized resource reference. Creating, expiring, deleting or migrating a cache object is a separate resource lifecycle, not a synthesized inference field. |

Keep lifetime values native to the capability profile. Anthropic `1h`, OpenAI
`12h`/older retention values and Gemini resource TTLs are not interchangeable.
Capabilities can restrict a documented API feature per model or deployment.
The first automatic compiler uses a conservative declared budget of one to four
controls (implicit OpenAI placement consumes one). This is a local synthesis
limit, not a claim that OpenAI rejects every native request with more than four
lookup boundaries. Preserve-native mode does not rewrite that native policy.
Codex remains native-only until its constrained endpoint is independently qualified;
Platform documentation never overrides its fixed endpoint, access-only credential,
account header, dispatch-expiry fence or rejected request fields.

### Forwarding plane and native snapshots

Compile after authorized model/backend selection and native-input validation,
before rate/budget admission. A no-op native plan retains the transparent plane.
An executable mutation must also affect raw requests; either apply a bounded,
target-specific native patch or force a loss-checked typed encode. Do not compute
a plan that only one plane executes. Text/embedding routes reject chat-only intent.

Do not mutate codec-owned history behind its digest. Encoders restore ordered
input, apply typed model/limit authority and then apply the compiled cache patch.
Only validated native locations may change; unknown input and signatures stay
intact. Portable projections without a suitable target boundary fail explicitly.
An output cap forcing typed execution must not change a preserved cache plan.

### Namespace, affinity and account identity

Conversation IDs, reusable prefixes, cache namespaces and backend affinity are
different values. A generated key hashes a versioned tuple of authenticated scope,
backend identity/revision, credential/account generation, private actual-credential
identity, actual model, capability
revision and the allowed caller hint/session. Do not forward raw subject, group,
virtual-key or credential identifiers as cache keys. Delimiter-safe serialization
and bounded inputs prevent collisions and resource growth.

When scoped keys are enabled, wrap client-supplied keys on both forwarding planes;
never accept one as authority for another tenant's namespace. Stable related turns
retain the same tuple; a different tenant, target, model or incompatible account
generation cannot reuse it. Metadata registration durably advances a positive
generation, including replacement at the same reference and re-registration after
revocation; overflow refuses registration. Unreported external vault changes are
not observed by this counter. Version 2 scoped keys and compiled-plan bindings also
include a private identity derived from the actual reconstructed credential and
endpoint/header configuration. A changed external secret therefore fences old
plans/namespaces after reload without exporting the secret or its standalone
digest. Existing version 1 generated keys change once on this candidate upgrade.
The constrained Codex account-fenced renewal rule is defined in
[ADR-0021](0021-logical-routing-and-compatible-affinity.md); scoped cache generation
remains conservative across registration. A reference string alone is insufficient.

Some APIs provide no caller-controlled cache namespace. A hash or affinity header
cannot manufacture isolation there. A strict isolation requirement needs a
qualified dedicated account/resource boundary or must fail. Legacy provider cache
sharing is not silently advertised as tenant-isolated by this refactor. Gemini
resource references and opaque continuation state need M4's compatibility rules;
neither an ingress clone nor a copied cache identifier authorizes transfer.

## Validation and remaining qualification

`cache_policy.rs` integration tests cover 32 ingress/backend/mode combinations,
pre-budget refusal, native precedence, exact TTLs, no-op declarations, namespace
changes, forced typed output caps and Codex refusal. SDK tests exercise all four
clients, policy-file CLI registration, config, restart and credential rotation.
The generated contract accepts `cache_intent`; `compiled_cache` is serialization-
skipped and cannot be imported from JSON. A plan is bound to the original input
and selected backend/generation; output-cap injection remains allowed.

The following gates also govern the remaining broader routing/accounting work:

1. Red/green tests reproduce unrelated-extras suppression and cross-family loss of
   required native cache controls; refusals hold no reservation and send nothing.
2. Every ingress and both delivery modes exercise preserve, explicit suppression,
   stable/conversation defaults and per-request selection. Explicit markers and
   lifetime fields survive; contradictory/unsupported directives fail consistently.
3. Config/admin/CLI/restart agree on capabilities and revision changes. Missing
   declarations preserve legacy behavior; unknown settings cannot be inert success.
4. Namespace tests vary authenticated scope, hint, prefix/suffix, target, account
   generation and model. Identical client hints do not collapse different tenants.
5. Wire, canonical usage, persisted evidence and both budget legs agree on disjoint
   counts. Malformed/missing/zero read/write evidence remains distinguishable.
6. SDK tool loops, cancellation, held HTTP EOF, restart and recovery demonstrate no
   duplicated inference or settlement. Capture M5's attribution and unresolved
   liability rather than extending the limited tracked profile by assertion.
7. Live minimum-prefix, TTL, warm/cold and resource-lifecycle cases remain marked
   unqualified until run with explicitly authorized endpoints and synthetic prompts.

## References

- [Active goal and constraints](../td/TD-0034-protocol-neutral-router-cache-accounting.md)
- [Legacy cache behavior](0013-native-extensions-passthrough-and-auto-cache-control.md)
- [Backend lifecycle](0017-backend-registry-lifecycle.md)
