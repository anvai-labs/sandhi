# OIDC crypto-bigint advisory remediation

Assessment date: 2026-11-13. Reassessment required before 2026-11-18 or on
recorded dependency, adapter or consumer drift. The existing deadline is unchanged.

[RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387.html)
identifies timing variability in integer operations and lists crypto-bigint
>=0.7.6 as patched. This candidate resolves crypto-bigint 0.7.11 through
openidconnect 4.1.0, p256/p384 0.14.0, elliptic-curve 0.14.3 and primeorder 0.14.0.
The upstream [4.1.0 release](https://github.com/ramosbugs/openidconnect-rs/releases/tag/4.1.0)
explicitly updates the curve family to address this advisory.

The synthetic ES256 test signer also uses p256 0.14. Its fixed disposable key
replaces the old fixture RNG API without changing production key handling.
Consequently the complete lockfile, including development dependencies, contains
one patched crypto-bigint version and no old 0.5.5 copy. Neither binding graph
includes crypto-bigint. The production auth adapter is unchanged.

The RUSTSEC-2026-0387 ignore is removed from deny.toml. The
[machine-readable record](oidc-crypto-bigint-advisory.json) retains exact package
checksums, the adapter checksum and the existing bounded review deadline.
`scripts/check-oidc-advisory.py` rejects a version below the patched floor,
checksum drift, an additional production consumer, a route bypassing the OIDC
owner, and reinstatement of this ignore. Existing expiry and unsupported-use
failure guards remain active. Independent exact-source review and security CI
remain required before adoption.

This replaces the earlier 4.0.1/0.13-curve non-applicability assessment with a
patched dependency graph. It makes no remediation claim for the separate
[RSA advisory](oidc-rsa-advisory.md), whose bounded exception remains necessary.

The postrelease integration binds this review to the exact adapter with the explicit resource-audience selector. This changes introspection audience matching only; independent combined-source reassessment is required before adoption.
