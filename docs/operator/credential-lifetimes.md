# Authentication credentials, virtual keys and lifetimes

Sandhi separates **who or what authenticated**, **what inference authority was
granted**, and **how long a credential may be used**. Keeping those decisions
separate lets an interactive person use short-lived identity tokens while an
unattended client uses a scoped, revocable virtual key without creating a second
authorization authority.

This guide gives product-level recommendations. It does not describe any particular
deployment, identity-provider tenant, hostname, account, or private audit record.

## Choose one authority per gateway

| Profile | Use it when | Client credential | Inference authority |
| --- | --- | --- | --- |
| In-process library | One trusted process needs Sandhi's routing, translation, or metering without a network listener | Caller-owned provider configuration | Caller-owned policy |
| Shared starter | A local, single-trust-realm experiment needs the quickest safe start | No client credential | One shared server-owned identity and budget |
| Token mode | An isolated compatibility deployment must use static client credentials | Client secret exchanged for a short-lived virtual key | Sandhi virtual-key policy |
| OIDC mode | People or services have a qualified identity provider | Access token or OIDC-delegated virtual key | Verified issuer/subject plus a Sandhi grant |

Do not run a second token-mode gateway beside an OIDC gateway for the same users,
models, budgets, and enterprise provider credentials. That splits revocation,
rate limiting, budget enforcement, and audit history. If token-shaped credentials
are needed in OIDC mode, use OIDC-delegated virtual keys.

## Credential ladder

| Credential | Typical lifetime | Who owns renewal | Security boundary |
| --- | ---: | --- | --- |
| Browser session | About one hour | Person signs in again | Dashboard only; not an inference credential |
| OIDC access token | Minutes, commonly 5–15 | Identity client or credential broker | Authenticates a person/service to Sandhi |
| Short delegated virtual key | 1–120 seconds | Client mints another after OIDC authentication | Task- or request-scoped inference |
| Durable delegated virtual key | Hours to days | Operator-approved rotation procedure | Long-running client with explicit subject grant |
| Token-mode client secret | Long-lived, privately stored | Operator rotates the static registration | Exchange credential; not sent to the provider |
| Token-mode exchanged virtual key | 15 minutes recommended; one hour maximum | Client re-exchanges | Scoped compatibility inference credential |
| Upstream provider credential | Provider-defined | Gateway operator | Never exposed to callers |

Long-lived does not mean non-expiring. A service's private exchange secret can
remain stable while the credentials it produces remain short-lived. Rotate the
exchange secret on a documented schedule and immediately after suspected exposure.

## Lifetime policy

| Client class | Recommended policy |
| --- | --- |
| Interactive dashboard | Browser session only; no durable virtual key |
| Interactive agent | Renewable OIDC access token; use a short delegation only when a tool cannot pass the token directly |
| Batch or CI job | OIDC access token for the job duration, or a delegated key lasting no longer than the job |
| Long-running service | Renewable access token first; otherwise a 24-hour delegated key with automated rotation |
| Compatibility token-mode client | Re-exchange every 15 minutes and cap issuance at one hour |
| Recovery or break-glass client | Separate identity, explicit grant, short TTL, and immediate revocation after use |

Start with the shortest lifetime that satisfies availability and retry behavior.
Increase the configured ceiling only after renewal, alerting, revocation, and
rollback have been exercised. The implementation permits longer ceilings, but a
one-year key is not a recommended operational default.

## Renewal, extension and rotation

```mermaid
flowchart LR
    A[Private identity secret] -->|Authenticated exchange| B[Short-lived access token]
    B --> C{Client need}
    C -->|Per request| D[Direct access token]
    C -->|Task or service| E[Delegated virtual key]
    E --> F[Expiry or rotation event]
    F -->|Re-authenticate and mint| G[Replacement key]
    G --> H[Switch private client config]
    H --> I[Verify inference and accounting]
    I --> J[Revoke old public key ID]
```

- A signed access token is never edited, replayed into the future, or accepted
  after its verifier reports expiry.
- A virtual-key expiry is fixed when the key is minted.
- Raising a configured maximum affects only newly issued keys.
- Extension is performed by minting a replacement, switching the private client
  configuration, verifying the request/accounting join, and revoking the old key.
