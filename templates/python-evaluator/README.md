# Python evaluator project

One business class, two transports: supervised local IPC and a single Flask service
hosting named model routes. Python 3.11+ on Unix. Edit `evaluator_app/evaluate.py`,
`artifact.json`, `evaluator.json` (unique versioned name), and model golden tests.
The example marker detector is a protocol fixture, not production DLP.

## Generate and customize

From the Sandhi checkout, using the interpreter/environment that will load the model:

```bash
python scripts/new_policy_evaluator.py /absolute/new-project --name company.detector.v1
cd /absolute/new-project
python -m pip install -r requirements-dev.txt
python -m pytest tests -q
python build_bundle.py
```

The destination must not exist. The generator copies conformance tests and builds a
content-addressed zipapp. The bundled application imports under Python `-I`, so Sandhi
pins **all bundled application code**, including business logic, with one SHA-256.
Third-party dependencies and interpreter remain separately pinned by your environment
lock/container image; they are not included in the archive. The worker requires no
Flask dependency. Flask/Gunicorn are optional HTTP dependencies; the supplied ranges
are bootstrap requirements, not a production lockfile.

`Evaluator.__init__` loads the artifact once. `self_test()` must exercise golden
positive/negative inputs and raise on failure. `score(text, joined)` returns a finite
number in [0,1], never an authorization decision. Preserve both inspection views,
including split-text coverage. Add NLP libraries to your pinned environment and load
local artifacts here. Model downloads, MLflow, training, external calls, logging
prompt fragments, and importing user-supplied code do not belong in this method.

`build_bundle.py` regenerates **development examples** under `.runtime/` (private,
ignored by Git), preserving the service secret. It overwrites sample deployment and
policy configurations; copy reviewed configurations to a separate deployment directory
before editing TLS, replica lists or multi-model settings. Do not rebuild an active
production project in place. Publish immutable bundles/artifacts and review new hashes
before restart. Retain old deployment manifests for evidence.

## Local worker: no listening port

Set `SANDHI_POLICY_WORKERS` to the generated `.runtime/sandhi-workers.json`,
`SANDHI_POLICY_CONFIG` to `.runtime/policy.json`, and `SANDHI_STORE` to your gateway
store. Configure normal gateway identity/upstream settings separately. The example
policy blocks marker detections; change the effect/threshold/selectors through review.
Sandhi owns process startup, input bounds, deadlines, replacement and cleanup.
Do not also register the same name through a remote or ONNX adapter.

## Remote service: one listener, multiple models

Start **one service per server/container**, not one service per evaluator:

```bash
python build/evaluator-<sha256>.pyz --http /absolute/service.json
```

Use the generated `.runtime/service.json` as a starting point. Its `evaluators` array
contains absolute paths to the model configurations (`.runtime/http.json` from each
project). Names must be unique. Each model can use its own Python environment and
bundle. The service holds one shared authentication credential and one port; model
configuration `auth_file`/`port` fields are local single-model defaults and do not
create listeners or override service configuration. Initially up to four models and
four total child processes are allowed per service.

```json
{
  "version": 1,
  "bind_host": "127.0.0.1",
  "port": 9088,
  "auth_file": "/run/secrets/evaluator-service.key",
  "evaluators": ["/opt/models/pii/http.json", "/opt/models/safety/http.json"]
}
```

Gunicorn serves one Flask application process with four HTTP threads, 16 active connections and a 16-connection listen backlog; each model has
its own supervised child pool. This avoids accidental model duplication from WSGI
worker multiplication. Shared listener does not mean loading every model into the
Flask process. Pool saturation fails promptly; there is no inference backlog. A hung
model is killed/reaped and replaced before reuse, with at most three replacements per
slot. Test actual model startup, peak memory, throughput and timeout behavior before
changing those limits. Graceful service shutdown reaps children; abrupt termination
requires a service manager/container that cleans the process group.

Remote binds require TLS. Set `bind_host` to an IP address and add absolute
`tls_certfile`/`tls_keyfile` paths. The private key must be owned by the service user,
regular, non-symlink and mode 0600. Optional `tls_cafile` requires client certificates
(mTLS). The service bearer key is still required. Loopback plaintext is allowed for
local development or a separately managed TLS ingress. Use network allowlists, a
non-root dedicated UID, immutable mounts, CPU/memory/PID limits and denied model-worker
network access. This template supervises trusted model code; it does not install an OS
sandbox or a general public internet ingress defence.

Routes:

- `GET /healthz`: process liveness, no sensitive metadata.
- `GET /readyz`: authenticated, requires every configured model to be ready.
- `GET /v1/evaluators/<name>/readyz`: authenticated model readiness and hashes.
- `GET /v1/evaluators/<name>/status`: authenticated operational state and bounded counters.
- `POST /v1/evaluators/<name>/evaluate`: authenticated evaluation.

