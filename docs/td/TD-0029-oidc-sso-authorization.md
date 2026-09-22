# TD-0029: OIDC login and role-based gateway access

Status: In progress — implementation and local protocol/browser coverage present; dedicated Kanidm registration and live browser acceptance passed. A live machine-to-ZAI integration check passed; upstream OIDC, full reconciliation, independent final review and release acceptance remain open.

## Context and evidence

The co-design deployment needs browser SSO and short-lived agent credentials instead of an administrator token pasted into the dashboard. Existing virtual keys, provider secrets, metering, budgets and attribution must survive migration. Sandhi `develop` was synchronized at `84414a9`; the original checkout's untracked files were preserved. Implementation uses a linked worktree.

On 2026-09-22, Kanidm 1.11.1 on dataserver3 served verified HTTPS discovery for the existing `proximadb` client. Its issuer is client-specific and its JWKS path ends in `public_key.jwk`. Discovery for `sandhi` and `inferflux` returned 404. DNS is fixed on the Mac and aiserver1; the public internal CA is available for application-scoped verification. Existing ProximaDB registration is not to be modified or reused. No private credentials were transferred. The Mac gateway has not been replaced.

## Contract

1. Standalone deployments default to `SANDHI_AUTH_MODE=oidc`. Missing/invalid OIDC configuration or failed discovery prevents startup. `tokens` is an explicit compatibility profile, never a fallback after failed SSO. Existing library `ProxyState::new` behavior is retained for embedders; OIDC is configured explicitly on state.
2. `SANDHI_OIDC_CONFIG` references strict JSON containing issuer, client ID, exact callback URL, optional public CA file/client-secret environment-variable name, requested scopes, subject-to-role bindings, and subject-to-inference grants. Secrets are excluded from committed configuration. TLS verifies chains and hostnames; redirects are disabled for discovery/token/introspection requests. Endpoints must use HTTPS and the issuer's origin.
3. Browser login uses authorization code + S256 PKCE, random state, nonce and a browser-bound login cookie. The maintained `openidconnect` library verifies ID-token signature, issuer, audience, expiry and nonce, including access-token hash when present. A one-use callback creates a fresh opaque server-side session; tokens never reach JavaScript or local storage. Sessions and pending logins are bounded, expiring, in memory and lost on restart. Session lifetime is capped by ID-token expiry and one hour. Cookies are HttpOnly, Secure, SameSite=Lax, host-only. HTTPS callback is required.
4. Roles are explicit server-side bindings to issuer-qualified subjects, configured under one issuer: `viewer` reads usage, runs and masked operational metadata; `operator` additionally changes budgets and alerts; `admin` additionally manages credentials, virtual keys, configuration and diagnostics. No role is implied by successful login. Read scope is deployment-wide, not tenant isolation. The UI displays the authenticated subject/role and enables controls from server-provided permissions; backend authorization remains authoritative.
5. Cookie-authenticated mutations require a per-session CSRF value and exact configured Origin. Logout invalidates the local session and cookie; it does not pretend to terminate the IdP session. Expired sessions require login again. No refresh tokens are persisted.
6. Agent/admin bearer access tokens are validated through the discovered RFC 7662 introspection endpoint (Kanidm advertises no endpoint authentication), with explicit active/issuer/audience/subject/expiry checks. ID tokens are not accepted as access credentials. Introspection outages fail closed. There is no automatic key retry and browser cookies cannot authorize inference.
7. An inference grant maps the validated subject to a configured upstream reference, nonempty model allowlist, optional group/budget scope and rate limit. It produces one stable, nonsecret authorization identity and enters the existing model/attribution/rate/budget/session/forwarding/metering path. Caller headers cannot replace the bound identity. OIDC does not forward caller credentials to providers. ZAI still requires its existing provider credential; upstream InferFlux OIDC/token exchange is a separate co-design acceptance item, not solved by browser login.
8. Gateway routing and authentication are independent. Authorized clients may select direct provider access, retaining provider authentication, but these calls have no Sandhi metering or budget enforcement. Managed deployments that require gateway controls must restrict origin access accordingly. No automatic route or credential downgrade follows a gateway/IdP failure. Client UX must expose the selected route, metering status and actionable failure category. See the [deployment guide](../operator/oidc-sso.md#optional-gateway-and-authentication-are-separate-choices).

## New evidence, 2026-09-22

- Dedicated public Kanidm clients `sandhi` and `inferflux` and scoped access groups were
  provisioned through the pinned Docker CLI using existing private administrative
  credentials. Existing application registrations were preserved. Registration alone
  does not establish InferFlux OIDC acceptance.
- A candidate using an isolated SQLite backup passed actual Kanidm browser login,
  administrator role enforcement, a CSRF-protected budget write and logout through
  verified HTTPS. Evidence remains on dataserver3 in the private co-design state
  directory as `sandhi-browser-evidence.json`; the original Mac gateway is unchanged.
- A dedicated machine service account successfully exchanged its locally retained
  Kanidm API token for a 900-second OAuth access token. The subsequent inference
  attempt returned gateway 502 and remains failed evidence, not model acceptance.
- The candidate restart failure was independently identified in the macOS crash report
  as `SIGKILL (Code Signature Invalid)` / `Taskgated Invalid Signature`, before startup.
  Staging a new inode, verifying its signature and atomically replacing the candidate
  restored the process and `/auth/session` through the existing SSH tunnel. This 502
  did not establish an OIDC rejection. Earlier discovery timeouts were addressed by
  removing the shorter connection deadline while retaining the ten-second total bound.
- All-feature workspace coverage reports 89.17% lines. The 55 dashboard/startup tests
  passed; all 36 broker/recovery/acceptance tests passed with short macOS socket paths
  and an explicit fixture token file. `SANDHI_SENTINELPASS_TOKEN_FILE` avoids reading
  the developer's default daemon credential in these fixtures; invalid explicit files
  fail instead of falling back. Final consolidated checks and review remain required.

## Increments and acceptance

| Increment | Evidence required | Status |
|---|---|---|
| S1 discovery/login/session and role enforcement | Signed mock IdP protocol tests; state/nonce/PKCE/replay/expiry/issuer/audience negatives; backend role matrix; CSRF/logout | Implemented and locally tested; final review pending |
| S2 OIDC inference grants and compatibility | Actual proxy transport tests retain model allowlist, attribution, budgets, correlation; explicit token profile regression | Local protocol/compatibility coverage and live machine-to-ZAI integration passed; final review pending |
| S3 dashboard and deployment | Browser login/role/expiry/logout tests; dedicated Kanidm registration; verified HTTPS; preserve existing state and rollback | Browser suite and live Kanidm browser pass; release/cutover pending |
| S4 multi-provider co-design | Direct and optional Sandhi InferFlux acceptance, all formation cohorts, C5 six-Qwen/one-ZAI reconciliation | Pending; separate Victor/InferFlux ownership |

Test ownership: extend existing operator authorization and dashboard browser suites for their existing routes; keep OIDC protocol cases in one module. Existing compatibility tests cover virtual-key accounting and must not be cloned for each role. Remove tests only when an equivalent owner demonstrably preserves their assertions. Mock protocol acceptance is not live Kanidm or model acceptance.

## Open gaps

- The corrected machine integration run passed OAuth exchange, ZAI inference and forbidden admin access; full request/run/dashboard reconciliation and final-binary acceptance remain required. Preserve the failed 502 and wrong-route harness evidence.
- InferFlux TLS hostname verification, strict token claims, discovered JWKS, explicit OIDC-only policy and safe audit identity require their own fixes before SSO acceptance.
- Session revocation is local until expiry/restart; distributed sessions, IdP backchannel logout and refresh are out of scope and must remain documented.
- Server-side subject role/grant configuration requires restart. IdP group-to-role synchronization is not implied.
- C5 remains open. Successful discovery, login, unit tests or the earlier five-call replay cannot establish full mixed-team acceptance.

### Reviewed corrections and machine check

Authority transport failures, HTTP 5xx/429 and introspection client-auth failures now
return 503. Discovery determines whether introspection uses client authentication;
Kanidm no-auth HTTP 400/401 responses for malformed caller tokens remain 401; an inactive access token or rejected authorization grant remains 401.
Endpoint URLs are stripped from error diagnostics, including query-string secrets.
Existing HTTPS protocol test owners cover these distinctions, a real token timeout,
the total request deadline and captured-log redaction. Tests first reproduced the
misleading 401 responses, then passed after the correction.

The corrected new machine run `oidc-machine-1790114383462918686` exchanged a service
identity token (900-second access-token lifetime), received HTTP 200 from ZAI and HTTP
403 from the real admin configuration route. Wire usage and the isolated SQLite event
both report 23 fresh input tokens, 24 output tokens and zero cache-read tokens; the
subject, group, model, run, step and session match the configured grant. End-to-end
request time was 1,742 ms versus the gateway's recorded 1,717 ms; token exchange took
33 ms. These are individual observations, not a performance SLO. Gateway event ID
`req_1790114388547_1` differs from the provider response's `x-request-id`; do not claim
those identifiers are identical or that this proves C4 origin correlation. This small
integration request is not actual-member, formation or C5 evidence. The preceding
HTTP-200 run had a harness failure because it tested a nonexistent admin URL (404);
its original report remains preserved alongside the earlier 502.

Consolidated SDK/browser validation: 269 passed, 19 optional integrations skipped.
The release suite exposed a pre-existing brittle drift fixture: it changed the first
matching version in Cargo.lock, which can belong to a third-party package. The fixture
now targets the named sandhi-core package; no production version check was weakened.
