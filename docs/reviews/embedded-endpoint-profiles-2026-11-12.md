# Embedded endpoint profile checkpoint

This source candidate implements [ADR-0039](../adr/0039-shared-embedded-endpoint-profiles.md):
one Rust constructor shared by Python and Node, explicit protocol/profile selection,
family-specific authentication and provider identity independent of protocol.
The backend registry reuses the same non-secret profile type with unchanged stored
names. Victor carries profile selection in its persistent handle identity and
rejects per-call overrides before transport.

## Source validation

The candidate wheel passed **107 Python binding tests**; the Node addon passed
**65 tests**. These include real local HTTP fixtures for nonstandard Chat and
Messages conventions, standard Responses/Gemini with arbitrary provider labels,
actual protocol authentication, disjoint counts and standard-profile refusal.
Core/providers/store passed **731 tests**, with four opt-in cases ignored across
25 suites. Default and ONNX/SentinelPass all-target workspace Clippy, formatting,
generated binding facades, version alignment and strict MkDocs passed.

Victor's provider and adjacent agent/core resilience suites passed **1,932 tests**
with five skips against the candidate wheel. Black/Ruff, typing for all three
changed modules and full collection (**40,170 tests**) passed. Victor remains
local and unmerged because `sandhi-gateway==0.17.0` is not published. The inherited
native 0.28.1 wheel warning is not certification of Victor's prepared 0.29.0 native
artifact or the supported packaging matrix.

The full workspace run is **failed evidence**: 1,016 passed and one failed before
Cargo stopped at the proxy suite. The retained back-leg test failure timed out in
admission's store reserve call before dispatch. The
[correlated diagnostic artifact](artifacts/accounting-admission-wait-2026-11-12.json)
and [accounting wait report](accounting-running-wait-2026-11-12.md) preserve its
identity and limits. Changed-crate passes do not replace a full green run or
diagnose that reliability defect. No wait increase, weaker durability or inference
replay was added.

## Live direct-source evidence

The coding endpoint's models listing returned HTTP 200 and listed `glm-4.6`.
Two subsequent tiny **direct FFI streaming** calls used the external configured
credential, zero retries, 32 output tokens maximum, disabled thinking, a 30-second
operation bound and 10-second idle bound. Each returned text, exactly one final
usage event and a validated finish:

| Endpoint | Explicit profile | Fresh | Read | Write | Output |
| --- | --- | ---: | ---: | ---: | ---: |
| `/api/coding/paas/v4` Chat | `openai_terminal_chunk_usage` | 16 | 0 | 0 | 3 |
| `/api/anthropic` Messages | `anthropic_implicit_cache` | 16 | 0 | 0 | 3 |

The [sanitized artifact](artifacts/embedded-endpoint-profiles-2026-11-12.json)
pins factory and wheel hashes. No credential, raw model response, header or error
body was exported. Chat also reported two reasoning tokens; this export did not
select the canonical inclusion flag, so it makes no derived billable-total claim.
Missing Messages reasoning is kept absent. These small calls establish no cache
hit, cold/warm guarantee, price saving, gateway receipt join or subscription claim.

## Remaining qualification

Streaming direct-source profiles are the implemented scope. Ordinary buffered
binding calls still bypass strict usage qualification. Gateway clients still need
an explicit second-hop counts contract across native and translated routes and
one owner for cache-intent compilation. Buffered unification, gateway accounting
default retirement, the spontaneous accounting waits and the broader cache,
agent/media and issuer/subscription gates remain open in TD-0034. Prior public
CI success belongs to `a74a8f1`; it does not qualify this subsequent source delta.
