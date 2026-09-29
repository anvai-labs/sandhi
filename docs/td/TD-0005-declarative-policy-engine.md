# TD-0005: Declarative policy and bounded evaluator extensions

Status: In progress — local policy MVP implemented as a feature candidate; broader contract below remains the target
Updated: 2026-09-27
Depends on: ADR-0004 (enforcement boundary), ADR-0005 (atomic budget leases),
TD-0003 (operator surface), TD-0029 (verified identity), TD-0008 (Victor boundary).

This revision replaces the earlier first-match-wins sketch. It retains a pure
decision engine, optional signed distribution and neutral token accounting, and
adds identity-scoped content inspection. The existing proxy already enforces
authentication, grants, model allowlists, rate limits and durable budget leases.
The opt-in [implemented MVP](../operator/policy-evaluation.md) adds strict JSON
configuration, regex/threshold/lexical evaluation, verified identity selection,
bounded admission and mandatory metadata receipts. Quarantine holds dispatch
without storing the payload. Its actual generated PolicyDocumentV1 contract is
narrower than the target contracts below. General semantic models, encrypted
payload review/release, signed bundles, activation epochs, outcome joins and
hierarchical caps remain future work. Existing alerts are best effort.

The [Python NLP/MLflow extension design](../operator/python-ml-evaluators.md)
keeps supervised model inference separate from offline experiments and promotion.
Supervised Python workers, a backend-neutral score contract and offline MLflow export
are now implemented candidates. The [embedded ONNX profile](../operator/embedded-onnx.md)
also has native parity/cancellation and live gateway acceptance. A
[multi-model Python service template](../../templates/python-evaluator/README.md) and
Sandhi-owned HTTP replica adapter now have TLS/mTLS, IPC/HTTP parity and failure tests.
Local evaluators run before remote text inspection; a local block, quarantine or
incomplete check prevents remote disclosure. Remote denials stop additional remote
inspection. Each service owns one listener with named model routes and bounded child
pools. HAProxy/platform ingress is optional; a shared gateway ledger remains separate. The [container profile](../../templates/python-evaluator/DEPLOYMENT.md) now provides
reviewable deployment snapshots and locally tested Linux/cgroup-v2 CPU, memory and PID
limits. Internal-only and published-port networks are explicit choices; published
mode requires operator-owned egress filtering. This remains trusted-model execution.
The broader
request ordering and interfaces below remain
requirements for later phases, not claims about current runtime behavior.

## Ownership and admission order

Sandhi is the authoritative enforcement point when it holds the provider
credential. Victor can use the same contract for local previews, but a client
preview, client-supplied score or client-supplied policy cannot authorize egress.
Kanidm owns directory membership. Sandhi maps verified identities to grants and
policy bindings; it does not become another password/group directory.

The proposed request path is:

1. Bound and authenticate the request; reject ambiguous credentials and forged
   attribution. Resolve the current issuer/subject, grant and destination.
2. Apply existing model/route restrictions and admission rate/concurrency limits.
3. Pin an immutable policy revision and evaluator manifest; select all applicable
   policies using trusted identity attributes. Extract a bounded inspection view.
4. Run required evaluators under one total monotonic deadline, including queue
   time and evidence admission. Reduce results to a decision and obligations.
5. Durably admit required audit/quarantine evidence. If evidence is mandatory,
   failure to persist prevents dispatch. Commit budget reservations atomically
   against current counters; an earlier snapshot is not authorization to spend.
6. Recheck revocation and the policy activation epoch immediately before dispatch;
   abort if either invalidated admission. Forward only an admitted request.
7. Meter/settle the provider attempt and append its outcome using the same request
   ID. Never replay an ambiguous provider attempt after a policy or sink failure.

An admission epoch change during evaluation rejects that attempt with a bounded
retryable pre-dispatch error; it does not restart evaluation indefinitely. An
already dispatched request cannot be recalled. This defines the revocation race
boundary explicitly. No provider byte leaves while content admission is pending.

## Versioned data contract

Rust types in `sandhi-core` will be authoritative. Export JSON Schema and language
facades using the existing codegen pipeline. Do not hand-maintain a competing
schema or introduce HTTP into core. JSON is canonical. A YAML/TOML/UI frontend may
compile to exactly that contract, rejecting duplicate keys, unknown critical
fields, unsafe tags/includes, unbounded expansion and unsupported versions.

