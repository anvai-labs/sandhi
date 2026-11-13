# Release readiness and integration verification

This page summarizes changes and qualification relevant to release selection.
Use the [published releases](https://github.com/anvai-labs/sandhi/releases) and
[upgrade guide](../releases/v0.17.1.md) for installable artifacts. A source merge
or a passing fixture does not establish a published package or a deployment.

## Current 0.18 access and publication checkpoint

| Area | Qualified behavior | Remaining release gate |
| --- | --- | --- |
| Access profiles | Credential-free local starter; explicit token and OIDC profiles; configured authentication fails closed | Deployment-specific access checks |
| Listener security | Authenticated standalone listeners require TLS; deliberate HTTP exceptions are limited to loopback | Installed artifact and client trust verification |
| Identity and authorization | Identity is verified centrally; Sandhi applies roles, inference grants, model policy and budgets | Combined dashboard/API deployment acceptance |
| Agent interoperability | Actual tool loops across supported request dialects; source usage and durable accounting verified | Official 0.18 SDK adoption and consumer qualification |
| Token accounting | Fresh input, cache read, cache creation and output remain distinct and conserved | Real-provider observations must retain their own provenance |
| Publication | 0.17.1 is published; 0.18.0 remains a source candidate | Reviewed promotion, exact-main CI, immutable tag and artifact verification |

The 0.18 candidate includes optional authentication, explicit compatible endpoint
profiles, typed reasoning support, resource-audience validation and refreshed
runtime dependencies. Consult the [changelog](../../CHANGELOG.md) for feature
changes and the [authentication guide](../operator/oidc-sso.md) for configuration.

## Release boundary

| Version | Status | Installation guidance |
| --- | --- | --- |
| 0.17.0 | Historical tag; publication did not complete | Use a verified corrective release |
| 0.17.1 | Published; GitHub, PyPI, crates and npm artifacts verified | Follow the versioned upgrade guide |
| 0.18.0 | Source candidate under qualification | Do not assume source-only features exist in 0.17.1 packages |

Release validation covers actual binary startup, package identities, supported
platform artifacts and registry metadata. Publication is complete only after the
expected artifacts are independently verified. Client adoption is a separate
check: it must use the published SDK version intended for that release.

## Follow-up status

These public PRs record the maintenance and integration work following 0.17.0.
Detailed deployment records and operator-specific diagnostics are not part of
this user-facing summary.

| PR | Disposition | Result |
| --- | --- | --- |
| [#395](https://github.com/anvai-labs/sandhi/pull/395) | Superseded | Protocol updates retained in the reviewed integration |
| [#410](https://github.com/anvai-labs/sandhi/pull/410) | Merged | Immutable CI dependency maintenance |
| [#396](https://github.com/anvai-labs/sandhi/pull/396) | Merged | Documentation and branding maintenance |
| [#447](https://github.com/anvai-labs/sandhi/pull/447) | Integrated through #456 | Protocol dependency update |
| [#362](https://github.com/anvai-labs/sandhi/pull/362) | Replaced through #456 | Explicit provider-envelope admission |
| [#363](https://github.com/anvai-labs/sandhi/pull/363) | Replaced through #456 | Catalog refresh without granting routing authority |
| [#364](https://github.com/anvai-labs/sandhi/pull/364) | Replaced through #456 | Additive typed reasoning contract |
| [#448](https://github.com/anvai-labs/sandhi/pull/448) | Split replacements integrated | Qualified dependency and runtime updates |

## Corrective release and documentation queue

The corrective 0.17.1 release covers binary startup and stream-finality fixes.
The 0.18 source train adds its own reviewed access and transport changes; it does
not alter the feature set of already published 0.17.1 artifacts.

Documentation follows the installed contract. Examples use placeholder domains
and identifiers; role admission, certificate trust and token lifecycle must be
validated for each deployment. Operational records retain detailed provenance
separately from public setup instructions.

## Three-project identity co-design

| Component | Responsibility |
| --- | --- |
| Anvai Identity | Authentication, active identity/session authority and resource-scoped tokens |
| Victor | Originating OAuth client, private credential lifecycle and Agent execution |
| Sandhi | Consuming application, service-owned authorization, routing and accounting |

The same authenticated identity can use the dashboard and API. Browser sessions
and API access tokens remain distinct credentials, and a dashboard role does not
implicitly authorize inference. See [identity and service policy](../operator/oidc-sso.md#shared-identity-service-owned-policy).

### Public issuer Agent acceptance

A recorded integration used a development identity service, a protected local
source-candidate gateway and a synthetic backend. It verified code/S256,
authoritative UserInfo, an explicitly intended resource, a private token file
and a successful real Agent read-tool loop.

| Observation | Result |
| --- | --- |
| Physical model attempts | 2 |
| Settled tokens per request | 14, 14 |
| Total neutral token units | 28 |
| Correlation | Request identities, durable usage and issuer receipts matched |
| Provider costs and cache savings | Not measured by this synthetic fixture |

The run used the published 0.17.1 SDK against the 0.18 source candidate. Separate
local fixtures covered browser roles, credential rotation, expiry, incorrect
resource/purpose and logout. These results do not establish another deployment's
SSO, renewal, provider connectivity or authorization policy.

## Completion evidence

Before rollout, verify the installed artifact, HTTPS trust, intended issuer and
client, allowed and denied operations, real tool execution, conserved usage,
expiry/logout and restart behavior. Keep credentials, internal addresses and
unredacted diagnostic artifacts out of public documentation and support reports.
