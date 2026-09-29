# Evaluator backends: native, Python, ONNX and remote

Status: September 27, 2026 feature candidate. Backend-neutral scores and supervised
local Python execution are implemented and tested. The optional [embedded ONNX
profile](embedded-onnx.md) and [named remote services](../../templates/python-evaluator/README.md)
are implemented candidates with bounded Sandhi-owned replica routing. This is separate from Sandhi's Python client
transport binding.

## One policy contract, multiple execution backends

Python is not required in production. The core `ScoreBackend` trait accepts the
bounded `Inspection` and absolute deadline, and returns a finite score in [0,1]
or an error. `ThresholdedScore` applies a validated threshold and produces a
finding for the existing `Evaluator` contract. Scores are not necessarily
probabilities. Sandhi alone combines findings with verified identity and policy
effects; no evaluator grants access, selects credentials or spends a budget.

The existing registry now accepts captured, thread-safe factories. Trusted
embedding code can register an in-process model; the proxy can register an
out-of-process adapter under the same versioned evaluator name. Policy rules only
reference that name and validated configuration. They cannot load code, choose an
executable, fetch a model or select an arbitrary endpoint. Deployment manifests
belong to the operator, separate from user/group/role policy.

| Backend | Status | Scaling and isolation |
| --- | --- | --- |
| Native Rust regex/threshold/lexical and registered score backend | Implemented | Per-replica bounded work; shared gateway process |
| Supervised local Python pool | Implemented on Unix | At most four workers total per gateway; owned process can be killed/reaped |
| [Embedded ONNX](embedded-onnx.md) | Implemented optional CPU/ASCII TF-IDF profile | One pinned session/model per replica; scale with gateway replicas |
| Remote evaluator service | Implemented HTTP candidate | One multi-model Flask listener/server, supervised children, TLS/mTLS and Sandhi-owned replica routing |

