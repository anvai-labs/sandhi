# OpenAI subscription gateway: Victor / sandhi co-design

Status: implemented on the subscription-gateway branches; release promotion remains gated.

## Ownership and request path

```
Victor client A -- virtual key A --+
                                  +--> sandhi --> subscription Responses backend
Victor client B -- virtual key B --+       ^
                                          |
Codex login owner --> access-only lease publisher --> OS credential vault
```

A gateway configuration is a complete authentication mode in Victor. It takes
precedence over `auth_mode=oauth`, explicit upstream keys, environment keys and
local credential resolution. Victor neither reads nor refreshes an OpenAI grant
in this mode. It sends ordinary Chat Completions with its virtual key to the
configured HTTPS or literal/localhost loopback HTTP gateway. There is no direct
upstream fallback. Invalid/missing gateway credentials fail before discovery.

Sandhi's `openai` vault entry with `scheme=oauth` contains a strict JSON envelope:
`access_token`, `account_id`, `expires_at` (Unix seconds). Unknown fields,
including refresh tokens, are rejected. The upstream is fixed to
`https://chatgpt.com/backend-api/codex`; an arbitrary override is rejected before
vault mutation. The handle declares Responses family explicitly and uses the
ChatGPT/Codex constrained codec: SSE even for aggregated completions,
`store=false`, system/developer instructions, and supported parameter mapping.
Raw forwarding is disabled so native Responses requests cannot bypass these
constraints or expiry checks. A caller cannot replace the vaulted account header.

## Login, rotation, revocation and expiry

The existing Codex process remains the sole writer/refresh owner for its grant.
`scripts/subscription_lease.py` reads an owner-only regular cache file without
following symlinks. The operator pins the expected account. The publisher copies
only the access token to sandhi's OS vault and limits the lease to five minutes
or the token's own expiry, whichever is earlier. The cache is never rewritten.
JWT expiry decoding is scheduling, not signature verification; OpenAI validates
the bearer. Keyring-only Codex installations need a separately supported export
or credential broker; do not silently downgrade keyring storage to a plaintext
refresh-token copy.

Renew the lease every minute. Every typed dispatch checks expiry with a 30-second
margin, including streaming and observed-attempt paths. The lease remains fenced
after gateway restart; expired vault entries do not yield an active handle.
In-flight requests may finish after expiry. Login disappearance/account change,
publisher failure or expired access token stops *new* dispatch at the deadline.
Run `codex login` to renew the login when needed; the publisher never performs
an undocumented refresh flow. An independent gateway-owned OAuth grant could be
added later, with its own single writer and persisted rotation protocol. Sharing
one refresh token between Victor, Codex and sandhi is rejected as a design.

Revoking a client virtual key stops future admission for that client. Revoking
the gateway vault entry stops local route dispatch, but does not revoke the
OpenAI grant. To revoke upstream access, use the account's session controls and
stop the publisher; otherwise it would republish. Admin credentials never go to
Victor. A missing/invalid token is not grounds to try a different account/provider.

## Per-client authority and accounting

Provision one virtual key per client, bound to an immutable subject, explicit
model allowlist, rate limit, expiry and blocking daily token budget. Use a shared
owner group for attribution; each key's budget scope is independent. The gateway
rejects forged subject/group headers. All clients still share the account's real
subscription quota and limits; virtual keys do not create new entitlement,
independent subscriptions, or authorization to share/resell account access.

Store key material only in private client files or a credential broker. Usage
records contain attributed neutral token units and provider-reported usage;
subscription requests are not priced as ordinary Platform API-key requests.
Errors must not expose tokens, cache contents or upstream response bodies.
Subscription upstream transport retries are disabled in this route; callers
must distinguish pre-dispatch failure from ambiguous inference completion.
Agent/tool side effects require their own approval and idempotency ledger.

## Provisioning and client configuration

Build the matching gateway branch. Bind loopback with `SANDHI_BIND`, private
`SANDHI_STORE`, `SANDHI_ADMIN_TOKEN`, `SANDHI_VAULT_BACKEND=keyring`, and a unique
vault label. Keep admin/dashboard private. `subscription_lease.py --help`
documents publisher and scoped client provisioning. Admin JSON contains
`admin_token`; credential outputs use exclusive creation and never print keys.
A failed key-output write requires inventory reconciliation, not blind minting.

Victor profile provider kwargs:

```yaml
provider: openai
model: <model explicitly allowed on the virtual key>
# gateway is passed in provider kwargs; a profile can use env resolution below
```

