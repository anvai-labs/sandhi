# Identity, groups and delegated agent credentials

The verified current Kanidm domain and origin are `id.anvaiops.com` and
`https://id.anvaiops.com`. Its existing Sandhi discovery document advertises
`https://id.anvaiops.com/oauth2/openid/sandhi`, client `sandhi`. This is the intended
authority for the migration. The running gateway still has the older
`https://sso.singh.local:8443/oauth2/openid/sandhi` configuration; it has not been
silently switched. A realm reset invalidated old signing-key provenance.

Issuer is part of identity. Verify the current registration's public/confidential
class, exact callback, scopes, group claims and current subject UUIDs before
switching the gateway. An existing client must not be deleted/recreated merely
because old credentials fail. New issuer trust is an explicit migration, never
an availability fallback. Both authority URLs must not be treated as aliases.

## Ownership

| Concern | Authority |
| --- | --- |
| Accounts, login, MFA, membership, account disabling | Existing Kanidm/OIDC provider |
| Identity key | Verified `(issuer, sub)`; never email or caller headers |
| Group-to-role and group-to-inference rules | Sandhi's server-side OIDC configuration |
| Agent credentials | Sandhi virtual-key store, linked to verified owner and grant |
| Budgets, rate limits, alerts, allowed upstream/models | Sandhi enforcement ledger and grant policy |
| OpenAI subscription credential | Gateway vault and one login/refresh owner |

This adds policy bindings and credential metadata, not another user/group
directory. Group membership is not accepted from request headers or from an
unverified JWT. No application infers administrative authority from successful
login alone. Roles remain deployment-wide; they are not tenant-isolated views.

## Group policy configuration

Keep existing subject bindings during migration, especially the recovery admin.
Add only verified group claim values. `groups` has the same binding shape as
`subjects`: an optional `role`, `allow_diagnostics`, and named inference `grants`.
Roles combine by their existing privilege ordering. Grants with the same name
must be identical across applicable bindings; conflicting definitions deny access
rather than depending on hash-map or membership order.

```json
{
  "group_claim": "sandhi_groups",
  "group_source": "userinfo",
  "allow_delegated_keys": true,
  "groups": {
    "sandhi-accounting": {"role": "viewer", "allow_diagnostics": true},
    "sandhi-budget-operators": {"role": "operator"},
    "sandhi-subscription-owner": {
      "grants": {
        "subscription": {
          "upstream": "openai:victor-subscription-20260926",
          "models": ["gpt-6-astra"],
          "group": "subscription-owner",
          "budget_scope": "group:subscription-owner",
          "rate_limit_per_min": 10
        }
      }
    }
  }
}
```

Merge this shape with the existing issuer/client/callback/CA configuration; it
is not a complete replacement file. The subscription-owner group must represent
the authorized account owner and their clients, not unrelated users sharing an
entitlement. `operator` may manage budgets and alerts but cannot mutate credentials
or infer without a grant. Group management stays in Kanidm. Configuration changes
currently require a controlled gateway restart.

`group_source=introspection` (default) reads the named claim from an active,
issuer/audience/expiry-validated token introspection response. `userinfo` first
performs those checks, then fetches the discovered same-origin HTTPS UserInfo
endpoint with that access token. Its `sub` must exactly match introspection.
Only the configured group claim is consumed; UserInfo cannot replace issuer,
audience, subject or expiry. Arrays of bounded strings are required; malformed
claims fail closed. Missing membership grants no group-derived authority.

