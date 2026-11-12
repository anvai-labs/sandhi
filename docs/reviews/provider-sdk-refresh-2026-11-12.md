# Provider SDK refresh and develop alignment

The provider-client conformance baseline now uses exact stable releases selected
from PyPI on 2026-11-12, shared by local qualification and the public CI job through
[one requirements file](../../tests/sdk-conformance/requirements.txt).

| Provider client | Previous CI range | New exact pin | Stable release source |
| --- | --- | --- | --- |
| OpenAI Python | `>=2,<3` | `3.49.0` | [Official release](https://github.com/openai/openai-python/releases/tag/v3.49.0) |
| Anthropic Python | `>=0.97,<1` | `2.8.0` | [Official release](https://github.com/anthropics/anthropic-sdk-python/releases/tag/v2.8.0) |
| Google GenAI Python | `>=1.73,<2` | `3.2.0` | [Official release](https://github.com/googleapis/python-genai/releases/tag/v3.2.0) |

PyPI release metadata and installed distributions agree on these versions. Their
Python requirements fit the existing Python 3.12 conformance job. Harness packages
keep their previous constraints; provider SDK versions are exact rather than
floating to a different release between local qualification and CI.

## Interoperability changes exposed by the upgrade

Google GenAI 3.x defaults to `/v1/models/...`. The initial matrix passed 12 cases
and returned 404 for all four Gemini-client cells. Both `/v1` and `/v1beta` now
reach the same generation, streaming, token-estimate, authorization, admission and
accounting handlers. No SDK override forces the new client back to the old prefix.
The 30-case Gemini/matrix regression passes with the current SDKs.

The shared `/v1/models` path selects a native discovery envelope from credential
and client/version headers, then uses existing key/route authorization. Anthropic
2.x now sends API keys as Bearer; its `anthropic-version` header selects Anthropic
discovery. Gemini uses its Google headers. Older native credential presentations
remain supported. Explicit credential headers take precedence over a different
client's version hint. An unrelated Bearer request retains OpenAI discovery.

Gemini discovery omits model IDs its single-segment URL cannot call, including
slash-containing backend IDs. OpenAI/Anthropic listings retain those authorized
IDs. A logical alias can map a path-safe Gemini name to that backend model. The
joined discovery regression checks each envelope, Bearer hints, invalid keys,
authorized filtering and zero origin inference calls.

Current SDK majors also use the separate `httpx2` transport namespace. Three
failed-stream tests initially escaped an assertion that accepted only
`httpx.RemoteProtocolError`. The harness accepts the specific failed-body error
from either installed transport, while retaining one-origin-call, original
attempt, partial-usage, held-liability and restart assertions. Their focused
current-SDK rerun passes. This changes the test's exception matching, not source
qualification, inference replay or production deadlines.

## Evidence and limits

The initial broader current-SDK router run passed 205/208 cases and retained the
three transport-namespace assertion failures above. The old SDK environment
(OpenAI 2.18.0, Anthropic 0.79.0, Google GenAI 1.75.0) passes all 34 selected
generation, discovery, matrix and failed-stream recovery cases against the updated
gateway. The proxy's 202 unit tests, Clippy, formatting and strict docs checks pass.
An initial full run completed with 538 passed, three opt-in integration skips and
the Anthropic discovery failure reproduced before its header-hint fix. The
corrected current-SDK discovery rerun passes both the unmodified client and the
shared-path authorization test. This diagnostic run is retained; it is not
counted as final clean qualification.
The final frozen-source run passed **539 tests with three optional skips** in
585.25 seconds, including SDK, real Playwright dashboard and pinned CPU-origin
qualification. Independent review of `f7cd6bf` against `4443ed8` found no actionable
issues and independently passed 17 archived-source gateway cases, including
mixed-header/two-key precedence and zero-inference discovery. Formatting, strict
documentation, generated bindings and six public-runner policy tests passed.
The refreshed checkpoint's public SDK/dashboard, coverage, packaging and docs
passed. Its Rust SentinelPass job reproduced an admission `wait_timeout`, so the
aggregate merge gate failed. The [public failure report](public-ci-admission-timeout-2026-11-12.md)
retains evidence and limits; no blind retry or deadline increase was used.

The full suite uses the CI-pinned InferFlux source
`bfdf32c00ccf40c7af73b86df2eedabc20a7ae85` and llama.cpp submodule
`bb4caa7540188872173c44d161602d9271386413`, exported into a disposable tree and
built with the public CI's CPU-only flags. It does not use the unrelated current
InferFlux checkout as evidence for that pin. These are synthetic local origins;
SDK qualification does not establish new live provider, GPU or subscription
acceptance.

Actual configured-credential scanning checked 552 indexed files and 3,191 locally
reachable blobs for five credential values in 15 raw/common encoded forms, with
zero matches. Gitleaks cleared all 16 branch commits. Provider credentials remain
outside Git; this SDK qualification uses synthetic fixture credentials.

Fresh `origin/develop` remained `4443ed8544404d1fdd944e596141ac615f3f23d9`.
The published routing branch contained it already: zero commits behind and
fifteen ahead before this SDK increment. No rebase or forced history rewrite was
needed. Unfinished streaming-fidelity work remains isolated in its original
worktree and is not included in this SDK refresh.
