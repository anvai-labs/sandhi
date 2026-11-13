# Native reference ownership checkpoint

This increment closes a confirmed shared-credential authorization gap. The retained synthetic two-client regression established independent pins for two minted virtual keys. Before the fix, the second key could reuse the first key's Responses response and item IDs and receive the simulated first tenant's history. A pin was the only stateful-route check. Fixed-backend calls did not require a pin at all.

The new registry authorizes known native references before routing confirmation, budget admission or provider dispatch. It uses authenticated scope and the frozen backend/account/endpoint/model binding, rather than caller hints or descriptive provider labels. The original namespace migration, budgets, holds, execution contexts and affinity/cache fingerprints remain unchanged.

## Supported proof and refusal boundary

| Input | Issuer evidence or boundary |
| --- | --- |
| Responses `previous_response_id`, saved item IDs and `item_reference` | Actual native response/item IDs returned under the same owner and frozen binding. |
| Responses/Codex encrypted reasoning | Actual native encrypted value and any item ID, including constrained Codex buffered SSE aggregation. Codex server lookup remains unsupported by its existing profile. |
| Messages thinking/redacted thinking | Actual returned signature or redacted value. Fragmented signature frames wait for complete-block proof before exposure. |
| Gemini thought signatures | Actual returned native part signature, preserving the original frame. |
| Chat audio state and known reasoning envelopes | Actual returned audio ID or opaque native value. Plain reasoning prose is not treated as a resource. |
| Structured file references and provider containers | Require a matching server-observed proof. Unobserved/pre-created references refuse. |
| Gemini cached content/file-search/Vertex retrieval, Responses saved prompts/vector stores/connectors, and custom Messages skills | No implicit adoption from a route pin, URL, provider label or cache namespace. Without explicit issued proof these selectors refuse; this checkpoint provides no external adoption API. |
| Fully materialized portable history | May project into another family when the existing native-input fidelity fence allows it; metadata IDs that are not sent upstream do not become resource authority. |

Known structured fields are inspected without parsing JSON-looking user prose or function arguments. Recognized private selectors include nested tool-result/file parts and Gemini function-response file parts. Public creation controls, such as a new automatic code container, do not require an existing resource proof. Unknown future/custom protocol resource fields require a codec/resource-contract review; this is not a claim that arbitrary vendor extensions are resource-free.

Proof storage is digest-only and bounded as described in [ADR-0044](../adr/0044-native-reference-ownership.md). A server-private `Response.extensions` marker is set by the existing native/rendered emission decision. Neither an upstream nor a caller can create it through an HTTP header. Only actual native output is eligible for issuance; canonical rendered response IDs are excluded.

Self-contained encrypted reasoning, signatures and redacted blocks may be independently reissued with identical bytes (for example from a provider cache). Such source issuance adds a second owner-specific proof without transferring or revoking the first. The other owner cannot submit the value before its own issuance; no input echo or caller hint is imported. Resource/item/audio IDs retain single-owner collision refusal, including a provider returning an ambiguous ID to another caller. This is a conservative qualification boundary for duplicate IDs, rather than a silent cross-owner grant.

## Evidence

This report and its artifact preserve the original candidate evidence. Independent review found bootstrap, account-renewal, expiry-cleanup and selected-backend payload gaps in that candidate. The [follow-up report](native-reference-followup-2026-11-12.md) describes their corrections and separate validation; these earlier results alone do not qualify the integrated implementation.

The [artifact](artifacts/native-reference-ownership-2026-11-12.json) pins the isolated source, local logs and frozen executable. The implementation is based on archived `12d7291d2bdeb03d2dee4f35217ca4b82a239c2c` plus the separately authored provider-identity migration. It has not been independently reviewed or committed by this author. No full workspace, live vendor, client SDK suite or Windows runtime qualification is claimed here.

Focused tests cover same-owner native state over fixed/logical routes and buffered/streamed delivery for Responses, constrained Codex, Messages, Gemini and Chat. They retain own reuse after reopening the database and reject the other owner before another dispatch or reservation. The same matrix checks unobserved external resources. Separate forced-write failures preserve one physical successful source attempt, Final five-input/two-output usage, one paired receipt and two settled leases while delivery fails without automatic retry. Storage failures do not turn source success into a fabricated provider rejection.

An additional eight-cell HTTP regression covers independent identical opaque reissuance across Responses, Codex, Messages and Gemini in both delivery modes. Its origin verifies that the second owner's issuance request is materialized and contains neither the first owner's encrypted value nor signature. Foreign-before-issuance input refuses; both owner proofs remain usable, including restart. The focused HTTP total is 210 requests: 146 physical source sends and 64 refusals before admission.

Unit regressions cover owner and every frozen binding dimension, key/credential/reference expiry, unavailable/exhausted storage, scoped collision rollback, global/owner/batch capacity, expiry cleanup progress, byte-preserved SSE framing across every split and all newline forms, signature retention before exposure, pre-mutation bounds, rendered-ID exclusion and first-refusal preservation while draining a later failing source.

A separate actual-process TCP regression uses the frozen private gateway binary and two minted synthetic keys. Foreign response/item references refuse with no additional origin call; the first key's own response/item reuse succeeds before and after restart. It does not use live accounts or provider credentials.

## Operational limits

Native continuation needs the durable registry. A host composition without it cannot authorize native references; it cannot manufacture proof from client metadata. The shared execution-owner initializer opens the registry from its actual original single-file ledger path before publishing the owner. Both the binary and public library serving refuse startup before binding if initialization fails; read-only app construction retains health/readiness probes. Historical objects without proof must be materialized again or await a separately reviewed operator adoption capability. A valid proof proves source-observed ownership under a binding, not provider availability, creation time, deletion, final usage or price.

A full registry may refuse output after a billed provider call. Do not retry that inference automatically. Proof failure drains the connected original source under its existing bounded owner; an early client disconnect may still retain unknown liability. Keep the registry with the original database during stopped-writer backup/restore, and quiesce old binaries for rollout. The existing spontaneous accounting waits, native Python exit occurrence, original shutdown timing occurrence and broader protocol/resource/subscription/cache lifecycle gates remain open.
