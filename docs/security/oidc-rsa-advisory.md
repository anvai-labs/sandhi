# OIDC RSA advisory applicability

Assessment date: 2026-09-22. Reassessment required before 2026-10-22, on dependency
or adapter drift, or before introducing another RSA/OIDC consumer. This is an
explicit non-applicability assessment, not a fix for the dependency vulnerability.

[RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) remains
unpatched in rsa 0.9.10 and the current 0.10 prerelease. Its private-key timing
attack does not apply to the reviewed production operation:

1. Sandhi's auth adapter calls `openidconnect` 4.0.1 ID-token verification.
2. That library's `core/crypto.rs::verify_rsa_signature` constructs `RsaPublicKey`
   from the public JWKS modulus/exponent and invokes public-key verification.
3. rsa's `pkcs1v15::verify` uses public exponentiation. No production private RSA
   key, private signing/decryption, JWE or client assertion is used by this path.
4. Private RSA operations in `auth/tests.rs` belong to the `cfg(test)` mock IdP
   and use a committed, public, disposable fixture key. They are not a deployed
   signing authority or a confidentiality boundary.

An independent adversarial review checked these call paths and the normal
dependency graph. The [machine-readable record](oidc-rsa-advisory.json) pins the
reviewed packages/checksums and auth adapter. `scripts/check-oidc-advisory.py` runs
before cargo-deny in CI, rejects expiry, dependency route/version/checksum changes,
adapter changes and ordinary new use sites, and checks both binding graphs too.
The supplemental source-pattern check is not a formal call-graph proof. Changes
to build-time code generation or operation reachability require fresh review.
Do not regenerate the record mechanically to make a failed check pass.

`deny.toml` exempts only this advisory. Every other vulnerability and yanked-crate
failure remains fatal. A patched maintained dependency should replace this
exception when available. Replacing the OIDC protocol library with custom claim
orchestration was rejected for this increment because it introduces new
security-sensitive logic without removing an exercised private-key operation.

Run the paired checks locally:

```sh
python3 scripts/check-oidc-advisory.py
cargo deny --locked --all-features --config deny.toml check advisories
cargo deny --locked --all-features --manifest-path bindings/python/Cargo.toml --config deny.toml check advisories
cargo deny --locked --all-features --manifest-path bindings/node/Cargo.toml --config deny.toml check advisories
```
