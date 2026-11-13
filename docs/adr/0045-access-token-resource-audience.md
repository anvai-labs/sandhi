# ADR-0045: Separate access-token resource audience from OAuth client identity

- **Status:** Post-release source candidate; no live migration or deployment acceptance.
- **Owner:** Existing OIDC configuration and introspection in `sandhi-proxy/src/auth.rs`.

## Decision

Add optional `access_token_audience` to the strict OIDC configuration. Only omission
uses the existing `client_id` audience. A supplied value is an exact 1–512-byte
string without whitespace, controls or wildcards; explicit null and other JSON
types fail loading. No normalization or URL fetching is performed.

Use the selected value only to validate introspected access-token `aud`, as an
exact scalar or a member of an all-string array. Browser ID-token verification,
authorization/code exchange and confidential introspection retain the configured
OAuth `client_id`. The origin client's `client_id` or `azp` in a resource token
does not have to equal Sandhi's consuming client, and cannot grant a Sandhi role.

The trusted issuer must distinguish access-token purpose from browser ID tokens
and check current resource authority before returning active introspection. For
the separate IDM resource-token design, this includes durable issuance receipt
and live-authority checks; adding an audience option here does not implement
those issuer-side mechanisms. Keep existing HTTPS/same-origin discovery, S256,
signature/nonce/access-token-hash browser checks, expiry, groups, explicit grants,
delegation and revocation behavior. There is no JWT-only access-token shortcut,
failure fallback, token exchange or inference replay.

Victor already has an explicit resource audience in its private token-file
credential. That owner reads fresh broker output, pins issuer/audience/subject
and replaces admitted handles on rotation; it does not log in, refresh or validate
the issuer's token. Cross-service tokens need the intended resource audience and
separate admission. Shared login alone does not authorize forwarding a Victor
token to Sandhi or an upstream provider.

## Qualification boundary

Extend the existing configuration, introspection, confidential Basic and browser
RS256/ES256 test owners. Preserve omitted-field legacy behavior; reject malformed
configuration, wrong/browser-client resource audiences, mixed arrays and using a
resource audience for browser login. Require Basic/form consumer identity to stay
unchanged when the token's originating client is different. Existing failure,
group, subject and replay tests remain applicable. Synthetic local fixtures are
source evidence; actual IDM, Victor broker and deployment acceptance are separate.

Changing `auth.rs` requires renewing the source checksum in the existing scoped
OIDC advisory records. Dependency locks, cryptographic operations and expiry stay
unchanged; this configuration change is not advisory remediation.