Set `SANDHI_GATEWAY_URL` and `SANDHI_GATEWAY_VIRTUAL_KEY_OPENAI` only for that
client process, or pass `gateway={url, virtual_key}` to `SandhiOpenAIProvider`.
Do not set `OPENAI_API_KEY` to the subscription access token. Do not let global
routing settings silently move message-hub's private triage to a cloud provider.

## TDD and acceptance gates

Red tests first demonstrated local OAuth discovery in gateway mode, acceptance
of malformed/expired OAuth secrets, wrong protocol selection, and mutable
account headers. Green tests cover those boundaries, complete/stream expiry,
access-only publication, filesystem restrictions, account mismatch, and two
independent virtual-key clients through a mocked subscription SSE upstream.
The end-to-end test checks model denial, forged attribution, gateway-only bearer
replacement, fixed account identity, mandatory codec fields and per-client usage.
Existing provider/proxy suites and Victor direct OAuth tests remain required.

Live acceptance uses synthetic prompts only: both clients must succeed with
separate attributed records; denied models/spoofing must never reach upstream.
Test publisher/gateway restart and expiry; then configure supervision and record
binary/source provenance. Do not claim a live result from mocked conformance.

## Evidence and limits

[Official Codex authentication documentation](https://developers.openai.com/codex/auth/)
distinguishes ChatGPT login from separately billed Platform API keys and describes
cached credentials as sensitive. This route reuses the existing supported-in-code
subscription Responses adapter; it is not a claim that ChatGPT login is an
ordinary `api.openai.com` API key, or a provider guarantee for arbitrary workloads.
Account entitlements and backend behavior remain external acceptance gates.


## Identity-first follow-up (2026-09-26)

The configured local OIDC issuer remains authoritative. Group policy and short-lived
delegated virtual keys are implemented on this feature branch; no live migration is
claimed. Legacy token-mode subscription provisioning is superseded for this deployment.
See [identity and group ownership](operator/identity-groups.md) for the tested mapping, shared
budget/rate semantics, membership freshness limits and pending live acceptance.

### Private gateway certificate authorities

The provider transport loads native trust roots in addition to WebPKI roots. Set
`SSL_CERT_FILE` to a PEM CA bundle in the client process when using a private
HTTPS gateway. This also works in the Python binding used by Victor; setting
Python/httpx trust alone did not configure its Rust transport. Keep hostname
verification enabled and include the actual gateway hostname in the certificate.
Public WebPKI roots remain available. This is process-wide transport trust, not
an operating-system trust-store installation or a per-provider CA pin.

Real TLS regression tests cover a trusted private CA, an untrusted CA and a
hostname mismatch. Already installed wheels must be rebuilt/replaced to pick up
this feature; a configuration variable cannot change an older wheel's feature set.


## Model plus reasoning effort

The foundational request already represents these independently:
`ChatRequestV1.model` and optional `ChatRequestV1.reasoning_effort`. Clients
should supply both explicitly when they need a stable reasoning default:

```json
{"model":"gpt-6-luna","reasoning_effort":"medium","messages":[{"role":"system","content":"Classify the input."},{"role":"user","content":"hello"}]}
```

Both standard and subscription Responses codecs preserve the exact model and
map effort to `reasoning.effort`; typed effort wins over extension duplicates.
No model-name suffix, gateway-global default, or inference from temperature is
introduced. Absent/null delegates to the model default; `"none"` is an explicit
supported effort value. Effort does not select a credential or loosen policy,
model allowlists, rate limits or token budgets.

[GPT-6 Luna](https://developers.openai.com/api/docs/models/gpt-6-luna) and
[GPT-6 Sol](https://developers.openai.com/api/docs/models/gpt-6-sol) document
none, low, medium, high, xhigh and max. This is model-specific metadata, not a
universal scale. The neutral string also accommodates other provider labels.
Compatible chat codecs forward a top-level field; existing native-family
codecs have separate thinking controls and do not all map named effort. Do not
advertise unsupported mappings as working merely because the neutral contract
accepts the field. Gateway consumers should select a verified adapter/model.

Victor now shares a typed effort vocabulary between profile configuration,
immutable session overrides and FEP-0037 classification. Message-hub uses a
Luna-only key with a separate budget for WhatsApp/Facebook and retains the
existing local route for other channels. Authentication, contract discovery
and schema pinning precede classification; policy denials remain terminal.
A codec matrix tests both models and all six documented levels under both
Responses profiles. No gateway binary change is needed for this forwarding.
