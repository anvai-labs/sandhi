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

## Explicit vendor final-choice profiles (2026-11-13)

The proposed catalog-selected profiles in PR #362 are replaced by endpoint
contracts `zai_final_choice_usage` and `deepseek_final_choice_usage`. These require
Chat Completions and static credentials, just like the generic terminal profile;
provider names, catalog membership and URLs never select a usage contract.

Both require one index-zero terminal choice and retain the existing strict
counter, cache-conservation, framing, conflict and completion checks. Unlike the
generic profile, neither accepts an empty-choices usage event. ZAI permits an
omitted `object`, but rejects explicit null or incorrect objects. Its reasons are
stop, length, tool_calls, sensitive, model_context_window_exceeded and network_error.
DeepSeek requires the usual chunk object and permits stop, length, tool_calls,
content_filter, insufficient_system_resource and aborted. Its terminal delta must
carry no new content, reasoning_content, reasoning, refusal or tool_calls: absent,
null and correctly typed empty values qualify. Role metadata may be null or
assistant; all unknown terminal delta keys and malformed values are refused.
After either vendor profile accepts its sole final usage choice, only `[DONE]`
may follow. Further data, including duplicate totals, invalidates qualification
and retains the first source measurement as partial evidence.

The reason sets follow the primary [ZAI Chat reference](https://docs.z.ai/api-reference/llm/chat-completion)
and [DeepSeek Chat reference](https://api-docs.deepseek.com/api/create-chat-completion/),
reviewed 2026-11-13. DeepSeek documents the final usage-bearing choice as carrying
no new content. ZAI's absent-object admission preserves the earlier proposed
compatible envelope; this review does not claim a fresh live-provider probe.

Configuration reaches the shared factory, proxy reconstruction and transparent
source observer. Distinct `v1-chat-zai-final-choice` and
`v1-chat-deepseek-final-choice` usage conventions retain the exact interpretation
when composing trusted gateway peers. Older peers reject unknown conventions;
they must not silently substitute generic terminal qualification. No credential
authority, replay, count inference, settlement shortcut or new retry owner is added.
