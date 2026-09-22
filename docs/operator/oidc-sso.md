# OIDC SSO setup and migration

The standalone `sandhi-proxy` defaults to `SANDHI_AUTH_MODE=oidc`. It refuses to start
without valid OIDC configuration and verified HTTPS discovery. `--help` and `--version`
work without service configuration. Embedded `ProxyState::new` retains its existing
behavior; embedders opt into OIDC explicitly.

## Configure the identity provider

Register a dedicated OAuth client with authorization-code flow, S256 PKCE and an exact
HTTPS callback ending in `/auth/callback`. ES256 and RS256 signing are supported. The
provider must publish discovery, JWKS and RFC 7662 introspection endpoints on its issuer
origin. Sandhi validates signature, issuer, audience, expiry, nonce and any access-token
hash for browser login; bearer access tokens are introspected on each request.

For Kanidm, use a dedicated client and access groups; do not reuse another application's
registration. For a public client, no application secret is needed:

```sh
kanidm system oauth2 create-public sandhi 'Sandhi Gateway' https://gateway.example.com/dashboard
kanidm system oauth2 add-redirect-url sandhi https://gateway.example.com/auth/callback
kanidm system oauth2 enable-strict-redirect-url sandhi
kanidm group create sandhi_users
kanidm system oauth2 update-scope-map sandhi sandhi_users openid profile
kanidm group add-members sandhi_users YOUR_ACCOUNT
```

Kanidm's issuer is client-specific: `https://IDP/oauth2/openid/sandhi`. Modern Kanidm
discovery advertises S256 and ES256, and its introspection endpoint advertises `none`
authentication. A confidential client can instead use `client_secret_env` in the
configuration below; keep that secret in the deployment's private environment.

## Configure Sandhi

Create a private `oidc.json`. Subject keys are the verified OIDC `sub` (Kanidm UUID),
not display names, email addresses or caller headers. Replace every example value:

```json
{
  "issuer": "https://idp.example.com/oauth2/openid/sandhi",
  "client_id": "sandhi",
  "redirect_url": "https://gateway.example.com/auth/callback",
  "scopes": ["profile"],
  "subjects": {
    "OPERATOR_SUBJECT_UUID": {"role": "admin"},
    "AGENT_SUBJECT_UUID": {
      "grants": {
        "local": {
          "upstream": "inferflux:local",
          "models": ["qwen3-coder-30b", "qwen2.5-coder-14b"],
          "group": "multiagent-validation"
        },
        "cloud": {"upstream": "zai:cloud", "models": ["glm-5.3"], "group": "multiagent-validation"}
      }
    }
  }
}
```

For an internal CA add `"ca_file": "/run/identity/public-ca.pem"`. This adds trust for
the IdP without disabling chain or hostname verification. To use a confidential client
add `"client_secret_env": "SANDHI_OIDC_CLIENT_SECRET"`. No secrets belong in committed JSON.

```sh
SANDHI_OIDC_CONFIG=/run/sandhi/oidc.json \
SANDHI_CONFIG=/run/sandhi/config.json \
SANDHI_STORE=/var/lib/sandhi/usage.db sandhi-proxy
```

Configure listener TLS in `SANDHI_CONFIG` (`tls.cert` and `tls.key`) or terminate TLS at
a trusted reverse proxy with a private backend connection. The configured callback must
match the browser's external HTTPS origin. Do not expose a plaintext backend across an
untrusted network. Forward `/auth/*`, `/dashboard*`, and `/admin/*` on the same origin.
OIDC configuration grants access; provider credentials still live in the existing vault
or explicitly configured provider environment. Register credentials through an administrator
session or declarative provider configuration before inference.

## Roles and agent access

| Role | Allowed operations |
|---|---|
| `viewer` | Usage, run queries and masked operational metadata |
| `operator` | Viewer operations plus budget and alert changes |
| `admin` | Operator operations plus credentials, virtual keys, configuration and diagnostics |

Roles are deployment-wide, not tenant isolation. A successful login grants no implicit
role. Subject bindings are server configuration and require restart; IdP groups control
client admission but are not automatically synchronized to Sandhi roles.