The first ONNX-exported TF-IDF model now runs inside Sandhi without a Python worker.
Additional NLP/tokenizer profiles still need model-specific implementation. Export
compatibility, tokenizer/normalizer behavior, unsupported/custom operators,
execution-provider support and numerical/threshold parity need golden tests.
Do not download runtime libraries or artifacts during admission. Cap input tensor
shapes, batch size, model memory and intra/inter-op threads. ONNX Runtime exposes
termination and thread-pool controls, but test cancellation with the actual model
and execution provider; do not claim a process-isolation boundary for embedded
native execution. See [thread management](https://onnxruntime.ai/docs/performance/tune-performance/threading.html)
and [run termination](https://onnxruntime.ai/docs/api/c/struct_ort_1_1_run_options.html).

## Implemented Python adapter

`SANDHI_POLICY_WORKERS` points to a strict JSON deployment manifest and requires
`SANDHI_POLICY_CONFIG`. This is startup-only, opt-in, and fails closed on malformed
configuration, unknown evaluators, changed artifacts or failed readiness. Example
shape (replace paths and hashes with reviewed deployment values):

```json
{
  "version": 1,
  "workers": [{
    "name": "sklearn.tfidf.v1",
    "python": "/opt/policy-env/bin/python",
    "script": "/opt/sandhi/scripts/policy_text_worker.py",
    "script_sha256": "<64 lowercase hex characters>",
    "artifact": "/opt/policy/model.json",
    "artifact_sha256": "<64 lowercase hex characters>",
    "pool_size": 1,
    "startup_timeout_ms": 10000
  }]
}
```

Policy rule:

```json
{
  "id": "review-python-reference",
  "effect": "quarantine",
  "evaluator": {
    "kind": "registered",
    "name": "sklearn.tfidf.v1",
    "configuration": {"at_least": 0.6}
  }
}
```

The manifest allows up to four deployments with at most four total workers.
Executables and files require absolute paths. Script (256 KiB maximum) and JSON
artifact (1 MiB maximum) SHA-256 values are checked at startup/replacement. Protect
the files and interpreter/environment with immutable deployment permissions.
Hashes do not pin transitive Python dependencies or defend against a privileged
operator racing file replacement. Dependency/image provenance remains deployment
responsibility. Give a changed artifact/configuration a new policy/deployment
revision and retain its manifest; receipts currently record policy revision/rule
IDs, not a full model/environment digest join.

Each worker uses a private Unix socket pair carried through stdin/stdout, Python
isolated mode, an empty inherited environment and single-thread math-library
settings. It receives text and a correlation ID, not credentials, identity,
groups, provider URL or policy decisions. Standard error is discarded, and reply
frames contain only protocol version, matching ID and normalized score.
Malformed/unknown fields, NaN/out-of-range values, oversized frames, EOF and stale
IDs deny admission. The adapter re-applies an absolute deadline to partial reads
and writes. Text views are capped at 256 KiB each, request frames at 2 MiB and
response frames at 4 KiB.

Readiness loads the model and exercises known/empty inputs before serving.
One slot admits one request; there is no pending workload queue. Saturation
fails immediately. Timeout/crash/protocol failure denies that attempt, kills and
reaps its worker, and warms a replacement in the background before reuse.
At most three replacements per slot are attempted over its lifetime; a failed
restart or exhausted allowance leaves that slot unavailable until gateway
restart. Failed requests are never replayed. Normal shutdown closes channels,
joins supervisors and reaps owned processes. Abrupt gateway/host termination
still needs a service manager/container that cleans up the whole process group.

This is **trusted-code process isolation**, not an adversarial plugin sandbox.
No OS filesystem/network deny policy or memory cgroup is installed by this
adapter. Run workers under a constrained service/container with resource limits
and denied network access before processing sensitive production text. Arbitrary
third-party code and descendant process trees are outside this MVP. Native/OS
cleanup is not a hard real-time guarantee; a timed-out result cannot authorize
dispatch regardless of cleanup latency.

## First library integration and experiments

`scripts/policy_text_worker.py` uses scikit-learn TF-IDF plus cosine similarity.
It reconstructs a bounded vectorizer from pinned JSON references at readiness;
there is no pickle, model download, online training or per-request artifact load.
It is lexical matching, not semantic understanding or comprehensive DLP.

`scripts/policy_text_experiment.py` evaluates an explicit labeled corpus and
writes metadata-only JSON: model/corpus digests, library version, threshold,
confusion counts and warmed sequential p50/p95/p99 timings. It imports MLflow only
when the operator supplies `--mlflow-uri`. Initial export accepts an absolute
local SQLite URI only. It logs parameters/metrics, not prompts, matched entities,
reference texts, embeddings or artifacts. No autologging or runtime tracing is
enabled. Experiments do not modify active gateway policy or automatically promote
a model.

Example, from the Sandhi root with the chosen Python environment:

```bash
python scripts/policy_text_experiment.py \
  --artifact examples/text-evaluator-model.json \
  --corpus examples/text-evaluator-corpus.json \
  --threshold 0.6 --output /private/path/experiment.json \
  --mlflow-uri sqlite:////private/path/experiments.db
```

The output path must be new. MLflow/SQLite dependencies are optional; the worker
only needs scikit-learn and its dependencies. Tested here with Python 3.12,
scikit-learn 1.9.1, NumPy 2.2.6, SciPy 1.17.1 and MLflow skinny 3.16.1.

The ten synthetic examples found four true positives, three true negatives,
two missed paraphrases and one benign-question false positive at threshold 0.6.
This fixture proves the experiment/transport path, not acceptable production
detection quality. Its warmed local timings are not gateway or service-load SLOs.

MLflow owns experiments, lineage and controlled promotion outside the hot path.
Resolve mutable registry aliases during deployment and pin the resulting
artifact/environment/tokenizer/index digests. Registry promotion automation,
signed manifests and asynchronous production metric export are not implemented.
A tracking outage cannot affect an already-warmed worker because it has no
MLflow dependency. Keep mandatory policy receipts independent of optional
experiment telemetry. Official references:
[tracking](https://mlflow.org/docs/latest/ml/tracking/),
[registry](https://www.mlflow.org/docs/latest/ml/model-registry/workflow/),
[TF-IDF](https://scikit-learn.org/stable/modules/generated/sklearn.feature_extraction.text.TfidfVectorizer.html).

## Horizontal scaling contract

Gateway replicas must agree on immutable policy/deployment revisions; health and
readiness should expose that agreement before routing requests. Embedded sessions
and local pools replicate with the gateway. Set per-replica worker/thread/memory
limits explicitly so N replicas do not oversubscribe the host or shared GPU.
Retain deployment manifests alongside policy receipts for investigation.

The [cookie-cutter project](../../templates/python-evaluator/README.md) now provides
one business interface, a content-addressed local-worker zipapp, a multi-model Flask
service, private sample manifests and conformance tests. `SANDHI_POLICY_REMOTE` loads
operator-owned replica URLs and service credentials; consumers keep using Sandhi.
One named URL per model shares one listener per server. Gunicorn owns the HTTP process;
supervised child pools own model loading and hard process replacement. Optional direct
TLS/mTLS avoids requiring a separate load balancer or TLS proxy.

Sandhi uses bounded round-robin selection, connection reuse and a one-second passive
failure exclusion. At most four remote replica slots total, no application retry,
redirect, environment proxy or request-selected destination. A failed request denies
admission; later requests can select another available replica. Replies must match
request ID, evaluator name, code/artifact hashes and finite score bounds. Remote health
is passive; startup does not probe servers. Local checks run first regardless of rule
order, and local block/quarantine/incomplete evaluation prevents external text egress.
The first remote denial also prevents further remote disclosures. This means receipts
record checks actually executed, not all rules that might have matched.

The manifest separately authorizes evaluator egress. Restrict identity/group/role
selectors and network destinations to the approved trust boundary; do not assume an
LLM-provider allowlist automatically authorizes arbitrary inspection services. Model
servers receive text views only, never user keys, IdP tokens or identity/group claims.
They return scores; Sandhi owns permissions, effects, budgets, alerts and evidence.
HTTPS verification is mandatory outside literal loopback development, with optional
private CA and client identity files. Service keys are separate from user virtual keys.
Immutable files/environment, network restrictions, cgroups and deployment review remain
operator responsibilities. No distributed queue, service discovery, autoscaler or
model-quality guarantee is included.

The evaluator tier can be stateless across replicas. Distributed budgets, rate
limits, revocation propagation and mandatory evidence are a separate control-plane
problem. Sandhi's current SQLite ledger is single-node; do not multiply a user's
allowance by deploying independent per-replica ledgers or place SQLite on a shared
network volume as an HA substitute. Shared atomic reservations/settlement and
distributed limits remain TD-0007 gates. Scaling an evaluator service does not
require scaling or bypassing the authoritative gateway ledger.

## Verification

Red/green evidence covers missing adapters followed by successful implementation,
shared score thresholds/errors, artifact/readiness rejection, saturation, native
sleep timeout, crash replacement, stale/nonfinite/oversized replies, environment
isolation, zero provider calls on denial and owned-process cleanup. The real
scikit-learn acceptance test is opt-in:

```bash
SANDHI_TEST_PYTHON=/absolute/env/bin/python cargo test -p sandhi-proxy \
  --test policy_workers python_worker_real_sklearn_model_acceptance -- --ignored
```

The message-hub live harness supports `--python-worker /absolute/env/bin/python`
and `--victor`; it combines real IdP identity, actual Victor transport, the worker
and synthetic provider. No production gateway or real provider is touched.



For the technical difference between the current subprocess and a Flask/HTTP
model service, see [local worker versus Flask endpoint](embedded-onnx.md#local-worker-versus-flask-endpoint).

## Local transport comparison

A synthetic marker workload ran through the policy engine with native regex, the
generated Python zipapp and two actual TLS Flask replicas (one requiring mTLS).
One hundred warmed sequential requests/backend gave p50/p95/p99 of 9/19/27 us,
45/73/106 us and 945/1283/1456 us respectively on this development host.
These are debug-build policy-engine measurements, excluding gateway authentication,
receipt storage and provider time, not service SLOs or real NLP/GPU throughput.
Prefer built-ins for deterministic checks, embedded ONNX for supported models with
validated cancellation, local workers for Python-only libraries, and remote services
when shared model memory/GPU ownership or independent scaling justifies the network
hop. Do not add a network service solely for this marker fixture.


The service template now exposes authenticated per-model operational status with
bounded metadata-only counters and explicit saturated/recovering/unavailable states.
Tests cover exhausted restart allowance with healthy sibling models, passive replica
recovery without replay and fixed-capacity overflow. The canonical TLS smoke records
accepted/rejected requests separately and sampled process CPU/RSS under a bounded
synthetic burst; it does not establish production capacity or resource isolation.
See the template runbook for counter semantics and restart behavior.
