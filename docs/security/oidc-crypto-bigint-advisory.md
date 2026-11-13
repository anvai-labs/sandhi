# OIDC crypto-bigint advisory applicability

Assessment date: 2026-10-25. Reassessment required before 2026-11-18, on dependency
or adapter drift, or before introducing another ECDSA/OIDC consumer. This is an
explicit non-applicability assessment, not a fix for the dependency vulnerability.

[RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387.html) reports
timing variability in crypto-bigint integer operations that are intended to be
constant-time. The affected crypto-bigint 0.5.5 does not apply to the reviewed
production operation:

1. Sandhi's auth adapter calls `openidconnect` 4.0.1 ID-token verification.
2. That library verifies ECDSA signatures with the RustCrypto 0.13 curve
   family — `p256` 0.13.2 (ES256) and `p384` 0.13.1 (ES384), both built on
   `elliptic-curve` 0.13.8 (with `primeorder` 0.13.6), whose arithmetic uses
   crypto-bigint 0.5.5 transitively. No sandhi code touches crypto-bigint,
   the curve crates, or elliptic-curve directly.
3. The graph check in `scripts/check-oidc-advisory.py` enforces containment
   for this leg: crypto-bigint must be absent from both binding graphs,
   present exactly once in the root production graph, and every production
   consumer chain must reach the workspace solely through openidconnect into
   sandhi-proxy. The record additionally pins the version+checksum of every
   crate on the route (openidconnect, p256, p384, primeorder,
   elliptic-curve, crypto-bigint), so any silent route drift fails the gate.
4. The advisory's attack primitive is high-resolution timing observation of the
   verification computation. The reviewed usage verifies tokens from the homelab
   IdP at the gateway's trust boundary; an attacker positioned to measure that
   timing is already inside the boundary the verification defends. This is the
   same reviewed-usage threat model as
   [RUSTSEC-2023-0071](oidc-rsa-advisory.md), sharing the same adapter.

The locked openidconnect 4.0.1 pins the 0.13 curve family. The advisory's fix
requires crypto-bigint >=0.7.6 and the 0.14 curve family (elliptic-curve / p384
>=0.14). Newer dependency candidates require their own graph and operation
reassessment; this record does not claim 4.0.1 is still the latest release or
qualify an upgrade. The resolution path is a qualified dependency change, not a
renewed ignore.

The [machine-readable record](oidc-crypto-bigint-advisory.json) pins the
reviewed packages/checksums and the auth adapter, and is enforced by the same
CI gate (`scripts/check-oidc-advisory.py`) that runs before cargo-deny. Do not
regenerate the record mechanically to make a failed check pass.

The 2026-11-13 audience candidate changes configuration validation and the
introspection response's expected access-token audience only. Browser signature
verification, OAuth client authentication, dependency locks and private-key
operation reachability are unchanged. The source-bound JSON record carries the
new adapter checksum; its expiry remains **2026-11-18**. This is not advisory
remediation or a claim that a different dependency version is covered.
