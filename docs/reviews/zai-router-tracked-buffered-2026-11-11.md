# Z.AI tracked buffered routing and cache qualification

**Result:** final run **24/24 successful SDK calls**, 24 correlated durable usage
observations, 24 qualified attempt/usage/receipt joins and no verifier errors.
This is an unreleased source candidate, not deployment acceptance.
Run finished at **2026-11-12 01:49:04 UTC** (2026-11-11 in America/Chicago).

The [active goal](../td/TD-0034-protocol-neutral-router-cache-accounting.md) and
[ADR-0025](../adr/0025-qualified-buffered-protocol-accounting.md) define the scope.
The [earlier live matrix](zai-router-live-2026-11-11.md) exercised legacy accounting;
these new results explicitly exercise logical routes and tracked ownership.

## Executed matrix

OpenAI Chat, Anthropic Messages, Responses and Google Gemini SDKs each made two
buffered requests through an authorized `zai-code` alias to each advertised Z.AI
backend, using `glm-5.3` and a repeated synthetic Python prefix.

| Client protocol | Chat backend | Messages backend | Responses backend |
| --- | --- | --- | --- |
| Chat | 2/2 | 2/2 | 2/2 |
| Messages | 2/2 | 2/2 | 2/2 |
| Responses | 2/2 | 2/2 | 2/2 |
| Gemini | 2/2 | 2/2 | 2/2 |

Endpoints match [Z.AI's advertised coding integration](https://docs.z.ai/devpack/tool/others):

- Chat: `https://api.z.ai/api/coding/paas/v4`.
- Messages: `https://api.z.ai/api/anthropic`, with the explicitly configured
  `anthropic_implicit_cache` endpoint profile.
- Responses: `https://api.z.ai/api/v1`, standard endpoint profile.

The harness used a disposable gateway, store and read-only credential fixture,
with no changes to the user's settings or managed gateways. SDK retries were off;
tracked transports were retry-free. The synthetic prompt requested an identity
function. Credentials and standalone credential-derived fingerprints were omitted
from retained evidence. The disposable database was removed after export; these
artifacts are not production recovery backups.

## Token and settlement evidence

All 24 final calls reported positive cache reads: **8 per backend protocol**.
The input prefix was already warm from earlier qualification runs. These are warm
reuse observations, not proof of cold-cache construction, TTL behavior, worker
pinning or cache portability.

| Observed category | Total |
| --- | ---: |
| Fresh input | 120 |
| Cache-read input | 90,624 |
| Separately metered cache writes | 0 |
| Output, inclusive of reported reasoning | 1,095 |
| Reported reasoning subset | 455 |

Cache reads were **99.87% of observed input**. That share is not a dollar saving,
subscription quota conversion or guaranteed future hit rate. Under the declared
Messages profile, implicit cache construction is accounted within uncached input;
an omitted separate write counter is not presented as an origin-reported zero.
Observed reuse agrees with [Z.AI's automatic-cache mechanism](https://docs.z.ai/guides/capabilities/cache).

The admitted accounting policy used 10,000 basis points for cache reads: neutral
weighted billable units. Reasoning is included in output for these backends and
is not added twice. Each execution joins its authorized route, selected backend,
profile/model/cache context, physical attempt, immutable qualified usage, original
charge and receipt. Each receipt settles the linked client and upstream reservations
with the same amount/timestamp. There are 48 reservations for 24 model calls;
adding both budget legs would double-count consumption.

## Defects and failures retained

| Run | SDK results | Qualified attempt/receipt joins | Finding |
| --- | ---: | ---: | --- |
| Initial tracked run | 16/24 | 16/24 | All eight Messages cells lacked the standard profile's required separate write counter; both reservations remained unsettled and shutdown returned 124 |
| Separate legacy diagnostic | 1/1 | Not applicable | Native Messages usage omitted `cache_creation_input_tokens`; this call did not amend/replay failed executions |
| Explicit Messages profile, before contention handling | 23/24 | 24/24 | One client received accounting 502; its original receipt was present by shutdown |
| Final source run | 24/24 | 24/24 | No verifier errors; clean accounting shutdown |

The first finding led to an explicit, revisioned endpoint accounting profile rather
than inferring missing counters from provider names or weakening standard Messages
qualification. Missing reads and contradictory/nonzero writes still refuse in that
profile. Both forwarding planes and all four clients have synthetic profile tests.

The second failure motivated deterministic outer-ledger contention tests. Owned
accounting now retries only `LedgerBusy`, which proves the outer mutex was never
acquired and no SQL ran, within the original accounting wait and shutdown deadline.
Admission, authorization, closure and settlement retain their exact identity and
evidence. Storage ambiguity is not retried by this loop; neither is inference.
Public one-shot jobs retain their prior behavior.

Those controlled tests prove the busy-lock behavior and cutoff bounds. They do
**not** establish that every historical intermittent stress failure, or the retained
23/24 live run, had that cause: those runs did not retain the necessary reason log.
The harness now exports only bounded generated request IDs and static diagnostic
reason codes. A later green run does not erase either failure.

## Reproduction and retained artifacts

```sh
cargo build --locked -p sandhi-proxy --bins
python scripts/qualify_zai_router.py --live --accounting tracked --delivery buffered \
  --logical-route --cache-pairs --cache-profile implicit_only \
  --output /tmp/zai-tracked-buffered.json
python scripts/qualify_zai_router.py --verify /tmp/zai-tracked-buffered.json
```

`--live` consumes authorized provider quota; verification is offline. The harness
records the binary/harness hashes, endpoint/profile declarations and SDK versions,
and verifies actual correlated SQL evidence before export. Offline source tests
also corrupt receipt amounts, scope, identity, usage and inventory to verify refusal.

| Artifact | SHA-256 |
| --- | --- |
| [Final tracked matrix](artifacts/zai-router-tracked-buffered-cache-pairs-2026-11-11.json) | `5737699a1e433773e54e5d78eb523fa11412661b5f3c37d6cd14bd8d684cbe1f` |
| [Initial profile failure](artifacts/zai-router-tracked-buffered-cache-pairs-initial-failure-2026-11-11.json) | `70f0757ca78eda3ebbabe3dda2ff4b692e7e4d34eeacf3b60978893ef434e33c` |
| [Native usage diagnostic](artifacts/zai-router-messages-usage-diagnostic-2026-11-11.json) | `7919c65df6a2d4c815da46355c79212ca6539fe0cb9b99f149f63bd803db28a0` |
| [Retained accounting-response failure](artifacts/zai-router-tracked-buffered-cache-pairs-settlement-response-failure-2026-11-11.json) | `4b3c4bb960a3195d2c6a7ab77741f3bfba8c056d2b639b113883d3d0a50866c2` |

Final gateway binary SHA-256:
`f08fc912396ecc36dbb62f828e3a0c9c822e28c1c53886b775144f9c9ca9faac`.
SDKs: OpenAI 2.18.0, Anthropic 0.79.0, google-genai 1.75.0.

## Limits

No live Gemini upstream, Codex/ChatGPT subscription or Anthropic subscription is
qualified here. A Gemini **client** was translated to each of the three Z.AI
backends. Full agent/tool/media loops, issuer provenance, additional strict stream
observers, constrained Codex aggregation, subscription lifecycle, export and
deployment remain open. This result does not complete M5, M6 or the overall goal.
