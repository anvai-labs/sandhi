# Credential audit at the reviewed router checkpoint

At code checkpoint `e728102`, a value-aware scan checked 558 indexed files and
3,215 locally reachable historical blobs for five externally configured credential
values in 15 raw/common encoded forms. It found zero matches. The loader reads
credentials outside Git in memory; neither their values nor transformed forms
are included in reports. The configured ZAI credential remains external.

Default-rule Gitleaks found zero issues across the 19 router-branch commits.
An extended scan of all 490 locally reachable commits reported seven historical
findings. A separate reviewer independently classified each against its exact
historical source:

| Count | Rule/location | Classification |
| --- | --- | --- |
| 2 | `generic-api-key`, `scripts/check_brand_tokens.py:25` at `bc2d9b3`/`d90b5d5` | CSS integrity hashes; each matches the SHA-256 of that commit's tracked `tokens.css` |
| 2 | `private-key`, `tests/fixtures/tls` at `9c16571` | Intentional private-key test materials: the localhost TLS identity and a deliberately mismatched key, referenced by tests |
| 3 | `curl-auth-header`, operator guide at `60f0074`/`6af9135` | Literals matching the preceding local admin setup or synthetic virtual-key JSON example |

No provider credential leak was identified among those findings. The TLS fixtures
are genuine private-key material intentionally public for tests; the classification
does not treat them as absent. No scan suppressions were added, and no repository
history or external credentials were rewritten.

These are two separate results: zero configured-value matches, and seven generic
historical findings independently classified. Neither scanner proves that an
arbitrary credential could never have existed in history. The reports retain
scope and classification without publishing credential values.
