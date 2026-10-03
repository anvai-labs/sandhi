# Embedded ONNX policy evaluator

Status: implemented opt-in feature candidate, September 27, 2026. CPU-only,
fixed-shape ASCII TF-IDF and UTF-8 trigram profiles; not a general transformer/tokenizer adapter.
The optional [sensitive-text bundle](sensitive-policy.md) provides audit-only classifier defaults
alongside narrow credential-pattern blocks.
Remote HTTP services now have a separate [template and adapter](../../templates/python-evaluator/README.md); gRPC remains unimplemented.

## Deployment

Build the proxy with `--features policy-onnx`. This optional feature requires
Rust 1.88 or newer; validation used Rust 1.95. The workspace's default feature
does not include ONNX. The pinned ort 2.0.0-rc.13 wrapper uses API 26, with
automatic runtime downloads and default execution-provider features disabled.
The operator supplies an approved ONNX Runtime >=1.26 shared library by absolute
path and SHA-256. Validation used ONNX Runtime 1.26.0 on macOS arm64. Other
platforms/runtime builds require their own acceptance.

The ONNX dependency is proxy-only. Neither sandhi-core nor the Python/Node
client bindings gains a native runtime dependency. Model inference is inside
the proxy process; it does not launch or call Python. Python is used offline
to export and test the model.

Export the existing reference model into a new private directory:

```bash
python scripts/policy_onnx_export.py \
  --artifact examples/text-evaluator-model.json \
  --runtime-library /absolute/path/to/libonnxruntime.dylib \
  --corpus examples/text-evaluator-corpus.json \
  --output-dir /private/path/new-onnx-deployment
```

The development environment needs scikit-learn, NumPy and onnx. Parity tests also
use Python onnxruntime. This export was validated with Python 3.12, scikit-learn
1.8.0, ONNX 1.21.0 and ONNX Runtime 1.26.0. Do not assume a different library
version has identical tokenization or numerical behavior.

The export directory is mode 0700 and contains model.onnx, preprocessing.json,
deployment.json and golden.json. Golden cases contain source text; do not upload
or commit private corpora. The manifest pins the native library, ONNX graph and
preprocessor. Install files and the runtime in an immutable operator-owned
location before deployment. Runtime transitive libraries, native dependencies
and graph resource behavior are deployment trust responsibilities.

Set `SANDHI_POLICY_ONNX=/private/path/new-onnx-deployment/deployment.json` together
with `SANDHI_POLICY_CONFIG` and the mandatory `SANDHI_STORE`. A build lacking
the optional feature rejects this configuration. Missing policy, malformed
manifest, unknown name/profile, changed hashes or failed model readiness fail
startup. There is no fallback to an uninspected route or another model.
`SANDHI_POLICY_WORKERS` can coexist for separately named Python evaluators.

Example policy rule:

```json
{
  "id": "review-onnx-reference",
  "effect": "quarantine",
  "evaluator": {
    "kind": "registered",
    "name": "onnx.tfidf.v1",
    "configuration": {"at_least": 0.6}
  }
}
```

The manifest format is strict JSON:

```json
{
  "version": 1,
  "runtime_library": "/absolute/path/to/libonnxruntime.dylib",
  "runtime_sha256": "<64 lowercase hex characters>",
  "models": [{
    "name": "onnx.tfidf.v1",
    "model": "/private/path/model.onnx",
    "model_sha256": "<64 lowercase hex characters>",
    "preprocessing": "/private/path/preprocessing.json",
    "preprocessing_sha256": "<64 lowercase hex characters>"
  }]
}
```

## Contract and bounds

The adapter implements the same `ScoreBackend` and threshold contract as the
Python process adapter. Identity, groups, roles, audit, budgets and final provider
dispatch remain Sandhi decisions. Policy data cannot choose a native library or
model path.

The initial `ascii_word_counts_v1` profile requires ASCII text, lowercases it and
counts words of at least two ASCII alphanumeric/underscore characters. It inspects
both separated and concatenated text views. Vocabulary entries are unique,
lowercase, 2–256 characters; vocabulary size is 1–4,096. Each text view is bounded
at 256 KiB. Unsupported Unicode input returns policy_unavailable rather than
silently using a different tokenizer. Multilingual input needs another validated
preprocessing profile/backend.

