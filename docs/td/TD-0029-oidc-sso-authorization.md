# TD-0029: OIDC login and role-based gateway access

Status: In progress — implementation and local protocol tests pending; live Kanidm acceptance blocked on dedicated client registration.

## Context and evidence

The co-design deployment needs browser SSO and short-lived agent credentials instead of an administrator token pasted into the dashboard. Existing virtual keys, provider secrets, metering, budgets and attribution must survive migration. Sandhi `develop` was synchronized at `167852a9`; the original checkout's untracked files were preserved. Implementation uses a linked worktree.

On 2026-09-22, Kanidm 1.11.1 on dataserver3 served verified HTTPS discovery for the existing `proximadb` client. Its issuer is client-specific and its JWKS path ends in `public_key.jwk`. Discovery for `sandhi` and `inferflux` returned 404. DNS is fixed on the Mac and aiserver1; the public internal CA is available for application-scoped verification. Existing ProximaDB registration is not to be modified or reused. No private credentials were transferred. The Mac gateway has not been replaced.

## Contract

1. Standalone deployments default to `SANDHI_AUTH_MODE=oidc`. Missing/invalid OIDC configuration or failed discovery prevents startup. `tokens` is an explicit compatibility profile, never a fallback after failed SSO. Existing library `ProxyState::new` behavior is retained for embedders; OIDC is configured explicitly on state.
2. `SANDHI_OIDC_CONFIG` references strict JSON containing issuer, client ID, exact callback URL, optional public CA file/client-secret environment-variable name, requested scopes, subject-to-role bindings, and subject-to-inference grants. Secrets are excluded from committed configuration. TLS verifies chains and hostnames; redirects are disabled for discovery/token/introspection requests. Endpoints must use HTTPS and the issuer's origin.
3. Browser login uses authorization code + S256 PKCE, random state, nonce and a browser-bound login cookie. The maintained `openidconnect` library verifies ID-token signature, issuer, audience, expiry and nonce, including access-token hash when present. A one-use callback creates a fresh opaque server-side session; tokens never reach JavaScript or local storage. Sessions and pending logins are bounded, expiring, in memory and lost on restart. Session lifetime is capped by ID-token expiry and one hour. Cookies are HttpOnly, Secure, SameSite=Lax, host-only. HTTPS callback is required.
4. Roles are explicit server-side bindings to issuer-qualified subjects, configured under one issuer: `viewer` reads usage, runs and masked operational metadata; `operator` additionally changes budgets and alerts; `admin` additionally manages credentials, virtual keys, configuration and diagnostics. No role is implied by successful login. Read scope is deployment-wide, not tenant isolation. The UI displays the authenticated subject/role and enables controls from server-provided permissions; backend authorization remains authoritative.
5. Cookie-authenticated mutations require a per-session CSRF value and exact configured Origin. Logout invalidates the local session and cookie; it does not pretend to terminate the IdP session. Expired sessions require login again. No refresh tokens are persisted.
6. Agent/admin bearer access tokens are validated through the discovered RFC 7662 introspection endpoint (Kanidm advertises no endpoint authentication), with explicit active/issuer/audience/subject/expiry checks. ID tokens are not accepted as access credentials. Introspection outages fail closed. There is no automatic key retry and browser cookies cannot authorize inference.
7. An inference grant maps the validated subject to a configured upstream reference, nonempty model allowlist, optional group/budget scope and rate limit. It produces one stable, nonsecret authorization identity and enters the existing model/attribution/rate/budget/session/forwarding/metering path. Caller headers cannot replace the bound identity. OIDC does not forward caller credentials to providers. ZAI still requires its existing provider credential; upstream InferFlux OIDC/token exchange is a separate co-design acceptance item, not solved by browser login.

## Increments and acceptance

| Increment | Evidence required | Status |
|---|---|---|
| S1 discovery/login/session and role enforcement | Signed mock IdP protocol tests; state/nonce/PKCE/replay/expiry/issuer/audience negatives; backend role matrix; CSRF/logout | Pending |
| S2 OIDC inference grants and compatibility | Actual proxy transport tests retain model allowlist, attribution, budgets, correlation; explicit token profile regression | Pending |
| S3 dashboard and deployment | Browser login/role/expiry/logout tests; dedicated Kanidm registration; verified HTTPS; preserve existing state and rollback | Pending |
| S4 multi-provider co-design | Direct and optional Sandhi InferFlux acceptance, all formation cohorts, C5 six-Qwen/one-ZAI reconciliation | Pending; separate Victor/InferFlux ownership |

Test ownership: extend existing operator authorization and dashboard browser suites for their existing routes; keep OIDC protocol cases in one module. Existing compatibility tests cover virtual-key accounting and must not be cloned for each role. Remove tests only when an equivalent owner demonstrably preserves their assertions. Mock protocol acceptance is not live Kanidm or model acceptance.

## Open gaps

- Dedicated Kanidm registrations and role subjects require authorized administrative access; Docker control alone is not a reason to reset an administrator password.
- InferFlux TLS hostname verification, strict token claims, discovered JWKS, explicit OIDC-only policy and safe audit identity require their own fixes before SSO acceptance.
- Session revocation is local until expiry/restart; distributed sessions, IdP backchannel logout and refresh are out of scope and must remain documented.
- Server-side subject role/grant configuration requires restart. IdP group-to-role synchronization is not implied.
- C5 remains open. Successful discovery, login, unit tests or the earlier five-call replay cannot establish full mixed-team acceptance.