Kanidm supports custom claims mapped from groups, with an array join strategy.
Before enabling a binding, verify the claim in the actual UserInfo response for
the actual human/workload identity. Discovery and an administrator's login are
not evidence that another account has this claim. See
[Kanidm custom claim maps](https://kanidm.github.io/kanidm/stable/integrations/oauth2/custom_claims.html).
On September 27, both replacement brokers completed real public-HTTPS token
exchange, introspection and UserInfo checks against `id.anvaiops.com`. The member
has only `sandhi_agents`; accounting has only `sandhi_viewers`. The restored
service accounts have new UUIDs, recorded in the private DS3 identity mapping.
Do not alias their workload subjects to a human identity or reuse old UUIDs.
The previously invalid bootstrap tokens remain unsuitable for the new authority.
Membership removal/freshness and browser callback acceptance remain deployment
gates; successful group issuance alone does not establish them.

## Delegated virtual keys

OIDC mode rejects legacy virtual keys by default. With explicit
`allow_delegated_keys=true`, a valid OIDC **access token** may call:

```
POST /auth/keys
Authorization: Bearer <OIDC access token>
Content-Type: application/json

{"grant":"subscription","models":["gpt-6-astra"]}
```

The gateway authenticates the token, resolves its current verified group/subject
policy, and derives the owner, upstream, group, budget and rate limit itself.
Callers cannot supply another subject, issuer, upstream, budget scope or role.
Models may only narrow the grant. The response contains the key once, its public
ID, subject and expiry; it is `no-store`. SQLite retains only the secret hash
and an atomic delegation record (issuer, subject, group evidence, grant, expiry).
The record is credential provenance, not a second membership authority.

By default, each key expires within 120 seconds and never after the source access token.
An explicit `ttl_seconds` from 1 through 120 may shorten that lease.
Delegated keys cannot mint another key or authorize administrative requests.
Explicit revocation is checked from durable state on each request. Restart
retains the same expiry and policy checks; a metadata-free legacy key is denied.
Current server policy is re-evaluated; a removed grant or changed upstream/budget
binding requires fresh issuance. Issuer changes invalidate old delegations.

Direct OIDC access and every delegated key for the same owner/grant share the
same rate bucket and budget scope. Minting more keys does not reset either.
Usage retains each delegated key's public ID, alongside its authoritative owner
and group. The gateway currently enforces one selected budget scope per grant;
simultaneous organization + group + user caps require a separate ledger extension.
Alerts attach to that same scope. Their delivery remains the existing best-effort
mechanism, not a durable guaranteed notification channel.

## Long-running clients: choose renewable OIDC or durable user keys

For group-driven access, prefer a renewable OIDC `TokenCredential`. Victor's
OpenAI gateway path accepts that protocol directly, or an `oidc` configuration
pointing to a private access-token file maintained by an external broker. It
re-reads the file before each model request, pins issuer/audience/subject, rejects
expired or malformed output, and retires handles holding old tokens. It never
refreshes the upstream OpenAI subscription credential. The broker owns OIDC
login/refresh and must publish atomically; the file reader does not perform login.
See Victor's `docs/architecture/sandhi-subscription-gateway.md` for the contract.

For an unattended client that needs a durable virtual key, explicitly set
`max_subject_key_ttl_seconds` in the OIDC configuration (default `0`, disabled;
maximum `31536000`, one year). Choose a shorter operational limit, e.g. `2592000`
for 30 days. Keep `allow_delegated_keys=true`. Then an authenticated owner calls:

```json
{"grant":"subscription","models":["gpt-6-astra"],"ttl_seconds":2592000}
```

A requested lifetime above 120 seconds uses **only an explicit `subjects[sub]`
inference grant**. Group-only membership cannot create durable authority. This
is a deliberate service grant to the verified user, not a permanent copy of
directory membership. Group claims are discarded from this key's delegation
metadata. The key may outlive the issuing login token; it never grants dashboard
roles, admin access, or the right to mint another key.

On every request the gateway checks durable revocation and expiry, the configured
issuer and subject, and the current explicit grant. Deleting that subject/grant,
changing its upstream/group/budget binding, disabling delegated keys, or setting
`max_subject_key_ttl_seconds=0` denies it after configuration reload (currently
restart). Model and rate constraints can only narrow. Changing a nonzero lifetime
limit affects new issuance; it does not rewrite existing expirations.

**Disabling an IdP account or removing its groups does not revoke this independent
service grant.** Revoke its public key ID through the authenticated admin key API
and remove the subject grant for immediate local response. Use renewable OIDC
when directory revocation must govern access. There are no nonexpiring keys in
this issuance path. Rotation means issue a replacement, switch the private client
credential, verify it, then revoke the old public ID. Rotating does not reset the
shared owner/grant budget or rate bucket. Revocation and owner mapping persist
across gateway restart.

## Membership freshness and failure

Existing delegated keys retain their verified membership evidence until their
short lease expires. Renewing requires a new authenticated exchange and policy
resolution. **The IdP's own freshness behavior still matters:** if it returns
old group claims for an active token, membership removal may not become visible
until that token expires/revokes. The gateway cannot manufacture live directory
freshness from an old token. Bound upstream token lifetimes and test removal at
the real IdP. Immediate incident response is local key revocation or grant removal.
Group-derived browser sessions are capped at 120 seconds and the source token's
expiry; they require a new login after expiration. IdP backchannel logout and
automatic browser-session renewal are not implemented.

Unknown identities, invalid audience/issuer, ambiguous credentials, missing grants,
expired delegation, unavailable required identity data and conflicting policies
fail closed. No authentication failure switches to legacy token mode. Keep a
controlled recovery admin binding; do not create a second password database.

## Validation and remaining work

TDD covers group-based inference, attenuated key issuance, shared budget exhaustion,
alert firing, model denial, inability to delegate again or administer, explicit
revocation, wrong issuer/subject, expiry, conflicting/malformed groups, key-level
attribution, shared rate identity and UserInfo subject mismatch. Durable-key
tests cover lifetime limits, group-only denial, persistence/reopen, revocation,
subject-policy removal and global disable. Victor tests exercise rotated tokens,
stale tokens, private-file permissions, symlinks, identity pinning, redacted
acquisition failures, and member-agent credential propagation. Existing signed
OIDC login, CSRF, roles, discovery and direct-provider suites remain regression gates.

Deployment must additionally verify actual Kanidm group claims, member removal,
agent key renewal, restart, and operator recovery against the configured issuer.
The source changes are a reviewable candidate, not evidence of a live migration.
Opt-in prompt inspection and metadata audit now have a [candidate MVP](policy-evaluation.md).
Redaction, richer audit workflows and group administration UI remain deferred. Agent tool/egress
permissions remain distinct from model access; no group grants arbitrary tools.


### Current authority migration gate

The active DS3 domain/origin and public discovery agree on `id.anvaiops.com`.
Both administrator accounts were recovered through Kanidm's supported local
recovery command, verified with fresh authenticated logins and exact built-in
account UUIDs, then atomically reconciled into owner-only DS3 credential files.
Private prior-file backups and sanitized receipts were retained. No realm reset
or wholesale client re-registration occurred.

The existing `sandhi` public client and PKCE remain intact. Its callback is still
`https://sso.singh.local:8444/auth/callback`: this is the gateway hostname, not the
IdP issuer, and needs browser acceptance before changing it. Missing broker
service accounts were recreated with explicit new identities and least-privilege
group memberships; custom `sandhi_groups` array claims were restored. Read-only
30-day bootstrap credentials feed brokers returning 15-minute OIDC access tokens.
Those bootstrap credentials need supervised rotation; they are not nonexpiring.

The real public path initially rejected the default Python User-Agent via
Cloudflare Browser Integrity Check. A truthful application User-Agent resolved
it without weakening TLS or Cloudflare rules. Sandhi now identifies its OIDC
HTTP client too; a red/green test covers all authority requests.

These checks repair identity issuance. The pre-existing gateway still uses the
old issuer configuration; no running gateway has been replaced. Stage the new
issuer and exact UUIDs together, verify acceptance and rollback, then cut over.

### Extensible content policy

The [policy MVP](policy-evaluation.md) selects rules using verified subjects,
groups and assigned roles, and implements regex, numeric threshold and lexical
similarity evaluation before provider dispatch. Audit receipts are mandatory;
block/quarantine deny dispatch. A missing group claim is unknown, not an empty
membership. Durable subject keys do not gain directory roles/groups by inference;
a matching rule requiring directory facts denies admission when they are unknown.

[TD-0005](../td/TD-0005-declarative-policy-engine.md) retains the broader target
contract. [Python workers/offline MLflow](python-ml-evaluators.md) are now implemented
candidates behind the same verified identity selection, alongside the optional
[embedded ONNX profile](embedded-onnx.md). Remote models,
embedding similarity, encrypted payload quarantine/release and signed distribution
remain proposed.
The candidate has passed isolated live acceptance; production is unchanged.