The ONNX graph receives float32 counts of shape [2, vocabulary_size] and returns
one finite score in [0,1] of shape [1]. The exporter moves IDF weighting,
normalization, reference comparison and score reduction into the graph. Loader
readiness validates tensor names/types/fixed shapes and executes a zero input.
This validates the runtime contract, not the classifier's quality.

At most four model deployments load per gateway. Each has one session with one
intra-op and one inter-op thread, sequential execution, no CPU arena and no memory
pattern cache. A busy session rejects another evaluation immediately. Manifest,
preprocessor, graph and native-library file bounds are 64 KiB, 256 KiB, 8 MiB and
256 MiB respectively. These file/input limits are not a native heap-memory cap:
only reviewed graphs are allowed. The exporter emits a self-contained graph;
the loader trusts model operations and does not sandbox arbitrary ONNX programs.

One absolute deadline covers preprocessing and inference admission. A bounded
per-call watchdog signals ONNX Runtime termination at that deadline; late/error
results cannot authorize dispatch. A deliberately slow Loop graph verified
in-flight cancellation on the tested CPU runtime and that the session remained
usable afterward. Termination is cooperative and execution-provider dependent,
not process isolation or a hard real-time guarantee. A noncooperating native
kernel can retain its slot; a process/service boundary is required when that
failure domain is unacceptable.

Model and runtime diagnostics are suppressed to avoid leaking evaluated text.
Receipts still record policy revision/rule IDs rather than a complete model
provenance join. Retain immutable manifests per revision and advance deployment/
policy revisions when changing model, preprocessor, runtime or threshold.

## Measurements and scaling

A sequential local comparison used the same reference model and alternating
sensitive/benign inputs, five warmups and 100 measured engine evaluations:

| Backend | p50 | p95 | p99 |
| --- | --- | --- | --- |
| Embedded ONNX | 73 µs | 118 µs | 136 µs |
| Local Python worker | 285 µs | 424 µs | 472 µs |

This includes policy-engine evaluation and each adapter's work; it excludes
identity, durable receipts, provider calls, cold starts and concurrent load.
It is a development microbenchmark, not an SLO or general ONNX/Python comparison.
Both paths retain the TF-IDF fixture's missed paraphrases and false positives.

Embedded sessions scale with gateway replicas. Pin the same policy/model/
preprocessing revision across replicas, with reviewed runtime hashes per platform.
Replicating evaluator capacity does not create a shared authorization ledger:
distributed budgets/rates/revocation/evidence remain separate TD-0007 requirements.
Keep the existing single-node gateway authority until those are implemented.
Remote evaluators can eventually scale independently behind an authenticated
endpoint without duplicating the gateway ledger.

## Local worker versus Flask endpoint

A local Python worker is an owned subprocess with a preloaded model and a private
socket connected to stdin/stdout. It opens no HTTP port. Sandhi starts, times out,
kills/reaps and replaces that process. It shares the host but not the gateway's
native process failure domain.

A Flask endpoint is a separate HTTP service, possibly on another host. The new
[template](../../templates/python-evaluator/README.md) shares one listener among named
model routes and owns supervised child pools. Sandhi's remote adapter authenticates
to operator-configured replicas with service credentials and optional mTLS, validates
model/code provenance and enforces a client-side total deadline. The server independently
bounds and terminates model execution; an HTTP timeout alone cannot establish the exact
instant remote inference stopped. Flask serves through Gunicorn, with bounded HTTP
threads and child counts. See the [Flask deployment guide](https://flask.palletsprojects.com/en/stable/deploying/).

Consumers use the same Sandhi endpoint for all backends. Optional HAProxy/platform
ingress can provide infrastructure routing, but Sandhi already owns bounded replica
selection, passive failure exclusion and fail-closed policy decisions. Scaling evaluator
servers does not create a shared HA budget/rate ledger for multiple gateway replicas.
