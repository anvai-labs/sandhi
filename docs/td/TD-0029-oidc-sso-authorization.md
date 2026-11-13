# TD-0029: OIDC login and role-based gateway access

Status: OIDC login, role enforcement, inference grants and scoped diagnostics are
released capabilities. The 0.18 source candidate adds optional authentication
profiles; see the [current operator guide](../operator/oidc-sso.md).

## Context

Operators need browser SSO and short-lived agent credentials while preserving
provider-secret custody, metering, budgets and attribution. A deployment must
qualify its own issuer, client registrations and user admission; a successful
login or provider discovery alone does not establish model access.

## Contract

1. The original deployment contract defaulted to `SANDHI_AUTH_MODE=oidc`. The 0.18 source candidate supersedes that default with a credential-free loopback starter; tokens and OIDC are optional. Supplying `SANDHI_OIDC_CONFIG` without a mode selects OIDC, and missing/invalid explicitly selected OIDC or failed discovery prevents startup. Tokens and starter access are never fallback profiles after failed SSO. Existing library `ProxyState::new` behavior is retained for embedders; OIDC is configured explicitly on state.
2. `SANDHI_OIDC_CONFIG` references strict JSON containing issuer, client ID, exact callback URL, optional public CA file/client-secret environment-variable name, requested scopes, subject-to-role bindings, and subject-to-inference grants. Secrets are excluded from committed configuration. TLS verifies chains and hostnames; redirects are disabled for discovery/token/introspection requests. Endpoints must use HTTPS and the issuer's origin.
3. Browser login uses authorization code + S256 PKCE, random state, nonce and a browser-bound login cookie. The maintained `openidconnect` library verifies ID-token signature, issuer, audience, expiry and nonce, including access-token hash when present. A one-use callback creates a fresh opaque server-side session; tokens never reach JavaScript or local storage. Sessions and pending logins are bounded, expiring, in memory and lost on restart. Session lifetime is capped by ID-token expiry and one hour. Cookies are HttpOnly, Secure, SameSite=Lax, host-only. HTTPS callback is required.
4. Roles are explicit server-side bindings to issuer-qualified subjects, configured under one issuer: `viewer` reads usage, runs and masked operational metadata; `operator` additionally changes budgets and alerts; `admin` additionally manages credentials, virtual keys, configuration and diagnostics. No role is implied by successful login. Read scope is deployment-wide, not tenant isolation. The UI displays the authenticated subject/role and enables controls from server-provided permissions; backend authorization remains authoritative.
5. Cookie-authenticated mutations require a per-session CSRF value and exact configured Origin. Logout invalidates the local session and cookie; it does not pretend to terminate the IdP session. Expired sessions require login again. No refresh tokens are persisted.
6. Agent/admin bearer access tokens are validated through the discovered RFC 7662 introspection endpoint using the advertised endpoint authentication, with explicit active/issuer/audience/subject/expiry checks. ID tokens are not accepted as access credentials. Introspection outages fail closed. There is no automatic key retry and browser cookies cannot authorize inference.
7. An inference grant maps the validated subject to a configured upstream reference, nonempty model allowlist, optional group/budget scope and rate limit. It produces one stable, nonsecret authorization identity and enters the existing model/attribution/rate/budget/session/forwarding/metering path. Caller headers cannot replace the bound identity. OIDC does not forward caller credentials to providers. ZAI still requires its existing provider credential; upstream InferFlux OIDC/token exchange is a separate co-design acceptance item, not solved by browser login.
8. Gateway routing and authentication are independent. Authorized clients may select direct provider access, retaining provider authentication, but these calls have no Sandhi metering or budget enforcement. Managed deployments that require gateway controls must restrict origin access accordingly. No automatic route or credential downgrade follows a gateway/IdP failure. Client UX must expose the selected route, metering status and actionable failure category. See the [deployment guide](../operator/oidc-sso.md#optional-gateway-and-authentication-are-separate-choices).

## Qualification summary

This is a curated summary of historical implementation and integration evidence.
Detailed deployment records and raw operational artifacts are retained privately.

| Capability | Evidence boundary |
| --- | --- |
| Browser login and roles | Published 0.9.1 checks covered verified HTTPS login/logout, backend permissions and CSRF-protected writes |
| Agent inference and accounting | Model responses were joined to gateway usage and receipts; this does not qualify every provider or routing topology |
| Human onboarding | Identity-provider admission and an explicit Sandhi subject binding are both required; an administrator login does not verify another account |
| Scoped diagnostics | `allow_diagnostics` permits bounded accounting reads without granting writes or inference |
| Failure classification | Authority outages and client-authentication failures remain distinguishable from invalid caller credentials; diagnostics omit endpoint secrets |

The protocol suites cover state, nonce, PKCE, replay, expiry, issuer and audience
refusal, exact Origin, role enforcement, introspection, revocation and bounded
requests. Dashboard and provider integration tests remain separate acceptance
owners. Mock protocol success does not claim a live deployment.

## Remaining boundaries

- Browser sessions are local and expire; distributed sessions, IdP backchannel
  logout and automatic browser-session renewal are not implemented.
- Subject-role and inference-grant configuration requires a gateway restart.
  Successful identity-provider login and group admission do not imply a Sandhi role.
- Upstream provider authentication is independent. A token intended for Sandhi
  cannot be forwarded as proof of access to another resource.
- Qualify discovery, registration, login, roles, actual inference, accounting,
  expiry, revocation and service readiness against the intended deployment.
  Preserve state and credential custody when changing its configuration.

For human access, follow [onboarding and role changes](../operator/oidc-sso.md#onboard-a-human-dashboard-user).
For supported group bindings, short-lived delegation and explicit durable subject
grants, see [identity and group ownership](../operator/identity-groups.md).
Durable subject keys do not inherit directory membership or automatic account
revocation; renewable OIDC credentials remain a separate client responsibility.

## Dependency advisory applicability

OIDC uses maintained public-key verification dependencies. Advisory applicability
is bound to reviewed operations and guarded against dependency or implementation
drift; it is not a claim that an affected dependency is patched. See the
[current assessment and limitations](../security/oidc-rsa-advisory.md).
