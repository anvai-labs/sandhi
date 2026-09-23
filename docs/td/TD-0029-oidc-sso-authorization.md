# TD-0029: OIDC login and role-based gateway access

Status: In progress — OIDC, role enforcement and scoped diagnostics are reviewed and released in 0.9.1. Published-binary Kanidm browser and ZAI actual-member checks passed. Human-account onboarding requires both IdP admission and a Sandhi role binding; runtime role management, bounded startup and full InferFlux/C5 acceptance remain open.

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
- All-feature workspace coverage reports 89.18% lines. The 55 dashboard/startup tests
  passed; all 36 broker/recovery/acceptance tests passed with short macOS socket paths
  and an explicit fixture token file. `SANDHI_SENTINELPASS_TOKEN_FILE` avoids reading
  the developer's default daemon credential in these fixtures; invalid explicit files
  fail instead of falling back. Final consolidated checks and review remain required.

## Increments and acceptance

| Increment | Evidence required | Status |
|---|---|---|
| S1 discovery/login/session and role enforcement | Signed mock IdP protocol tests; state/nonce/PKCE/replay/expiry/issuer/audience negatives; backend role matrix; CSRF/logout | Reviewed and released; published 0.9.1 browser login/logout and eight authorization checks passed |
| S2 OIDC inference grants and compatibility | Actual proxy transport tests retain model allowlist, attribution, budgets, correlation; explicit token profile regression | Reviewed and released; new 0.9.1 sequential actual-member case passed six calls and six accounting joins |
| S3 dashboard and deployment | Browser login/role/expiry/logout tests; dedicated Kanidm registration; verified HTTPS; preserve existing state and rollback | Published binary and actual-member browser lookup passed; initial 25-second startup acceptance failed and remains separate |
| S4 multi-provider co-design | Direct and optional Sandhi InferFlux acceptance, all formation cohorts, C5 six-Qwen/one-ZAI reconciliation | Pending; separate Victor/InferFlux ownership |
| S5 scoped accounting diagnostics | Explicit subject permission without writes/inference; existing default-role and compatibility denials, CSRF, introspection/revocation and bounds | Source #285 released in 0.9.0/0.9.1; separate viewer plus diagnostics passes live accounting while mutation/inference remain denied |

Test ownership: extend existing operator authorization and dashboard browser suites for their existing routes; keep OIDC protocol cases in one module. Existing compatibility tests cover virtual-key accounting and must not be cloned for each role. Remove tests only when an equivalent owner demonstrably preserves their assertions. Mock protocol acceptance is not live Kanidm or model acceptance.

S5 addresses Victor G49's permission mismatch: C4 diagnostics was admin-only even
though the operation is read-only. The additive `allow_diagnostics` subject flag
uses the existing permission dispatcher and defaults to false. A dedicated viewer
with this flag may perform accounting reads but cannot change credentials, budgets
or configuration and receives no inference grant. Existing role/CSRF tests own
the expanded matrix; the HTTPS authority fixture checks bearer access and revocation.
Compatibility/admin-first bounds tests remain their original owners. No duplicate
diagnostic projection, accounting or browser tests are added. The source increment
alone did not establish renewable Victor credentials or final-binary deployment;
subsequent release evidence is recorded below. Actual-member C5 remains open.

## Open gaps

- Published 0.9.1 ZAI acceptance below does not establish full mixed-provider origin correlation. Preserve the failed 502 and wrong-route harness evidence.
- InferFlux's strict token/identity boundaries ([#212](https://github.com/anvai-labs/inferflux/pull/212)), outbound TLS peer verification ([#213](https://github.com/anvai-labs/inferflux/pull/213)) and safe audit identity ([#214](https://github.com/anvai-labs/inferflux/pull/214)) are merged source fixes. Discovered JWKS, explicit OIDC-only policy and direct-origin Kanidm interoperability still require their own acceptance; the current origin uses its explicitly configured private API-key path.
- Session revocation is local until expiry/restart; distributed sessions, IdP backchannel logout and refresh are out of scope and must remain documented.
- Server-side subject role/grant configuration requires restart. IdP group-to-role synchronization is not implied. A future role editor/API must be admin-only, preserve one authoritative binding store, require CSRF/Origin for browser mutations, audit actor/target/change without credentials, reject concurrent stale updates, and define revocation and recovery/last-admin behavior. This is a design requirement, not shipped functionality.
- The published gateway twice missed a 25-second startup-readiness window while its process stayed alive; later exact-process/version/readiness checks passed. Preserve both failures. Startup phase timing and repeatable bounded acceptance remain open; the cause is not established as OIDC or keyring access.
- C5 remains open. Successful discovery, login, unit tests or the earlier five-call replay cannot establish full mixed-team acceptance.

### Published 0.9.1 and human onboarding, 2026-09-23

Reviewed promotion [#293](https://github.com/anvai-labs/sandhi/pull/293) and exact-main
CI produced source `4968fd045550539f0bddfb1892e9c4f3f226ce5f`. All release targets verified;
the first verification saw npm propagation 404 and the read-only verifier later passed
without republishing. [Tap #68](https://github.com/anvai-labs/homebrew-tap/pull/68) and
its actual install/tests passed before the Mac upgrade. Both commands report 0.9.1;
the serving proxy SHA-256 is `ff4d7bd68ce4ea33a593416899acb431b845e2d217255451f98bb42593885b63`.
Private configuration, usage data, provider credentials and rollback binaries were retained.

The new Victor sequential case `matrix-9099a9446a` passed six HTTP-200 calls and six
wire/SQLite/C4 joins, with two distinct member sessions, two deliverables, two passing
pytest checks and two numeric oracles. Fresh/cache-read/output totals are 5067/13248/535,
with explicit cache reporting 6/6. A TLS-verified admin browser login displayed one new
member's three calls with matching totals and logged out successfully. This is one new
release smoke case, not a rerun of the preserved 15-case cohorts or C5 acceptance.

A subsequent human login exposed a provisioning omission: Kanidm authenticated the
account but reported no available application scopes, and the subject had no Sandhi
role binding. The repair adds only the verified account to the existing mapped viewer
group and a `viewer` subject entry, preserving other bindings. Administrator browser
success is not evidence of another person's access. Follow the
[two-stage onboarding and role-change guide](../operator/oidc-sso.md#onboard-a-human-dashboard-user);
the person's fresh login is a separate verification step. After the repair and
restart, the user confirmed that the dashboard opened. This is user-reported human
login evidence, distinct from the automated administrator browser check.

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

The same successful run was subsequently reconciled through a fresh, TLS-verified
Kanidm browser session: the protected run API and visible dashboard both showed one
call, 23 input, 24 output, zero cache read and 47 billable tokens. Reporting coverage
was explicit for 1/1 calls, with 21 reasoning tokens included in output. Evidence is
retained on dataserver3 as `oidc-machine-1790114383462918686-dashboard-evidence.json`.
This closes the machine integration's wire/SQLite/run/dashboard comparison without
repeating inference; origin request-ID joins and actual-member acceptance remain open.

### Dependency advisory applicability

PR #281's initial security check identified RUSTSEC-2023-0071 in the maintained
OIDC library's RSA dependency. Independent operation review found only public-key
verification in production; the private-key timing attack has no private key to
recover on that path. This is not a patched dependency. The explicit assessment
expires on 2026-10-22 and is bound to package checksums, production dependency
routes and the reviewed adapter. The security job runs the drift/expiry guard
before cargo-deny; every other advisory remains fatal. See the [assessment and
limitations](../security/oidc-rsa-advisory.md). Reassessment is required before
private-key operations or additional OIDC/RSA consumers are introduced.
