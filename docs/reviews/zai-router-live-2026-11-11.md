# Live Z.AI protocol and cache qualification — 2026-11-11

Status: source candidate, tested live with user-authorized synthetic coding prompts.
No release, deployment, user-setting change or external credential mutation.

## Result

The final `glm-5.3` text matrix passed **24/24** cells. The final long-prefix cache
matrix passed **48/48** calls. Each final SDK result's normalized input/cache/output
split matches exactly one drained SQLite observation with its synthetic step ID.
All 48 long-prefix calls reported 3,776 cache-read tokens and 5 fresh input tokens.
These results qualify this setup's text interoperability and implicit cache reads;
they do not complete [TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md).

| Client protocol | Z.AI Chat | Z.AI Messages | Z.AI Responses |
| --- | --- | --- | --- |
| OpenAI Chat | Buffered + streamed passed | Buffered + streamed passed | Buffered + streamed passed |
| Anthropic Messages | Buffered + streamed passed | Buffered + streamed passed | Buffered + streamed passed |
| Gemini generateContent | Buffered + streamed passed | Buffered + streamed passed | Buffered + streamed passed |
| OpenAI Responses | Buffered + streamed passed | Buffered + streamed passed | Buffered + streamed passed |

The same 24 protocol/mode combinations each have two passing long-prefix calls.

## Setup and reproducibility