Open `/dashboard` and select **Sign in with SSO**. Tokens stay server-side. Browser
sessions use Secure, HttpOnly, SameSite=Lax `__Host-` cookies. Writes require both a CSRF
proof and the exact configured Origin. **Sign out** invalidates this Sandhi session; it
does not sign the user out of Kanidm or other applications.

Agent clients supply a short-lived OAuth **access token** in their SDK credential field.
ID tokens and dashboard cookies cannot authorize inference. With multiple configured
grants, send `x-sandhi-grant: local` or `cloud`; a single grant needs no selector. The
selected grant enters the same model allowlist, attribution, rate, budget and metering
path as virtual keys. Caller credentials are never forwarded to upstream providers.
Upstream authentication is independent: ZAI retains its provider credential, and an
InferFlux deployment may use its separately configured OIDC or API-key profile.

## Migrate and verify

### Optional gateway and authentication are separate choices

Choose the request route explicitly in the consuming application. Direct access means
Victor calls the provider itself; gateway access means it calls Sandhi. Disabling the
gateway must not disable authentication on InferFlux or another selected provider.

| Deployment | Request route | Authentication and enforcement |
|---|---|---|
| Managed multiagent deployment | Through Sandhi | OIDC by default; Sandhi grants, metering and budgets apply |
| Authorized direct development or diagnosis | Direct to InferFlux/provider | Provider authentication still required; Sandhi cannot meter or enforce these calls |
| Explicit compatibility deployment | Either route | Scoped API/virtual keys configured for that route; never selected automatically after an OIDC error |

If a deployment requires Sandhi's budgets or audit trail, restrict direct origin access
to the gateway identity/network and separately authorized operators. An unrestricted
direct route would bypass those controls even with valid user authentication. Never
silently reroute a failed gateway request or retry it with weaker credentials.

Keep route selection, model selection and identity separate in the client UX. Show the
selected endpoint and whether Sandhi metering is active; distinguish gateway connection
failure, identity-provider unavailability, expired login, forbidden access and model
readiness. Browser users sign in once through SSO; unattended agents use a dedicated
service identity and short-lived OAuth access tokens. Kanidm supports exchanging a
service-account API token for these access tokens; the bootstrap credential must remain
private. This does not eliminate provider credentials such as ZAI's API key.

Each service validates its own intended token audience. A token issued for Sandhi must
not simply be forwarded to InferFlux. Upstream token acquisition, renewal and exchange
need separate acceptance; the current gateway does not implement those automatically.

References: [OAuth security best practices](https://www.rfc-editor.org/rfc/rfc9700.html)
and [Kanidm service accounts](https://kanidm.github.io/kanidm/stable/accounts/service_accounts.html).

### Deployment sequence

1. Preserve the current binary, private configuration, provider credentials and SQLite
   state. Stop the old writer before replacing its live database; never clear shared cache.
2. Register the dedicated client and bind known subjects. Validate the candidate on an
   isolated listener and disposable or backed-up state first.
3. Verify login, roles, budget writes, expiry and logout through the external HTTPS URL.
   Test actual agent access and confirm provider/model, request/session and usage joins.
4. Replace the service with the reviewed release and its explicit configuration. Check
   `sandhi --version`, `sandhi-proxy --version` and `/version.package_version` agree.

For retained virtual-key/admin-token deployments, set `SANDHI_AUTH_MODE=tokens`
explicitly. This mode keeps the legacy API and dashboard token UI; it is never selected
after an OIDC error. `scripts/quickstart.sh` is only this local compatibility bootstrap.
`SANDHI_ADMIN_TOKEN` and `SANDHI_DASHBOARD_PUBLIC` do not bypass OIDC authorization.

Login attempts and sessions are in-memory and bounded (1,024 pending logins, 4,096 active
sessions, 16 simultaneous authority operations). Pending login expires after five minutes;
sessions expire at the earlier of ID-token expiry or one hour and disappear on restart.
Authority requests have ten-second deadlines and 256 KiB response limits. Distributed
sessions, refresh tokens, backchannel logout and automatic IdP role synchronization are
not implemented. An IdP outage fails closed; already issued browser sessions remain valid
until their local expiry or logout.
