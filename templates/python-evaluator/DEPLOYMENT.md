# Resource-limited evaluator deployment

This optional Linux/cgroup-v2 profile packages the existing multi-model service. It
creates one container/listener per evaluator server, not one per model. Sandhi retains
identity, policy, budgets and replica routing. No load balancer is required by this
profile. The server image and model bundle hashes are separate provenance values.

## Build and review

From your generated evaluator project, build the image using the checked-in Dockerfile
and `requirements-container.lock`. The base image is pinned by digest; dependencies
are pinned by version and distribution hashes. Build uses binary wheels only. The
`.dockerignore` allowlist excludes artifacts, development state, certificates and keys.
No secrets are copied into the image. Add model-library dependencies to your own lock
and run its golden/parity tests; the example image contains only the marker evaluator's
HTTP/runtime dependencies. All models in this image use `/usr/local/bin/python`.

```bash
docker build --iidfile /private/path/evaluator-image.id .
```

The image ID file contains an immutable local image reference. For another machine,
publish through your reviewed release process and use `repository@sha256:...`; a local
image ID is not a portable registry manifest digest. No image is pushed by the generator
or smoke. Update the Dockerfile base digest deliberately when upgrading Python; rebuild
and retest both architectures you intend to support. The committed lock was generated
for Python 3.12 with universal hashes; it does not lock custom NLP dependencies.

## Prepare a release; do not rebuild it in place

Run as a dedicated non-root deployment user. Its UID/GID becomes the container UID/GID;
its private material must already be owned by that user. `containerize.py` requires an
image digest, checked model manifests, a service key, server certificate/key and client
CA. TLS with client certificates (mTLS) and the service bearer credential are mandatory.
Certificates must contain the DNS names/IPs used by Sandhi; client certificates need
client-auth usage. Do not use the disposable smoke certificates for deployment.

```bash
python containerize.py /absolute/releases/evaluator-r1 \
  --model /absolute/model-a/.runtime/http.json \
  --model /absolute/model-b/.runtime/http.json \
  --image 'registry.example/evaluator@sha256:<64-hex-digest>' \
  --auth-file /private/service.key \
  --cert-file /private/server.crt --key-file /private/server.key \
  --ca-file /private/client-ca.crt
```

The destination must be new. Validation precedes creation: names/worker counts,
model/code hashes, private ownership/modes, TLS key matching and resource bounds.
Verified bundle/artifact bytes are copied into owner-only release directories. Runtime
files are mode 0400 and mounted read-only; source rebuilds cannot change the release.
`deployment.json` records image/model digests and limits, without secret values.
Configuration is immutable by convention and permissions, not cryptographically signed;
the owning operator can still change permissions/files. Protect the release directory
and Docker daemon from untrusted users. Copying a release to another host requires
regenerating its absolute mounts and UID/GID binding for that deployment user.

`compose.json` is JSON accepted by Docker Compose. Image pull policy is `never`: make
the reviewed digest available explicitly on the target before starting. Review the
rendered configuration, then start it through your deployment procedure:

```bash
docker compose -f /absolute/releases/evaluator-r1/compose.json config
docker compose -f /absolute/releases/evaluator-r1/compose.json up -d --no-build
```

These commands are operator actions; generation alone never starts/stops a service.
Rotate keys/certificates or promote models by preparing and reviewing a new release,
updating Sandhi's matching replica manifest and replacing the service. No hot reload,
overlapping-key window, zero-downtime rollout or automatic policy promotion is claimed.
One published host port prevents two releases from binding that same port concurrently;
plan drain/replacement or use separate servers behind Sandhi replica routing.

## Two explicit network profiles

Default `--network internal` creates an isolated Docker bridge and publishes **no host
ports**. A co-located Sandhi container must join that network and use the evaluator
service's TLS name/port. Joining the network is an operator decision; the generator
never attaches an existing container. Do not expect `ports` plus an internal-only
network to provide remote host reachability.

