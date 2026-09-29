# Policy MVP: local inspection before provider dispatch

Status: implemented candidate on the subscription-gateway worktree. Opt-in,
startup-only configuration; not a claim of production rollout or comprehensive DLP.

Set `SANDHI_POLICY_CONFIG` to a JSON policy file and keep `SANDHI_STORE` set to a
private durable SQLite file. An invalid policy, unknown evaluator or unavailable
mandatory store fails startup. Omitting the policy variable preserves existing
behavior; never omit it to recover from a policy error. Restart to change policy.
The process loads one immutable compiled revision. Signed distribution, hot reload
and a persistent revision floor are not part of this MVP.

The executable example is [message-hub-policy.json](../../examples/message-hub-policy.json).
The message-hub testbed keeps the same document at `config/sandhi-policy.json`.
It audits admissions, blocks private-key/API-key patterns and a synthetic test
marker, enforces a body-size threshold, and quarantines strong lexical overlap
with a synthetic restricted-data reference. It is a starting policy to tune, not
a complete catalog of secrets. Ordinary use of the word `confidential` is allowed.

## Contract and evaluators

Rust is authoritative. Generated contracts:

- [Policy document schema](../../schemas/policy-document.v1.schema.json)
- [Decision schema](../../schemas/policy-decision.v1.schema.json)
- Python `bindings/python/sandhi_gateway.pyi` and TypeScript
  `bindings/node/contracts.d.ts` include the corresponding document types.