The public contract consists of:

| Type | Required responsibilities |
| --- | --- |
| `PolicyBundleV1` | Version, ID, monotonic revision, issuer/deployment scope, issue/expiry times, evaluator manifests, bindings, rules, limits and evidence obligations |
| `PolicyIdentityV1` | Verified issuer + subject, principal kind, credential public ID/type, selected grant, verified groups/roles and their source/freshness; never raw credentials |
| `EvaluationRequestV1` | Random evaluation/request IDs, policy/evaluator digests, input coverage, bounded content segments and trusted request facts, remaining time budget |
| `EvaluationResultV1` | Matching IDs/digests, status, typed finite scores/findings, input coverage, safe reason codes and bounded evidence references |
| `PolicyDecisionV1` | Disposition, audit/evidence obligations, safe reasons, evaluated revision, elapsed time, constraints/reservations and opaque receipt ID |

A result status is `complete`, `unsupported`, `timeout` or `error`. `complete`
with no findings is distinguishable from unavailable/incomplete inspection.
Unknown statuses, NaN/infinite scores, oversized output, wrong IDs/digests,
missing coverage and malformed findings are evaluator errors, never a clean pass.
The gateway measures elapsed time; plugin-reported timings are supplementary.

Evaluators return findings, not authorization. The pure reducer maps findings to
policy effects. A plugin cannot invent a role, choose an upstream, mint a grant,
disable a budget, edit a request or directly call the final LLM provider.

## Identity bindings and composition

Policies may bind to issuer-qualified users/workloads, verified directory groups,
gateway roles, credential IDs, grants, destinations and explicit deployment-wide
baselines. Across distinct selector fields, predicates are AND; values within one
field are OR. Names are exact matches unless a schema field explicitly requests
another matching mode. Model prefixes must never be inferred from literal `*`.

Evaluate **all** matching policies. No user-specific rule or higher role cancels
a deployment/group restriction. There is no order-sensitive first-match bypass:

- Existing authentication and grants establish the maximum authority. Policy can
  narrow it; an evaluator's successful result cannot grant missing permission.
- Disposition combines as `block > quarantine > forward`.
- Audit and evidence obligations combine independently and survive a block.
  Contradictory obligations reject bundle activation or fail closed at runtime.
- Intersect model/route/egress constraints. Apply all required ceilings, with
  atomic multi-scope reservation before claiming simultaneous group/user caps.
  The current ledger's single selected scope remains the implementation limit.
- A privileged exception requires an explicit scoped, expiring, audited policy
  change. Specificity or an `allow` result is not an implicit override mechanism.

Long-lived virtual keys carry an explicit subject grant, not a live snapshot of
directory roles/groups. Such requests still receive baseline and subject/grant
policies. A route requiring current group evidence must require renewable OIDC or
an approved, freshness-bounded directory lookup; it cannot skip restrictions
because a long-lived key has no groups. Short delegations retain bounded verified
evidence and its documented freshness limits. A budget attribution group is not
automatically a verified directory group. Broker workload subjects and their
human operator remain separate identities; ownership needs explicit mapping.

## Evaluator registry and bounded execution

An operator-controlled registry maps evaluator kind/version/digest to an approved
implementation and configuration schema. A policy cannot load arbitrary code,
shell commands, packages, URLs or WASM from a caller-supplied location. Registering
code, authoring policies, and releasing quarantine are separately authorized
operations. Bind signatures to artifact digests and persist revision floors to
prevent rollback. Activation validates the entire bundle atomically; an invalid
replacement does not silently become an empty policy. Last-known-good use ends
at its signed expiry; thereafter required enforcement denies.

Initial evaluator families:

| Family | Contract and limits |
| --- | --- |
| Regex | Precompiled bounded linear-time engine, bounded pattern/count/input sizes, explicit flags/normalization; no backtracking engine supplied by a plugin |
| Threshold | Typed bounded numeric facts, explicit units/operator and finite threshold; tokens remain neutral units, not prices |
| Semantic search/classifier | Pinned model/index/corpus/normalizer versions, top-k and score meaning, explicit threshold and unsupported coverage; similarity is not a calibrated probability |

