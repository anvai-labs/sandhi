# InferFlux usage corpus

`complete.json` and `stream.sse` are the existing tinyllama captures. They remain
unchanged, including the fully cached 50-token prompt and late terminal SSE usage.

`cache-audit-2026-09-18.json` preserves the 18 usage objects from the credential-free
Qwen/ROCm audit (six plain and twelve tools/JSON/logprobs probes). It intentionally
omits prompts, completion content, credentials and live correlation identifiers.
The source evidence SHA-256 is embedded in the fixture. Expected neutral counts and
rounded durations are explicit regression expectations, not another provider parser.

The audit recorded **non-streaming usage projections**, not complete response bodies
or SSE captures. The tests construct clearly synthetic JSON envelopes and terminal
SSE frames around those same usage objects. They exercise both adapter and raw
forwarding paths, including byte preservation, late usage after finish_reason,
exactly one finalized usage emission, and catalog-derived correlation/session headers.
They do not establish live SSE/model behavior or diagnose the original member run.

The `sandhi-cold-0` label is retained from the audit: it means the first gateway
request, which followed direct calls and was already warm (426 cached tokens), not
a forced cold-cache observation. Tools alone did not disable caching. JSON/logprobs
zeros record those specific probe results, not a permanent capability declaration.

For every case, fresh + cached = inclusive prompt; fresh + cached + output = total.
Cache writes remain zero for this OpenAI-compatible accounting convention, which
does not mean the upstream created no KV state. These are neutral units, not dollars.

See [the handoff](../../../../../docs/upstream/inferflux-cache-codesign-2026-09-18.md)
for live topology, evidence limits and the remaining availability/dashboard/export work.
