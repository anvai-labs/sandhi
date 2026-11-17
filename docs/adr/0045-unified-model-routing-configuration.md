# ADR-0045: Use one typed model-routing manifest and a shared routing vocabulary

Date: 2026-11-17

Status: Proposed

## Context

Sandhi and Victor both describe model access, but their nouns have drifted:

- Sandhi's durable backend uses `provider`, `credential_ref`, `endpoint_family`,
  `endpoint_profile`, `base_url`, logical `routes`, group `profiles`, and budgets.
- Victor layers `profiles.yaml`, centrally pulled `profiles.d` files, `config.yaml`,
  `api_keys.yaml`, `routing_profiles.yaml`, environment variables, and many bundled
  capability registries. `ProfileConfig` also permits unknown fields.
- “OpenAI-compatible” is often treated as a provider even though it is a wire protocol.
  A provider such as Z.AI, Qwen/Alibaba, OpenAI, or InferFlux can expose more than one
  protocol and endpoint.
- Endpoint selection is sometimes encoded in a model identifier (for example a `:coding`
  suffix), and `base_url` can mean a direct upstream, an override, or a gateway depending
  on the surrounding block.
- Generation limits are commonly named `max_tokens` even where the intent is specifically
  an output cap.
- Before this decision, the committed Sandhi `config/sandhi.json` example used the removed
  singular budget shape, proving that duplicate hand-authored formats can silently become invalid.

A literal one-file/global configuration is not a safe boundary: public vendor facts, private
deployment topology, user workload profiles, and credentials have different owners, review
requirements, lifetimes, and trust domains. Consolidation must remove duplicate *authority*,
not force server secrets or topology into every client.

## Decision

1. Use **TOML as the single human-edited source format** for model-routing manifests.
2. Compile TOML into **canonical JSON** for machine exchange, digests, generated runtime
   documents, and schema-verified contracts.
3. Establish one manifest per trust domain:
   - an operator-owned `sandhi.toml` or shared `model-routing.toml` for server routing;
   - one user-owned Victor config file compiled from the shared vocabulary;
   - package-owned vendor catalogs remain versioned package data, not user configuration.
4. Never put secret bytes in the manifest. A credential entry names a source such as an
   environment variable, OS keyring, SentinelPass exact entry, or service-manager credential.
5. Reject unknown fields in user-authored routing sections. Extensions must be added to the
   versioned schema first.

TOML is chosen over the alternatives because it has explicit scalar types, comments, broad Rust
and Python support, and less ambiguity than YAML. JSON remains the canonical exchange form but
is too verbose and comment-hostile for hand maintenance. YAML is familiar but permits implicit
typing and easy structural drift across deep layered files. Properties/INI files cannot express
this hierarchy. CUE/HCL would add a new required toolchain before the vocabulary is stable.

## Canonical vocabulary

| Term | Meaning | Invalid uses |
|---|---|---|
| `provider` | Account/vendor authority: `openai`, `zai`, `qwen`, `anthropic`, `google`, `inferflux` | A wire shape or endpoint |
| `protocol` | Stable request/response family: `openai_chat_completions`, `openai_responses`, `anthropic_messages`, `gemini_generate_content` | A vendor or product plan |
| `dialect` | Protocol quirks and accounting profile, such as `zai_final_choice_usage` or `chatgpt_codex` | A user profile or endpoint URL |
| `credential` | Non-secret reference to an API key, OAuth lease, keyring item, or SentinelPass exact entry | Inline secret bytes |
| `account` | Billing/workload identity and credential lease selection | A transport implementation |
| `endpoint` | Provider + protocol + dialect + URL + headers + credential reference | A bare URL |
| `model` | Provider's actual model ID and neutral capability limits | An endpoint selector or route name |
| `backend` | Sandhi's executable endpoint/model binding and server-owned policy | A client profile |
| `route` | Logical model or workload to backend/target selection, affinity, and budget scope | A provider slug |
| `gateway` | A Sandhi service deployment and its HTTPS URL/auth material | A direct vendor `base_url` |
| `profile` | Named Victor workload defaults and route/model selection | An endpoint or credential definition |
| `generation.output_tokens` | Requested maximum generated output tokens | Total context or thinking budget |
| `generation.thinking.budget_tokens` | Native reasoning/thinking budget | Output-token cap |
| `transport` | Client implementation choice, such as HTTP or native FFI | Provider or protocol |

The compatibility mapping is:

- Sandhi `endpoint_family` maps to canonical `protocol`.
- Sandhi `endpoint_profile` maps to canonical `dialect`.
- Legacy `base_url` maps to `endpoint.url` for a direct upstream or `gateway.url` for Sandhi.
- Victor `max_tokens` maps to `generation.output_tokens`.
- Model suffixes such as `:coding` map to an explicit endpoint reference and are not part of
  the provider model ID.

## Manifest shape

The proposed source document is an array-oriented TOML file so ordering and repeated entries are
explicit:

```toml
schema = "anvai.model-routing.v1"

[[provider]]
id = "zai"
display_name = "Z.AI"

[[protocol]]
id = "openai_chat_completions"

[[credential]]
id = "zai_coding_api_key"
provider = "zai"
auth = "api_key"
source.env = "ZAI_API_KEY"

[[endpoint]]
id = "zai_coding"
provider = "zai"
protocol = "openai_chat_completions"
dialect = "zai_final_choice_usage"
url = "https://api.z.ai/api/coding/paas/v4"
credential = "zai_coding_api_key"

[[model]]
id = "glm-5.3"
provider = "zai"
provider_model_id = "glm-5.3"
limits.max_context_tokens = 262144
limits.max_output_tokens = 65536

[[route]]
id = "coding/glm-5.3"
model = "glm-5.3"
target = "endpoint:zai_coding"
budget_scope = "coding/glm-5.3"

[[profile]]
id = "victor/research"
route = "coding/glm-5.3"
generation.temperature = 0.2
generation.output_tokens = 16384
generation.reasoning_effort = "low"

[[gateway]]
id = "sandhi/ds3"
url = "https://dataserver3.singh.local:18444/v1"
credential.file = "/private/path/to/delegated-key.json"
tls.ca_file = "/private/path/to/ca-bundle.pem"
```

Sandhi compiles endpoints, models, routes, budgets, and policies into its durable stores.
Victor compiles profiles, gateway references, and generation defaults. Both consume the same
identifiers and emit the canonical JSON representation in diagnostics.

## Migration

1. Publish the versioned schema and canonical resolver separately from runtime adoption.
2. Add read-only importers for current Victor YAML and Sandhi JSON. Importers must reject
   ambiguous endpoint/model encoding and emit an explicit migration report.
3. Add `validate`, `preview`, `migrate`, and `render` commands in both products.
4. Keep old readers for one documented release; generated canonical JSON becomes the only
   runtime input after that.
5. Remove duplicated hand-edited files only after their sections have an authoritative manifest
   home and a tested migration.

Package-owned catalogs may remain YAML or generated data while this migration is in progress.
They are immutable product facts, not user setup files, and must not be layered into user overrides.

## Consequences

Configuration becomes reviewable and diff-friendly while runtime consumes one canonical,
schema-validated representation. Provider, protocol, endpoint, credential, route, profile, and
gateway can each change independently. A model can move between endpoints without changing its
identity, and an OpenAI-compatible server cannot masquerade as a provider.

The initial cost is a strict schema, migration tooling, and removal of permissive unknown fields.
That cost is warranted because the current ambiguity affects authorization, accounting, routing,
and operator diagnostics.
