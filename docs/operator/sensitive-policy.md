# Optional sensitive-text policy bundle

This homelab testbed combines narrow credential-pattern blocks with an **audit-only**
ONNX text classifier. It is opt-in. Normal installations do not inspect content
unless an operator configures a policy. Native release builds include the ONNX
adapter and small model bundle; ONNX Runtime remains an explicit, pinned dependency.
Python builds/installs artifacts offline. No Python process, external evaluator port,
GPU memory, or classifier HTTP call is used during embedded evaluation.

## Configure

Build from source with `cargo build -p sandhi-proxy --bins --features sentinelpass-ipc,policy-onnx`.
Obtain an approved ONNX Runtime 1.26 shared library for the server platform. Then:

```sh
python3 scripts/sensitive_policy.py prepare \
  --runtime-library /absolute/path/libonnxruntime.so.1.26.0 \
  --output /private/new/sensitive-policy-v1
```

Preparation verifies the shipped artifact inventory, refuses an existing destination,
creates a private directory/files, and emits the two environment settings:

```sh
SANDHI_POLICY_CONFIG=/private/new/sensitive-policy-v1/policy.json
SANDHI_POLICY_ONNX=/private/new/sensitive-policy-v1/deployment.json
```

Set `SANDHI_STORE` too. Keep the deployment and native library immutable to the service.
Restart to activate a reviewed revision. Invalid hashes, unknown profiles, an unavailable
runtime, or a failed readiness inference reject startup. Do not substitute an uninspected
upstream when policy admission fails. Disabling inspection is an explicit operator change.

The default rules block private-key headers and narrowly recognized provider-token
formats. Email-like text and classifier scores of at least 0.75 produce audit findings
and forward. These are opinionated demo rules: a quoted private-key header in technical
documentation will also be blocked. Review/tune a copied policy and advance its revision.
The model's registered name does not confer authorization to a user or group.

The policy also inspects text on `/v1/embeddings` and `/v1/completions`, including
all batch items and string-bearing options. Token-ID inputs and unsupported fields
are rejected rather than forwarded uninspected. See [text endpoints](text-endpoints.md).
Other wire dialects still fail closed when this policy is enabled.

## Model evidence and limits

`scripts/sensitive_policy.py build` reproduces a small logistic-regression ONNX graph
from 256 repository-owned synthetic examples. No real messages, passwords, customer
records, or downloaded training corpus are included. The source code is governed by
the repository license. Validation uses Python 3.12, scikit-learn 1.8.0, ONNX 1.21.0,
and ONNX Runtime 1.26.0. The model card and golden scores ship beside the model.

Preprocessing hashes UTF-8 byte trigrams into 512 bins using FNV-1a, lowercasing ASCII
bytes only. It evaluates separated and concatenated message views. Input is capped
at 256 KiB per view; Rust checks the deadline every 1,024 trigrams. The graph normalizes
counts, applies learned linear weights and a sigmoid, and returns the maximum of
the two view scores. It uses the existing CPU-only, single-thread, bounded-admission
ONNX adapter. Unicode is accepted; this does **not** establish multilingual detection.

The ten synthetic holdout examples happen to separate at the shipped threshold.
They are a smoke test, not a production quality estimate. An additional boundary
probe, `é界`, produces a false positive (approximately 0.896). This known limitation
is retained in the golden fixtures. Scores are not calibrated probabilities.
Hash collisions, paraphrases, obfuscation and dilution in long mixed text can cause
false positives or misses. Never promote this classifier to automatic blocking
without representative, consented evaluation and a reviewed threshold change.

## Operator evidence

The dashboard's **Content policy** panel and authenticated `GET /admin/policy` show
the active revision, deadline, rule actions/evaluator names, and durable receipt
capacity. They do not expose regex patterns, identity selectors, prompts, secret
values, or individual subjects. Public dashboard mode does not open this endpoint.
Clearing the token removes policy data along with the other privileged panels.

Receipts store rule IDs and decisions, not original text. The 100,000-row cap remains
fail-closed; the dashboard shows capacity, but automatic retention/export is not
implemented. Audit/classifier matches do not imply alerts were delivered, and an
admission receipt does not prove the provider completed the request.

## Classification and authorization boundaries

Victor owns `/v1/classify`; Sandhi currently governs the model call through its chat
ingress and InferFlux upstream adapter. Sandhi has no native `/v1/classify` route.
Use Victor → Sandhi → InferFlux for application task classification. For request-time
policy evaluation use embedded ONNX or the bounded evaluator contract, avoiding a
Sandhi → Victor → Sandhi recursion. Classification may supply risk/intent evidence;
verified identity and deterministic policy own permission grants.

Before treating Victor's current classify route as a security evaluator, harden its
mandatory schema validation, operation-wide deadline, model/preset authorization,
accounting across parse retries, and terminal policy-denial propagation. Those changes
are separate from this embedded detector. Nothing here claims parity with proprietary
agent permission classifiers.

Run `python -m pytest scripts/test_sensitive_policy.py -q` and
`python scripts/smoke_sensitive_policy.py` after installing the pinned export dependencies.
The latter runs real Rust/ONNX parity and default-policy action checks.
