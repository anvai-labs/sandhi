# Live GLM buffered fidelity and cache-pair qualification

**Result:** 24/24 SDK calls, source usage joins and paired settlements passed on
2026-11-12. This run uses the updated buffered source-evidence/output-fidelity
implementation in [ADR-0034](../adr/0034-buffered-source-evidence-and-output-fidelity.md),
four real client SDK formats, authorized logical aliases and a disposable tracked
gateway. Its external credential was loaded from the user's existing settings;
settings and persistent credentials were not changed.

| Advertised GLM backend | Calls | Fresh input | Cache read | Cache write | Output | Reasoning |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Chat Completions `/api/coding/paas/v4` | 8 | 3,816 | 26,432 | 0 | 112 | Reported zero |
| Anthropic Messages `/api/anthropic` | 8 | 3,816 | 26,432 | 0 | 101 | Not separately reported |
| Responses `/api/v1` | 8 | 7,592 | 22,656 | 0 | 402 | 290, included in output |
| Total | 24 | 15,224 | 75,520 | 0 | 615 | Inclusion preserved; absence remains unknown |

Each backend received two identical synthetic-prefix requests from Chat,
Messages, Gemini and Responses clients using `glm-5.3`. The route selected the
declared backend/profile and implicit-only cache policy. Every second request in
the 12 pairs reported 3,776 cached input tokens. Twenty of 24 calls reported
positive reads; eight first requests were already warm. The aggregate input read
share is **83.22%**. This measures reported input reuse, not percentage price
savings, guaranteed cold/warm behavior or TTL/expiry enforcement.

All 24 complete execution/context/attempt/usage/receipt joins passed the harness
verifier, as did SDK-to-source token conservation. There were 24 physical attempts,
24 receipts and 48 settled reservations, with clean gateway shutdown. Each receipt
keeps the original accounting-policy valuation; reasoning already included in
output is not charged again. Canonical source accounting keeps missing thinking
counters unknown. Some client-protocol fields use a compatibility default of zero;
those projected fields do not establish measured source reasoning or settlement.
This run used a cache-read weight of 10,000/10,000 and charged 91,359 neutral
tokens in the allowance ledger. Those units do not represent currency savings.

The harness explicitly requested thinking disable in each client protocol.
Responses still reported reasoning work; a request control does not prove the
provider performs no reasoning. This qualifies the observed portable output with
those controls, not default signed-thinking portability, subscription credentials,
native Google inference, stream fidelity or explicit cache-resource lifecycle.
Unsupported state remains an explicit translation refusal.

Reproduce with the existing external credential and installed SDKs:

```bash
cargo build -p sandhi-proxy --bins
python scripts/qualify_zai_router.py --live --accounting tracked --logical-route \
  --cache-profile implicit_only --cache-pairs --delivery buffered \
  --thinking disabled --output /tmp/zai-buffered-cache-pairs.json
```

The [sanitized structural artifact](artifacts/zai-buffered-fidelity-cache-pairs-2026-11-12.json)
contains exact gateway/harness hashes, SDK versions, controls, every join and the
verifier result. It contains synthetic output and no credential or prompt secret.
Default-rule Gitleaks and actual configured-credential scanning cover the artifact
before publication. This disposable qualification did not restart or deploy the
existing operator dashboard.
