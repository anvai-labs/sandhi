# ADR-0016: Explicit backend endpoint binding

Date: 2026-11-11

## Status

Accepted for the TD-0034 M2a implementation candidate. The typed factory seam is
implemented in the workspace. [ADR-0017](0017-backend-registry-lifecycle.md)
adds M2b operator persistence and activation; merge/deployment/release are separate gates.

## Problem

The gateway inferred protocol entirely from provider slug, except that OpenAI
`oauth` selected the constrained Codex route. A standard Responses backend could
be built through the provider runtime but not the gateway factory. A custom
Anthropic-compatible provider name selected OpenAI Chat by default. A provider
label, credential kind and endpoint protocol are independent facts.

Generic `oauth` on non-OpenAI providers also fell through to API-key behavior;
this neither implemented a credential lifecycle nor accurately described the wire
authentication. The gateway's Gemini factory ignored an explicit Bearer scheme.

## Decision

`BackendEndpoint` declares `endpoint_family` using the existing neutral
`EndpointFamilyV1` and an `endpoint_profile` of `standard` or `chatgpt_codex`.
Unknown fields and values are rejected. Family is required; only profile defaults
to standard. This is non-secret endpoint metadata, not a grant or a credential.

`build_provider_handle_for_endpoint` validates the binding independently of the
provider label before constructing the transport. It supports implemented Chat,
Responses, Anthropic, Gemini, Cohere and Ollama transports; parser-only Bedrock and
host callbacks are refused. Unsupported combinations return the factory's existing
invalid-configuration result without making a request or mutating a vault.

`build_provider_handle` remains the legacy wrapper: supported API-key/Bearer
families retain provider-name defaults, and OpenAI OAuth retains its constrained
Codex default. Explicit selection must not be inferred from an arbitrary URL or
introduced as a provider-name suffix convention.

The Codex profile requires Responses family and OAuth scheme. The existing strict
access-only lease envelope, fixed origin, pinned account header, expiry fence,
retry-free transport and disabled raw forwarding remain mandatory. An explicit
standard profile cannot turn the same OAuth lease into an ordinary API token.
An explicit Codex binding may carry a different operator provider label; that
label does not alter the profile's fixed origin or account constraints.

Standard profiles accept API-key or externally supplied Bearer credentials.
`oauth` requires an implemented lease profile; generic non-OpenAI OAuth is now
rejected rather than sent as an API key. A configured API key must use `api_key`;
an externally managed bearer must use `bearer`. This is a compatibility correction
for previously misinterpreted metadata, not an Anthropic subscription implementation.
Gemini Bearer uses its existing Bearer transport rather than `x-goog-api-key`.

Backend registry identity, credential reference, account scope, model capabilities,
route authority and persistence are separate concerns in TD-0034. ADR-0017 builds
admin/config and restart reconstruction on this seam, validating before storage.
Native-family
transport slugs continue their existing semantics until backend identity is recorded
separately with the dispatch plan and usage evidence.

## Validation

`tests/backend_endpoint.rs` exercises explicit family selection across provider
labels, strict metadata decoding, invalid profile/auth/family combinations,
unchanged supported legacy defaults, subscription origin/lease/expiry constraints,
and actual native Responses/Anthropic/Gemini paths with Bearer authentication.
The existing subscription suite remains required. These are synthetic local tests;
no provider account or external secret backend is contacted.

## References

- [TD-0034 goal and constraints](../td/TD-0034-protocol-neutral-router-cache-accounting.md)
- [Initial inventory](../reviews/router-cache-inventory-2026-11-11.md)
- [Subscription constraints](../subscription-gateway.md)
- [ADR-0004 forwarding planes](0004-two-plane-proxy-and-enforcement-boundary.md)