The request has exactly `version:1`, numeric-string `id`, `text`, `joined`,
`timeout_ms`, `evaluator`, `artifact_sha256`, `code_sha256`. The model name must match
the URL and the hashes must match the deployed model. Reply contains exactly version,
matching ID, score, evaluator and both hashes. No identities, groups, user virtual
keys, OIDC tokens or provider secrets are sent. Sandhi owns identity, permissions,
budgets, policy effects and receipts.

Input is bounded to 2 MiB JSON and 256 KiB per text view. Deadline is 1..2000 ms,
further limited by each model configuration (default 200 ms). Auth/validation precede
execution. Errors contain canonical codes, never exception strings or input text.
The HTTP deadline starts at Flask handler entry; Sandhi independently enforces the
full client-side deadline including connection, send and response. A remote clock
cannot guarantee immediate cancellation at the caller's exact deadline.

## Sandhi owns replica routing

Set `SANDHI_POLICY_REMOTE` to a reviewed copy of `.runtime/sandhi-remote.json` and
`SANDHI_POLICY_CONFIG` to the sample policy (plus `SANDHI_STORE`). List each replica's
base URL and private service-key file. For a multi-model service, use its **shared**
auth file for every model entry. Add `ca_file` for a private CA (one PEM certificate)
and `identity_file` for mTLS (PEM client certificate chain + private key, mode 0600).
Non-loopback endpoints require HTTPS. There is no TLS verification bypass.

Consumers continue calling their usual Sandhi URL. Sandhi selects an available
replica using bounded round-robin routing, connection reuse and one transport slot
per replica. Up to four replica slots total are allowed across remote evaluators.
A failure excludes that replica for one second; the next admitted attempt is a
probe. Health is passive, not an active background readiness poll. Requests already
sent are never replayed or redirected, including HTTP retry middleware. No arbitrary
URL, service key, fallback model or load-balancer setting is accepted from users.
Local denials/incomplete checks prevent remote text egress before provider dispatch.

HAProxy, Kubernetes Services or an existing ingress may be used operationally, but
are optional for this integration. Do not enable automatic POST retries or unbounded
queues in a proxy. An endpoint behind a load balancer must serve only the exact
configured model/code revision. Sandhi's own HA address and shared budget/rate ledger
are separate infrastructure concerns; evaluator replication does not solve them.

## Tests included

Conformance tests cover IPC/HTTP parity, authentication, malformed/oversized requests,
model hashes, crash/hang/nonfinite output, bounded concurrency, replacement/reaping,
secret environment isolation, private files, one-listener model routing and TLS
configuration constraints. Replace/extend marker golden tests for the real model;
passing infrastructure tests does not establish detection accuracy.

From the Sandhi repository, `python scripts/smoke_policy_evaluator.py --sandhi "$PWD"`
builds a disposable generated project, starts two TLS replicas (one requiring mTLS),
checks two model routes/listener, verifies actual Rust remote/local integration, and
checks graceful child cleanup. It uses only temporary synthetic credentials and text.
CI runs this smoke and template tests whenever template/generator/smoke or Rust code
changes. No deployment or production credentials are needed.

## Operational status and recovery

The authenticated per-model `/status` route reports version, model/code hashes,
capacity, ready/busy/recovering/unavailable slots, and state (`ready`, `saturated`,
`recovering`, `unavailable`, `closed`). It includes aggregate completed, failed,
restarted and rejected counts. No prompts, findings, request IDs, paths, PIDs, credentials
or exception strings are included. Counters are fixed-cardinality, saturating signed
64-bit values, in-memory and reset on service restart. Snapshots are advisory; admission
still acquires the actual slot. Poll from trusted operations tooling, not user clients.

`completed_total` counts successfully completed model jobs, not HTTP successes; a
caller can time out while the worker finishes. `failed_total` counts admitted jobs
that fail IPC, validation or their deadline. `restarts_total` counts replacement
attempts. `rejected_total` counts validated work rejected before admission (expired,
stopped, unavailable or saturated); HTTP authentication/format errors are excluded.
The status route returns HTTP 200 even for an unavailable model so operators can read
the cause category; use `/readyz` for readiness status codes. Global readiness requires
all models, while healthy model routes remain independently usable after another
model exhausts its three replacements. A restart is required to restore an exhausted
slot; there is no automatic unlimited restart loop.

The supervisor/worker release references to completed request text before idle waits
or replacement warmup. This reduces unnecessary retention; it is not secure memory
erasure and cannot constrain caches inside trusted model code.

The smoke also runs a fixed 64-request/eight-client synthetic TLS load and records
accepted/rejected counts separately, aggregate latency, and sampled process CPU/RSS.
It asserts that process count stays fixed and that successful scores remain correct.
The fixture uses fresh HTTP connections and is not comparable to warmed pooled-client
benchmarks or real NLP/GPU workloads. OS samples are observations, not cgroup limits.
Replica regression tests cover cooldown recovery without replay and saturated replica
pools rejecting overflow without queuing.


For immutable deployment snapshots, pinned images, explicit internal/published network
profiles and tested Linux CPU/memory/PID enforcement, see [container deployment](DEPLOYMENT.md).
