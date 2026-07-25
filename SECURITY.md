# Security Policy

## Reporting a Vulnerability

Sandhi is an open-source **AI usage gateway**. We take security reports
seriously — the proxy holds real upstream provider keys server-side, so a
disclosed vulnerability there is high-impact.

**Please do NOT open a public GitHub issue for security vulnerabilities.**

Instead, report them privately:

- **GitHub Security Advisories** (preferred): go to the
  [Security tab](https://github.com/anvai-labs/sandhi/security/advisories/new)
  and choose **Report a vulnerability**. This lets us collaborate on a fix under
  a private fork before public disclosure.
- Alternatively, email the maintainer directly. The default code owner is listed
  in [`.github/CODEOWNERS`](.github/CODEOWNERS).

Please include:

- A description of the issue and its impact (e.g. key disclosure, auth bypass,
  budget-cap evasion, attribution forgery).
- Affected version / commit and how you ran it.
- Steps to reproduce, including any request that triggers it.
- Suggested fix, if you have one.

## Response expectations

We will acknowledge your report within **5 business days** and aim to ship a fix
or mitigation within **30 days** for high-severity issues. We will credit
reporters in the release advisory unless you prefer to remain anonymous.

## Scope

This policy covers the code in this repository (`anvai-labs/sandhi`), including
the Rust crates (`sandhi-core`, `-providers`, `-store`, `-proxy`) and the Python
/ Node bindings. Vulnerabilities in third-party dependencies should be reported
upstream and coordinated with us if a Sandhi fix (e.g. a version bump) is
needed.

Out of scope:

- The commercial **AnvaiOps** control plane — that is a separate product;
  report through its own channels.
- The neutral **unit** model itself (Sandhi emits tokens / cache split /
  GPU-seconds and **never dollars**; pricing is downstream). A disagreement with
  that measure-vs-price boundary is a design question, not a vulnerability.
- The **unauthenticated dashboard read endpoints** (`/dashboard`, `/api/*`).
  These are unauthed *by design* for a self-hosted deployment and return masked
  values only — do not expose the proxy's admin/dashboard surface to an untrusted
  network. A report that they need no credentials is a known design decision; a
  report that they leak an **unmasked** secret, or that a *write* endpoint is
  reachable without an admin token, is in scope.
- Documented, not-yet-implemented enforcement: per-minute **rate limits** are
  stored but not enforced, and enforcement is **proxy-only** — the in-process
  bindings meter without enforcing. See the CHANGELOG's "Known limitations".

## Supported versions

Only the latest release line receives security fixes. See
[CHANGELOG.md](CHANGELOG.md) and [RELEASING.md](RELEASING.md).

| Version | Supported |
|---------|-----------|
| latest `vX.Y.Z` on `main` | ✅ |
| older releases | ❌ |
| `develop` / feature branches | best-effort only |