Semantic indexes must be access-scoped before retrieval, and result evidence must
not reveal another user's corpus. Pin corpus provenance and test poisoned inputs.
Keep semantic inspection local initially. A remote evaluator is a separate data
egress destination needing explicit authorization, credentials, TLS and address
allowlisting, redirect/SSRF controls, independent accounting and a deadline.
It must not recursively call the inspected route or quietly send the prompt to
the provider before permission to send that prompt has been established.

Each bundle bounds total wall time, per-evaluator time, input/output bytes,
segments, findings, queue length, concurrency, CPU/fuel and memory. One absolute
deadline covers queueing, work and mandatory evidence admission; spawning N
evaluators does not multiply it. Cancellation disconnects must cancel pending
work and release admission slots. Late results cannot alter a completed decision.

Async timeout alone does not terminate CPU-bound/native work. Built-ins need
bounded algorithms and cooperative checks; third-party execution requires a
restricted WASM runtime with fuel/memory interruption or a killable worker process
with resource limits and no ambient filesystem/network credentials. Merely
putting synchronous plugin code in `spawn_blocking` is not deadline enforcement.
Benchmarks must measure deadline overshoot and cleanup under saturation.

Required enforce-mode checks default to block on timeout/error/unsupported input.
An operator may explicitly select quarantine when durable secure storage exists.
Observe-mode checks can forward with a recorded failure, only if no required rule
denies. Operators cannot use observe mode to weaken authentication or budgets.

## Inspection coverage and transparent forwarding

Inspect all decoded text-bearing request surfaces, including system/developer
instructions, user and assistant history/prefill, tool schemas/descriptions,
tool arguments/results, structured text fields and supported inline attachments.
Inspect concatenated logical content as well as segments where splitting could
hide a match. Track the exact parser/normalizer and field paths inspected.

Missing coverage is explicit. Unknown provider fields, nontext modalities, file
references and opaque/encoded content cannot be reported as fully inspected.
Per route, a required policy blocks or quarantines unsupported surfaces. Never
fetch an arbitrary prompt URL as an implicit inspection capability. Decode only
documented encodings within size/decompression limits; never silently truncate
the tail of a required check. Preserve original bytes separately from normalized
inspection text. Reject ambiguous duplicate JSON keys before dual interpretation.

On Sandhi's existing same-family transparent plane, forward the original admitted
body bytes. Inspecting must not reserialize or rewrite the prompt. Existing
cross-family and subscription routes still use their documented codecs and are
not byte-exact paths. Translation needs coverage of the final provider-bound
semantic request, including generated instructions; both paths share enforcement.
V1 does not include redaction/transformation. A future transform needs its own
versioned output, reinspection, explicit consent/policy and evidence of the change.

Prefill inspection completes before streaming to the provider begins. Post-output
inspection is a separate design: once streamed response text reaches the client,
the gateway cannot retroactively quarantine it. Do not claim output protection
from a pre-dispatch input gate.

## Evidence, quarantine and client behavior

Audit is an obligation independent of disposition: `forward + audit` is allowed;
`quarantine` always holds provider dispatch; `block` refuses dispatch. Ordinary
audit records contain IDs, principal references, rule/evaluator versions, coverage,
reason codes, timing and outcome. Do not put prompt excerpts, matched secrets,
tokens, raw embeddings or arbitrary evaluator text in logs, metrics or alerts.
If correlating content, use a deployment-scoped keyed digest; a plain hash of a
low-entropy secret permits guessing. Bound cardinality of telemetry labels.

Quarantine payloads, when enabled, belong in encrypted access-controlled storage
with retention/TTL, deletion, capacity quotas and a separately audited reader
role. The metadata receipt must not expose payload content. A full or unavailable
mandatory evidence store blocks; it never converts a hold to forward. Persist an
outbox entry atomically with the evidence receipt when alert delivery is required;
the current best-effort alert facility is not that guarantee. Notifications carry
opaque receipt IDs and safe summaries only. No plugin controls notification URLs.

Keep the initial LLM API behavior terminal and dialect-compatible: block and
quarantine return 403 with distinct stable codes (`policy_blocked`,
`policy_quarantined`) and an opaque request/receipt ID. Required evaluator/storage
failure returns a safe 503 `policy_unavailable`, marked as pre-dispatch; it is not
an LLM completion. No raw score, content or credential is reflected to the caller.
Victor must surface these outcomes without trying another provider/credential or
automatically resubmitting a quarantined prompt. HTTP status alone does not prove
that a generic upstream failure happened before dispatch.

