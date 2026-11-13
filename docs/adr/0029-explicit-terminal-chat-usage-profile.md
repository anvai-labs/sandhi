# ADR-0029: Declare terminal-choice Chat usage placement explicitly

- **Status:** Workspace candidate; qualification evidence is recorded in
  [TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md).
- **Extends:** [Endpoint profiles](0016-explicit-backend-endpoint-binding.md)
  and [qualified translated streaming](0028-qualified-translated-stream-accounting.md).

## Observation

A live GLM coding Chat stream returned valid request-total counters on its single
final choice, followed by `[DONE]`. Standard tracked Chat qualification accepts
usage only on an empty-choices chunk. The SDK could parse the provider response
while accounting correctly retained unknown liability under the incompatible
contract. Provider identity, endpoint URL and cache-policy capability do not
justify silently changing that contract.

## Decision

Add `endpoint_profile: openai_terminal_chunk_usage`, valid only for Chat
Completions with API-key/static Bearer credentials. The default `standard` profile
and public standard qualifier retain their existing rules. This profile permits
both standard empty-choices totals and a single terminal choice carrying totals:

- Exactly one choice, with numeric index zero and an object delta.
- A known terminal reason: stop, length, tool_calls, function_call or content_filter.
- The existing required integer counters, cache conservation, reasoning subset
  and total validation. Usage on a progress choice is refused.
- Identical repeated totals are idempotent; conflicting measurements fail.
- Complete-event framing and `[DONE]`/EOF qualification remain independent.
  Finish reason alone never creates usage or authorizes settlement.

The explicit profile reaches both the transparent forwarder and typed transport.
Registered backends persist it in their definition and frozen backend context; it participates
in route affinity fingerprints and cannot be changed underneath an existing pin.
A restart reconstructs the same source observer. It grants no OAuth/subscription
codec bypass and changes no cache controls, prices, allowance weights or inference
retry policy.

This profile is not cumulative Chat progress accounting, arbitrary multi-choice
accounting or a provider-name heuristic. Additional placements require their own
qualified semantics. The GLM live harness declares this profile for its Chat
backend, the separate implicit-cache profile for Messages and standard Responses.

## Evidence and limits

Core and complete-event observer regressions cover the observed placement,
unchanged standard refusal, malformed/progress/multi-choice input, conflicts,
truncation, missing terminators and every byte split. HTTP, SDK and restart tests
exercise configuration through execution and paired settlement. Live results
remain endpoint/model/checkpoint specific; see the retained negative and corrected
matrix evidence in TD-0034. They do not establish OpenAI Platform behavior or
Codex subscription acceptance.