For a remote server, pass `--network published`. It publishes exactly one TLS port,
loopback-bound by default. Use `--bind-ip <specific-server-IP> --port 9088` to expose it
on an explicitly chosen interface. The container still uses one internal listener at
9088, regardless of model count. This uses a normal Docker bridge and **does not deny
outbound network access**. Apply host/network ingress and egress rules appropriate to
Docker forwarding; do not assume a generic host firewall automatically filters Docker
published traffic. mTLS authenticates clients; it is not an outbound network policy.
Neither profile isolates models from other models or secrets in the same container.
They are for trusted model code, not adversarial plugins.

References: [Compose networks](https://docs.docker.com/reference/compose-file/networks/),
[port publishing](https://docs.docker.com/engine/network/port-publishing/), and
[Docker firewall behavior](https://docs.docker.com/engine/network/packet-filtering-firewalls/).

## Enforced host limits

Defaults cover the whole service process tree, including model children:

| Control | Default |
| --- | --- |
| CPU quota | 1 CPU (`--cpus`, allowed 0.25..64) |
| Memory / total memory+swap | 512 MiB / 512 MiB (`--memory-mb`, allowed 128..32768) |
| Processes and threads | 64 tasks (`--pids`, allowed 16..256) |
| Writable scratch | `/tmp` tmpfs, 64 MiB, noexec/nosuid/nodev |
| Shared memory | 16 MiB |
| Root filesystem and model/secret mounts | Read-only |
| Linux capabilities | All dropped |
| Privilege escalation | `no-new-privileges` |
| Core dumps | Disabled |
| File descriptors | 1024 soft/hard |
| Unexpected service restart | At most 3 on-failure retries |
| Graceful stop | 20 seconds, then container process-tree cleanup |

Gunicorn's optional management socket is disabled where supported. Flask still has
one serving process, four HTTP threads and its existing application-level bounds.
Container `init` reaps orphaned descendants; this does not make arbitrary plugin code
safe. Memory constraints count process memory and tmpfs; they do not govern GPU VRAM.
GPU access is not enabled. Per-model fairness/isolation needs additional deployment
work if models cannot share the same trust and resource boundary.

The runtime must actually support these controls. An unsupported rootless/cgroup setup
may reject or ignore limits; inspect and run the acceptance probe before enabling
sensitive workloads. A CPU quota can increase tail latency under load. An OOM kill is
an unavailable evaluation, never permission to forward unchecked. The profile leaves
required evaluator failure behavior fail-closed and does not auto-clear exhausted
worker restart circuits.

[Docker resource constraints](https://docs.docker.com/engine/containers/resource_constraints/)
and [Compose service fields](https://docs.docker.com/reference/compose-file/services/)
describe the underlying runtime controls.

## Actual enforcement smoke

From Sandhi, with template development dependencies, Docker Compose and a local image:

```bash
python scripts/smoke_policy_container.py --sandhi "$PWD" \
  --image 'sha256:<local-image-id>'
```

The smoke generates synthetic TLS material/model fixtures, runs one published service
with two model routes and exercises authenticated mTLS. Inside the disposable service
it checks effective UID, capabilities, cgroup limits, read-only root, noexec scratch,
and actual PID exhaustion followed by successful readiness. Separate credential-free,
network-disabled probes verify CPU throttling at 0.25 CPU and an OOM kill at 64 MiB.
Those probes do not OOM the evaluator service or production processes. Only UUID-named
containers/networks created by the invocation are removed, and cleanup is checked.
Images remain available for inspection/reuse; there is no global Docker prune.

Both internal and published profiles have live acceptance: the internal profile has
no host port binding and passes authenticated TLS readiness inside its isolated
network; the published profile passes the remote-port test. Host and GPU variants still need acceptance on their target runtime.

Hosted Linux CI runs the container smoke automatically. The self-hosted runner lane
keeps the portable conformance/TLS tests and leaves Docker enforcement acceptance as
an explicit target-runtime check; Docker/cgroup support is not assumed on those hosts.
CI does not deploy or push the image.
