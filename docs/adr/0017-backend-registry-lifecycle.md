# ADR-0017: Backend registry and credential lifecycle

Date: 2026-11-11

## Status

Accepted implementation candidate for TD-0034 M2b. Source and synthetic process
validation are tracked in TD-0034. Release and deployment remain separate gates.

## Decision

A backend is non-secret metadata stored independently of its vault credential:
stable ID, provider label, exact credential reference, endpoint family/profile,
optional base URL and User-Agent, and declared models. Its route reference is
`backend/<id>`. Existing `provider:label` routes retain their meaning.

The registry is bounded to 256 definitions, 64 KiB per definition and 128 models per
backend. Unknown definition fields are refused. Model aliases, alternate endpoint
URLs and extensions are refused until executable routing semantics exist.
Declared model IDs constrain admission and discovery; capability declarations do
not yet implement feature negotiation.

Registration validates the profile and active exact credential reference before
reading the secret or committing metadata. An override must share the credential's
declared HTTP(S) origin; user information, query strings and fragments are refused.
Codex's stricter fixed URL, lease/account and expiry checks remain mandatory.
Backend registration reads credentials; it neither creates secrets nor broadens a
broker grant.

Definitions carry monotonically increasing revisions. Identical active create
replays are idempotent. Changes, reactivation and revocation require the current
revision; stale operations fail with conflict. This prevents an ordinary create
retry from silently retargeting existing keys. Host-owned handles occupying the
same route are preserved and registry registration reports conflict.

The provider handle owns an immutable binding snapshot, including the revision and
model declarations. New admissions clone that handle; metadata updates cannot pair
a previously selected transport with new model declarations. This is a foundation
for the full dispatch plan, not yet conversation affinity or attempt attribution.

Credential registration/rotation rebuilds dependent handles, then publishes the
direct credential route and its dependents under the same runtime lock.
Revocation removes both. An unreadable backend index disables dependents and
reports reconciliation instead of retaining old credential material. A specific
invalid, absent or expired credential leaves its backend dormant. Already admitted
requests retain their owned handle and existing dispatch-time expiry checks.

Startup reconstructs registry routes from durable definitions and authoritative
vault references. Invalid metadata is a startup failure; unavailable credentials
do not become an empty successful registry or an implicit fallback account.
Listing distinguishes active metadata from a matching loaded binding (`ready`);
this is not a vendor health or inference probe.

Config apply preflights every backend before credential mutation, then applies
providers, backends and dependent keys/profiles. Writes are additive and component
failures remain explicit; omission does not revoke. A failed dependency cannot
mint a new key or publish a gateway profile through an older backend version.
Persistence and publication occur inside lifecycle-owned blocking operations;
HTTP cancellation does not relinquish the shared writer while a commit continues.

## Limits and follow-up

This implements explicit fixed backend routes. Logical aliases, selection/affinity,
ordered opaque state, cache policy compilation and per-attempt durable attribution
remain TD-0034 M3–M6. A registration does not test provider capability claims or
prove cache hits. Standard Bearer transport does not imply a managed Anthropic
subscription profile.

## References

- [Operator guide](../operator/backends.md)
- [Explicit endpoint binding](0016-explicit-backend-endpoint-binding.md)
- [Goal and acceptance evidence](../td/TD-0034-protocol-neutral-router-cache-accounting.md)