- The replacement shares the owner/grant budget and rate bucket; rotation does
  not reset consumption.

For one identity exposing multiple independently metered models, use a
model-derived budget template rather than separate client-visible grants. The
caller selects a logical model such as `coding-fast` or `coding-reasoning`;
Sandhi derives the matching model budget scope after route selection. A warning
or block applies to the selected model without consuming the other models'
allowances.

## OIDC delegated keys

OIDC mode does not accept ordinary virtual keys retained from a token-mode
deployment. Delegation is opt-in and disabled by default.

A short-lived delegated key:

- is issued only after Sandhi validates an access token;
- cannot outlive the originating access token;
- can narrow the model allowlist, but cannot choose another subject, issuer,
  upstream, budget, role, or administrative permission;
- cannot mint another key;
- is checked for revocation and current Sandhi policy on every request.

A durable delegated key can outlive the login token that issued it only when all
of the following are true:

1. Delegation is explicitly enabled.
2. A nonzero lifetime ceiling is configured.
3. The verified subject has an explicit direct inference grant.
4. The grant names the upstream, models, attribution scope, budget scope, and
   rate limit.
5. The operator has a tested revocation and rotation procedure.

Group-only authority must not create a durable key. Disabling the identity-provider
account or removing a group does not by itself revoke an independent durable
service grant: remove the grant and revoke the public key ID when directory state
must govern access immediately.

## Example policy shape

The following values are illustrative, not a copyable deployment configuration.
Use your own verified issuer, client, subject, route, model, and budget names.

```json
{
  "issuer": "https://identity.example.com",
  "client_id": "SANDHI_CONSUMER_CLIENT_ID",
  "redirect_url": "https://sandhi.example.com/auth/callback",
  "access_token_audience": "https://sandhi.example.com/resource",
  "allow_delegated_keys": true,
  "max_subject_key_ttl_seconds": 86400,
  "subjects": {
    "VERIFIED_SUBJECT": {
      "role": "viewer",
      "grants": {
        "coding": {
          "upstream": "route/APPROVED_ROUTE",
          "models": ["APPROVED_MODEL"],
          "budget_scope": "service/APPROVED_BUDGET",
          "rate_limit_per_min": 30
        }
      }
    }
  }
}
```

Keep the file private. A client secret belongs in a service-manager credential
or secret store, not in this document, source control, shell history, or logs.

## Token-mode client credentials

Token mode is for an explicit compatibility deployment. It exchanges a registered
long-lived client secret for a gateway-opaque, scoped virtual key.

- The registration file is private and validated at startup.
- A nonempty model allowlist is mandatory.
- Requested lifetime is clamped by the registered client and the one-hour server ceiling.
- A zero TTL is invalid.
- Unknown clients, wrong secrets, and throttled attempts return an identical failure.
- The static secret never becomes the upstream provider credential.

This client-credentials registry is intentionally not combined with OIDC mode.
OIDC deployments use delegated keys so every `vk_...` inference credential has
issuer, subject, grant, expiry, revocation, and accounting provenance.

## Operator checklist

| Gate | Required evidence |
| --- | --- |
| Identity provider | Exact issuer, HTTPS discovery, signing keys, token/introspection contract, and supported client flow |
| Gateway listener | Trusted TLS certificate and readiness check; no plaintext credential listener |
| Registration | Separate origin and consumer clients, exact callback and resource audience |
| Authorization | Explicit role and inference grant; no role implies inference |
| Credential issue | Correct subject, models, budget, rate limit, expiry, and one-time secret handling |
| Inference | Allowed model succeeds through the intended client path |
| Accounting | Request identifier joins usage, budget, cache split, and provider/source evidence |
| Expiry | Expired access token and virtual key are refused without provider fallback |
| Revocation | Revoked key, removed grant, disabled delegation, and restart preserve denial |
| Rotation | Replacement key works, old key is revoked, and budget/rate continuity is observed |
| Recovery | A separate recovery administrator can regain access without weakening TLS or authentication |

Record artifact identities and acceptance results in private deployment evidence.
Public documentation should describe the product contract and safe procedure,
not private hostnames, accounts, issuers, credentials, or raw audit transcripts.