Built-ins are `regex` (Rust's bounded-size, non-backtracking regex implementation),
`threshold` (`body_bytes` or explicitly supplied `max_output_tokens`, exclusive
`above` comparison), and `lexical_similarity` (Jaccard score over lowercase
alphanumeric words, inclusive `at_least` comparison). Lexical similarity is **not
semantic embedding search** and is not a calibrated probability. Long unrelated
text can dilute its score; use regex/threshold baselines and measure quality on
representative labeled examples before relying on similarity for enforcement.

The public Rust `Evaluator` trait returns a match or an error. `Registry::register`
installs a trusted compiled factory under a versioned name;
`Engine::from_slice_with_registry` compiles `kind=registered` rules using that
allowlist. Each factory validates its own configuration. Duplicate registration
and unknown names fail. The standalone binary registers only explicitly deployed local Python workers via
SANDHI_POLICY_WORKERS and optional embedded CPU models via SANDHI_POLICY_ONNX;
see [evaluator backends](python-ml-evaluators.md) and [ONNX deployment](embedded-onnx.md).
Policies cannot fetch/load code or choose a network endpoint. Sandboxed third-party
plugins, remote classifiers, transformer/embedding profiles and vector indexes remain future work.
Do not pass untrusted native code to the embedding registry.

Every rule names `effect: audit | quarantine | block`. All matching rules apply;
block dominates quarantine, quarantine dominates forward. Audit-only findings
permit forwarding, subject to existing authorization and budget enforcement.
All completed evaluations, including clean admissions, require a durable receipt.
Evaluator/coverage/deadline failure stops admission; it cannot become a clean pass.
If a denial is already known, a later evaluator failure cannot permit forwarding.

## Identity and selection

`when` may select issuer, subjects, groups, roles, upstream references and models.
Distinct fields are AND; values within a field are OR; strings match exactly.
An absent/empty selector is deployment-wide. Caller headers cannot provide group
or role authority. Existing model/grant/budget/rate checks still bound access.

OIDC authentication supplies the verified issuer/subject and explicit group-claim
evidence. Policy sees all assigned roles from applicable subject/group bindings,
not only the highest dashboard privilege. Role selectors are exact assignments;
there is no implicit hierarchy expansion. A missing group claim is **unknown**,
distinct from a verified empty array. Short delegated keys carry that evidence
with their bounded source lifetime. Previously issued delegations lacking the new
evidence flag require renewal before using directory-dependent policy.

Durable subject keys retain identity but do not carry directory roles or groups.
They can use deployment/subject/upstream/model policies. If otherwise-applicable
policy needs directory evidence, such a key fails closed; choose renewable OIDC
for that route. Legacy compatibility keys have subject identity but no OIDC
issuer/directory evidence. A budget attribution group is never treated as a
directory group. Policy configuration does not grant inference or administration.

## Supported request coverage

The initial gate supports **text-only OpenAI Chat Completions ingress**, including
its typed translation to the existing subscription Responses backend. It checks
system/developer/user/assistant prefill, tool results, tool descriptions/schemas,
decoded tool-argument JSON, and supported string-bearing request extras. Regex
also runs over joined text to catch a marker split across text parts/messages.
JSON escapes are decoded; duplicate keys in the request or tool arguments fail.
Unknown top-level/message fields, image/file/audio content parts and other ingress
dialects return `policy_unavailable` while the gate is enabled. There is no silent
uninspected fallback. This narrower coverage is intentional for the MVP.

No remote URLs are fetched. Arbitrary base64, steganography, other encodings and
semantic paraphrases are not automatically decoded/detected. Text matching is not
a guarantee against all secret exfiltration. Tool arguments must be complete JSON;
partial assistant tool prefill may be rejected. No output-stream inspection or
prompt redaction/transformation is implemented.

Allowed same-family transparent requests keep their original body bytes. Existing
cross-family/subscription codecs retain their normal transformations; this MVP
inspects ingress text, not a second fully rendered provider-body copy. Provider
codec-generated instructions remain trusted code and need review when changed.

## Bounds and failure behavior

- Policy document: at most 128 KiB and 32 rules; rule IDs at most 64 characters.
- Request inspection: configured `max_body_bytes`, maximum 256 KiB; no truncation.
- Regex: at most 4 KiB pattern and 256 KiB compiled/DFA limits per rule.
- Lexical reference: at most 4 KiB; tokenization stops with failure above 8,192 words.
- Four policy evaluation slots per process; excess concurrency is rejected without queueing.
  Optional Python pools have their own total four-process cap.
- One configured deadline, 1–2,000 ms, includes worker scheduling, evaluation and
  required receipt admission. Late results never authorize a provider call.
- SQLite uses FULL synchronous durability, a short busy timeout and no mutex queue.
  Its maximum 100,000 receipt capacity is mandatory: when full, admission fails.

Timeout denies the HTTP attempt. Rust/native work cannot be forcibly killed by an
async timeout; the worker keeps its slot until cleanup. Built-ins use bounded
algorithms/input sizes and check deadlines. Trusted embedding extensions must do
the same. This is not a hard real-time CPU deadline or untrusted plugin sandbox.
The deadline does not include OIDC authentication or subsequent inference time.
Shutdown/cancellation cannot dispatch a late policy result. Configuration is static
for the process; runtime identity/key revocation retains the existing admission
race boundary rather than adding an atomic revocation/dispatch transaction.

## Receipts and quarantine

`policy_receipts` contains an opaque ID, timestamp, issuer/subject, public key ID,
policy revision, matched rule IDs, disposition and engine elapsed microseconds.
It stores **no prompt, matched secret, token, embedding or raw classifier output**.
The `x-sandhi-policy-receipt` response header correlates a completed policy check
with its receipt, including an admission later refused by a budget/provider.
This is an **admission record**, not proof of provider completion. Existing usage
events still record provider usage independently. No guaranteed alert outbox or
automatic per-receipt provider-outcome join is implemented.

Block/quarantine return 403 with `error.code` `policy_blocked` or
`policy_quarantined`. Missing inspection/evidence/deadline capacity returns 503
`policy_unavailable`. Responses include `x-sandhi-policy-code`; errors are marked
non-retryable in the JSON contract. Clients must honor these codes and avoid
provider failover. Generic clients may ignore that contract; deployment must
keep upstream credentials solely at the enforced gateway.

Quarantine means **hold dispatch and record metadata**, not retention of the raw
request. There is no automatic replay or release endpoint. An operator reviews
the receipt and the original request in its authorized source, then explicitly
resubmits under current policy. Encrypted payload capture and approval/release
workflows are separate phases of TD-0005. Protect the database directory and its
backups. Capacity/retention is operator-managed; this MVP never silently deletes
mandatory evidence to admit more traffic. Alert delivery remains the existing
budget facility; content-policy alerts need a later durable outbox.

## Verification and rollout

`cargo test --workspace policy` exercises core selection/reduction, extension
registration, failures, coverage, deadlines, store persistence and real router
zero-call denials. Full workspace fmt/Clippy/coverage and codegen remain gates.
The message-hub script `docs/validation/policy-live-acceptance.py` runs the solution
policy with real Kanidm identities against an isolated gateway and synthetic
provider. It also verifies short/durable keys, directory-evidence requirements,
private metadata receipts and restart/revocation. It never uses a real LLM or
restarts the production gateway.

Keep the direct-tunnel route disabled for any workload claimed to be governed by
this policy. Do not cut message-hub over until its actual classify/Chat Completions
path, latency budget, fallback behavior and connector acceptance have been tested.
The MVP does not yet make the custom `/v1/classify` endpoint policy-aware. See
[TD-0005](../td/TD-0005-declarative-policy-engine.md) for subsequent phases.


### Remote inspection egress

The [remote evaluator template/adapter](../../templates/python-evaluator/README.md)
is opt-in through `SANDHI_POLICY_REMOTE`. Its destinations and service credentials are
operator-owned, separate from policy selectors and end-user credentials. The engine
runs local checks first, regardless of document order. A local denial or incomplete
check prevents remote text transmission; a remote denial stops subsequent remote
checks. Receipts therefore describe executed checks. Keep remote evaluator selectors
within the identities authorized to disclose text to those services. Replica failures
never trigger replay or bypass the existing terminal policy response.