The endpoints match the user's supplied table and
[Z.AI's advertised integration endpoints](https://docs.z.ai/devpack/tool/others):

| Backend | Base URL | Sandhi endpoint family |
| --- | --- | --- |
| Chat | `https://api.z.ai/api/coding/paas/v4` | `openai_chat_completions` |
| Messages | `https://api.z.ai/api/anthropic` | `anthropic_messages` |
| Responses | `https://api.z.ai/api/v1` | `openai_responses`, standard profile |

All three reference one configured static Bearer credential through distinct
non-secret backend definitions at the same authorized origin. Responses does not
use or impersonate a ChatGPT/Codex OAuth lease.

[`scripts/qualify_zai_router.py`](../../scripts/qualify_zai_router.py) reads the
existing credential into a private disposable credstore, launches a fresh gateway,
registers the three backends and mints virtual keys. Stores, credentials and logs
are removed after clean process shutdown. The runner never edits Claude settings
or a user's running gateway. SDK retries are disabled. The actual model is
`glm-5.3`, without a client-only context suffix. Clients used:

| SDK | Installed version |
| --- | --- |
| openai | 2.18.0 |
| anthropic | 0.79.0 |
| google-genai | 1.75.0 |

Both OpenAI APIs use the real OpenAI SDK. The ordinary Anthropic and Google SDKs
submit their own request shapes; they do not handcraft an OpenAI payload.
Output is bounded to 1,024 tokens. The short prompt requests a Python identity
function. Cache pairs prepend 400 synthetic Python function definitions and repeat
the identical request. Reports contain binary hashes, SDK results, per-cell timing,
sanitized errors and allowlisted durable observations. `--verify report.json`
rechecks a report without credentials or network calls.

## What the live tests found and fixed

Z.AI's Messages stream initially reports zero input/output placeholders, then
includes fresh input and cache-read counts in a terminal `message_delta`. Sandhi
previously read input/cache only at `message_start`. Text succeeded while streaming
accounting silently lost the actual input/cache split on both forwarding planes.

The fix merges valid, present cumulative fields from either start or delta usage;
an output-only update retains earlier input/cache values. It uses the existing
plausible-count parser and preserves explicit zeros. The red unit test reproduced
`(fresh=0, read=0, write=0, output=67)` where the terminal frame reported
`(5, 3776, 30, 67)`. Regression tests now cover early and late input/cache usage
across all four ingress protocols, native and translated streams, and once-only
settlement of both budget legs. The rebuilt live gateway passes the accounting
comparison for every final cell.

An initial harness bug also assumed every Anthropic stream event had usage; two
cells raised a collector `AttributeError`. The collector now handles absent usage.
This is retained separately from product failures.

## Token evidence and cost interpretation

| Final long-prefix evidence | Chat backend | Messages backend | Responses backend |
| --- | ---: | ---: | ---: |
| Calls / matching durable observations | 16 / 16 | 16 / 16 | 16 / 16 |
| Positive reported cache reads | 16 | 16 | 16 |
| Inclusive input per call | 3,781 | 3,781 | 3,781 |
| Fresh input per call | 5 | 5 | 5 |
| Cache-read input per call | 3,776 | 3,776 | 3,776 |
| Cache-write count in event | 0 | 0 | 0 |

The final run was already warm. In the retained pre-fix run, the first Chat call
reported zero reads, followed by 3,776 reads on the identical second call. There
is no guarantee of a cold cache or isolated worker. One earlier Responses probe
also reported a miss. Latency is not used as cache evidence.

The measured read share is **99.87% of input**, not a percentage cost saving.
Provider prices, output, retention and subscription rules determine commercial
impact. No positive cache-write measurement, cache creation charge, TTL expiration
or guaranteed dollar saving was established. Where a native SDK omits a cache-write
field, a stored numeric zero is not proof of reported zero writes.

The cache run declares `implicit_only` with default `auto`, matching
[Z.AI's documented automatic mechanism](https://docs.z.ai/guides/capabilities/cache).
The baseline declares `native_only` without a default, which selects preserve-native.
Neither injects unqualified OpenAI Platform or Anthropic TTL controls.

## Retained evidence, including unsuccessful runs

| Artifact | Outcome |
| --- | --- |
| [Final text matrix](artifacts/zai-router-matrix-2026-11-11.json) | 24 successful SDK cells, 24 correlated observations and 24 usage matches |
| [Final cache pairs](artifacts/zai-router-cache-pairs-2026-11-11.json) | 48 successful calls, 48 correlated observations, 48 usage matches and 48 reported hits |
| [Post-fix timeout run](artifacts/zai-router-timeout-run-2026-11-11.json) | 23 successful cells; Chat client → Messages streaming hit the 90-second upstream deadline. A separate subsequent full matrix passed; cause not established. No automatic inference replay was added. |
| [Pre-fix text accounting](artifacts/zai-router-before-usage-fix-2026-11-11.json) | 24 text successes, but all four Messages streaming cells have invalid empty input measurements; native Messages SDK evidence also disagrees with storage |
| [Pre-fix cache accounting](artifacts/zai-router-cache-before-usage-fix-2026-11-11.json) | 48 text successes, but eight Messages streaming calls lose input/cache counts; native SDK terminal usage reveals the mismatch |
| [First collector run](artifacts/zai-router-first-run-2026-11-11.json) | 22 collected successes and two collector errors; not an accounting acceptance run |

Every unsuccessful run stays visible. A successful rerun establishes a passing
journey; it does not prove the earlier timeout cannot recur. The runner validates
known nonempty synthetic input as well as client/storage agreement so that matching
fabricated zeros cannot pass acceptance.

## Remaining qualification

- Native Gemini backend/Interactions/cache-resource lifecycle; ChatGPT/Codex and
  Anthropic subscription profiles; different models/accounts/regions.
- Explicit TTLs, expiration, minimum-prefix boundary, cache writes and namespace
  isolation. Synthetic compiler tests do not substitute for these live cases.
- Tool loops, media, reasoning-state replay, cancellation, restart and switching.
- Uniform durable attempt/backend/profile attribution and unknown-liability handling.
  Current legacy usage records identify `backend:external`; this is not the explicit
  backend ID. Synthetic step IDs correlate this harness, but do not supply M5's
  missing execution dimensions or prove once-only physical inference under failure.
- Broader provider reliability. The retained live timeout and the previously
  observed settlement-contention stress failures remain unresolved qualifications.

See the [visual product guide](../product/cache-aware-routing.md),
[operator configuration](../operator/backends.md) and
[active goal](../td/TD-0034-protocol-neutral-router-cache-accounting.md).
