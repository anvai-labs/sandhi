# ADR-0038: Coordinate Victor and Sandhi around one execution contract

- Status: Accepted direction; implementation and qualification in progress.
- Authority: User instruction on 2026-11-12: Victor is the only Sandhi client;
  migrate both rather than maintain a legacy mode for compatibility.
- Tracks: [TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md).

## Decision

Use one source-validation and token-observation pipeline for the four supported
protocol families. The ordinary embedded binding and the durable gateway must
use the same portable projectors and native-frame fidelity rules. Accounting
configuration must not select whether meaningful source output can disappear.

Client dialect, backend protocol/profile and delivery contract remain independent.
OpenAI Chat, Messages, Responses and Gemini are supported interfaces. Portable
delivery explicitly refuses state it cannot represent. Compatible native delivery
preserves original complete frames and ordered opaque state. These two delivery
contracts do not justify two source parsers with different validity rules.

Durable gateway accounting owns reservations, attempt identities, retained
liability and settlement. An embedded binding does not acquire a SQLite ledger
merely to decode a response. It shares response-scoped source evidence with the
gateway; an observation is never itself permission, a receipt or proof of payment.
Usage absence, partial evidence and reported zero must remain distinguishable.

Victor must own retries explicitly at its logical-call boundary. A failed native
binding call never falls back or replays through another transport. Gateway and
embedded paths use retry-free physical handles. Preserve provider-side failures
and source/output-projection failures as distinct terminal facts; a projection
failure cannot authorize another inference or discard measured source usage.

## Migration gates

The coordinated package targets are Sandhi **0.17.0** and Victor **0.29.0**,
with Victor pinning `sandhi-gateway==0.17.0`. These are prepared source versions,
not published artifacts. Keep wire schema v1 unless its shape actually changes;
package versions communicate the changed default execution semantics. Victor's
Contracts and CodeGraph packages retain their independent versions.

1. Replace ordinary permissive Chat/Responses/Messages/Gemini stream dispatch with
   the same response observer/projectors used by accounted dispatch. Remove the
   obsolete implementations and migrate their useful framing/size/error tests.
2. Unify buffered response fidelity and usage semantics, including constrained
   Codex aggregation; do not keep a second permissive aggregation path.
3. Update Victor's binding contract/version checks and retry configuration.
   Validate direct FFI and gateway operation against the exact candidate wheel.
   Expose and qualify explicit nonstandard endpoint usage profiles at the embedded
   boundary; do not silently broaden the standard qualifier to accept them.
   Keep unrelated native-only Victor providers outside this change.
4. Establish one documented gateway accounting default and migrate deployments
   and configuration deliberately. An accounting-off switch must not reactivate
   weaker content semantics. Remove obsolete switches once their owners migrate.
5. Test all four ingress/backend families in buffered and streaming delivery,
   native-state refusal/preservation, tools, cache accounting, cancellation and
   ambiguous dispatch behavior. Preserve earlier unresolved accounting-wait
   evidence; a green migration test does not diagnose that separate failure.
6. Review exact paired commits and publish reviewable PR checkpoints. Merging,
   releasing wheels and upgrading a running service are separate from source
   validation. Use public GitHub-hosted runners exclusively.

No fallback mode will be added to rescue malformed or unrepresentable output.
If a migration test fails, fix the contract or declare a capability unsupported.

## Current evidence and limits

Victor at `8912e69c7` pins `sandhi-gateway==0.16.2` and uses
`TypedProvider.complete_json` / `stream_json`, which reach ordinary provider-handle
methods. Its gateway path already disables handle retries, while its direct path
passes the provider's retry setting. The latter requires migration.

A local synthetic default-stream regression fixture initially failed in five of
six cases: four source families silently accepted nonportable state, and standard
Responses omitted text present only in the completed output. This is source
regression evidence, not live-provider qualification or a completed paired rollout.