Initial quarantine release is review plus explicit resubmission, not an automatic
replay worker. Resubmission uses current identity, grant, policy and budgets and a
new attempt ID linked to the receipt. Any later automated release must bind exact
payload/destination digest, reviewer authority, approval expiry and single-use
CAS state; ambiguous dispatch becomes `unknown`, never automatic retry. This is
the same safety principle as message-hub's durable approval/execution ledger.

## Illustrative policy fragment (not accepted configuration today)

This example selects a regex finding; replacing the registered evaluator with a
semantic implementation does not change identity resolution or final authority.
Digests and identities are placeholders, and production limits need measurements.

```json
{
  "schema_version": "1",
  "policy_id": "restricted-egress",
  "revision": 1,
  "bindings": [{
    "issuer": "https://id.anvaiops.com/oauth2/openid/sandhi",
    "groups": ["sandhi_agents"],
    "grants": ["subscription"]
  }],
  "limits": {"total_ms": 200, "max_input_bytes": 262144, "max_findings": 32},
  "checks": [{
    "id": "secret-patterns",
    "evaluator": "builtin.regex.v1",
    "config_ref": "approved-secret-patterns",
    "timeout_ms": 20,
    "mode": "enforce",
    "on_match": "quarantine",
    "on_timeout": "block",
    "on_error": "block",
    "on_unsupported": "block",
    "audit": "required"
  }]
}
```

The complete future bundle also requires validity/signature metadata, manifests,
baseline policy and evidence-store references. No API accepts this fragment yet.

## Implementation phases and TDD acceptance gates

1. **Contract and reducer:** Rust types/codegen, strict parsing, registry manifests,
   deterministic policy selection/composition and error outcomes. Property/table
   tests prove rule and group ordering cannot weaken a restriction, unknown fields
   and mismatched results fail closed, and no plugin result expands authority.
2. **Proxy regex/threshold enforcement:** bounded extraction, transparent/translated
   path coverage, required audit admission and lifecycle hooks. Tests exercise real
   router entrypoints with a counting upstream: denied, timed-out and quarantined
   requests make **zero upstream calls**; admitted transparent bodies are byte-exact.
3. **Durable evidence and operator UX:** encrypted payload store, permissions,
   retention, evidence/alert outbox transaction, explicit resubmission receipts.
   Fault tests cover disk full, crash before/after admission, revoked approvers,
   changed policy, duplicate release and ambiguous provider completion.
4. **Semantic/plugin isolation:** local calibrated fixture corpus, versioned index,
   sandbox resource controls, cancellation and cache scoping. Report false-positive
   and false-negative rates by dataset/version, plus latency and memory; test score
   boundary, stale index, unsupported modality, prompt injection and saturation.
5. **Distribution and parity:** signed caller-scoped bundles, persistent anti-rollback
   floor, expiry and revocation behavior, SDK/proxy decision parity. Client-side
   previews remain advisory where callers control execution. No anonymous inference
   is enabled by this work; any future guest credential requires explicit issuance,
   aggregate caps and rate limits. Shared replica caps remain a separate ledger gate.

Across phases, verify OIDC, short delegation and durable subject-key policy
selection; forged headers; missing/stale groups; role overlap; mandatory sink
failure; unknown fields and multimodal bypass; split/encoded secrets; regex resource
limits; semantic failure/late results; concurrency/queue exhaustion; restart and
policy rollback; no credential/prompt leakage into errors, evidence or metrics.

Measure end-to-end p50/p95/p99, total deadline overshoot, evaluator and queue time,
CPU/memory, admission throughput, audit latency and block/hold/error rates at
representative prompt sizes and concurrent load. The example 200 ms is a design
budget, not a measured guarantee. Identity TDD passing does not establish these
policy gates. The MVP now has red/green core, proxy, store and client-denial
coverage; the full workspace passed 778 tests (three live-provider tests ignored)
with 90.09% line coverage. Real Kanidm → isolated gateway → synthetic provider
acceptance, including Victor's rebuilt Python binding, passed 26 named checks.
Observed engine time in that functional smoke is not a load or p99 guarantee.
The remaining phases above still require their own implementation and evidence.
