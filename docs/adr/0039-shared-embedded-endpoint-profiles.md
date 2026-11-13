# ADR-0039: Share explicit endpoint configuration across embedded bindings

- Status: Implemented source candidate; direct binding qualification in progress.
- Tracks: [TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md) and
  [coordinated Victor migration](0038-coordinated-victor-contract-migration.md).

## Decision

Python and Node use one Rust constructor, `ProviderRuntime::configured_provider`.
It validates protocol, endpoint profile, authentication and representable timeout
bounds before constructing the existing family transport. It adds no dispatcher,
retry owner, ledger or alternate response parser. Provider identity remains
separate from the selected wire protocol; a custom Messages endpoint need not
pretend to be Anthropic.

The non-secret `EndpointProfileV1` lives in `sandhi-core`. The backend registry
reuses that type without changing its stored names. Profiles describe source
semantics, not credential authority:

| Profile | Required protocol | Meaning |
| --- | --- | --- |
| `standard` | Any supported family | Use that family's standard source qualifier. |
| `openai_terminal_chunk_usage` | Chat Completions | Also qualify totals reported on a terminal choice; arbitrary intermediate usage remains invalid. |
| `anthropic_implicit_cache` | Messages | Use the explicitly declared compatible endpoint's implicit-cache convention; missing cache-write fields are not interpreted using standard Anthropic semantics. |
| `chatgpt_codex` | Responses | Select the existing constrained Codex transport, including disabled raw forwarding. |

`protocol` can explicitly select Chat Completions, Messages, Responses or Gemini,
independently of the provider label. Cohere and Ollama remain available as backend
families. Omitted protocol uses the existing provider defaults. Unknown labels
need an explicit base URL. Known Chat catalog defaults retain model-specific URL
selection. Authentication is validated against the selected family: `api_key`
is valid for Messages/Gemini; their `bearer` option selects Bearer authentication.
Other families use their existing authentication conventions.

`chatgpt_responses` / `codex_responses` selects the Codex profile and refuses a
contradictory override. Explicit `responses` with `chatgpt_codex` selects the same
constrained implementation. The gateway's credential scheme, endpoint, account
and expiry checks remain separate and mandatory; this constructor grants no new
vault authority. Profiles are constructor configuration and never model-body
parameters. Neither protocol nor profile is inferred from credential contents.

Both binding APIs append the optional profile parameter after existing arguments.
Python calls it `endpoint_profile`; Node calls it `endpointProfile`. Victor includes
the selected profile in its persistent handle cache key, rejects per-call profile
overrides and explicitly configures its direct Z.AI Chat provider with the terminal
usage profile. Explicit `standard` remains available for independently qualified
endpoints. Direct handles continue using zero hidden retries.

## Qualification and remaining gates

Real local HTTP streaming binding fixtures exercise authentication by actual protocol,
provider-label independence, disjoint token counts, exactly one final usage event,
and absence of profile configuration in the model body. The same nonstandard
source is refused under `standard`; incompatible profile/protocol pairs fail at
construction. These are synthetic direct-binding tests, not live-provider or
packaging-platform certification.

Ordinary buffered binding calls still bypass strict usage qualification. Exposing
the same constructor profile does not close that separate buffered migration gate;
these usage qualification results cover streaming direct-source calls.

The profile describes the **immediate HTTP source**. This does not solve the
gateway's second-hop contract. Native forwarding can preserve a selected backend's
nonstandard usage fields, while translated output has the client's standard shape.
A client connected to a router cannot assume one backend's profile remains valid
after selection changes. In particular, blindly choosing `standard` or copying an
implicit Messages profile onto every gateway client is unsafe.

That migration gate needs an explicit gateway-client counts contract. Any future
response metadata must be trusted only for an explicitly configured gateway peer,
describe the actual emitted usage convention, validate against the client family,
and never activate Codex credential authority. Do not normalize native frames by
silently dropping opaque state or manufacture absent cache counts. Cache-intent
compilation also needs one clearly defined owner across the two hops. This ADR
does not add automatic profile detection or claim that gateway switching is fully
qualified.

Buffered source/aggregate unification, accounting-default retirement, spontaneous
accounting-wait diagnosis, broader cache lifecycle and issuer/subscription gates
remain in TD-0034. Sandhi 0.17.0 / Victor 0.29.0 are prepared source versions;
the unpublished exact dependency still gates registry-only Victor installation.
