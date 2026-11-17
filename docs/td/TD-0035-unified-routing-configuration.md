# TD-0035: Consolidate Sandhi/Victor routing configuration and nomenclature

- **Status:** Proposed, 2026-11-17.
- **Relates to:** [ADR-0045](../adr/0045-unified-model-routing-configuration.md),
  [TD-0034](TD-0034-protocol-neutral-router-cache-accounting.md).

## Why this exists

The current split creates avoidable operator burden:

1. Victor has multiple user-facing configuration layers and permits unknown profile fields.
2. Model strings and provider names sometimes encode endpoint behavior.
3. Sandhi and Victor use different names for protocol, endpoint, profile, gateway, and output
   limits.
4. Hand-authored examples can drift from executable schema changes; before this change, the
   committed Sandhi sample contained the removed singular budget shape.
5. Generated Sandhi profile documents and hand-written Victor provider blocks can describe the
   same route with unrelated identifiers.

## Work packages

| ID | Deliverable | Acceptance |
|---|---|---|
| W1 | Versioned `anvai.model-routing.v1` JSON Schema | Rejects every ambiguous field mapping in ADR-0045; canonical example passes; mutated examples fail with actionable paths |
| W2 | TOML reader and canonical JSON renderer | Stable formatting, deterministic digest, UTF-8/bounded-size checks, duplicate-ID rejection, no environment resolution during parse |
| W3 | Sandhi importer/compiler | Maps protocol/dialect to existing endpoint family/profile, endpoint credentials, logical routes, model limits, and budget scopes without semantics loss |
| W4 | Victor importer/compiler | Replaces endpoint-carrying profiles with route/profile references; preserves generation defaults and gateway auth boundaries |
| W5 | Migration reports | One report per existing YAML/JSON source; every warning names old path, canonical path, and required owner decision |
| W6 | Catalog boundary | Bundled vendor/model capability catalogs become immutable package facts exposed through identifiers, not layered user overrides |
| W7 | Documentation and CLI | `config validate`, `config migrate`, `config preview`, and `config render` behave identically across products |

## Non-goals

- Putting bearer bytes, OAuth refresh tokens, private keys, or delegated-key values in TOML/JSON.
- Making one global file span server trust boundaries.
- Hand-editing generated runtime JSON.
- Using provider aliases as authorization identity.
- Treating a protocol or endpoint as a model provider.

## Acceptance

1. One canonical identifier set appears in Sandhi receipts, admin inventory, Victor profile
   receipts, and diagnostics.
2. A provider can expose multiple protocols and endpoints without changing model identity.
3. A model can move to another endpoint through route/backend revision only.
4. `generation.output_tokens` is the sole user-facing output cap; protocol adapters translate it
   to native `max_tokens` fields at the wire boundary.
5. Unknown user-routing fields fail validation.
6. Existing Victor and Sandhi configurations migrate with explicit reports or are refused with
   actionable ambiguity errors.
7. The committed Sandhi example validates against the current runtime schema.
8. Cross-repository fixture tests prove the same canonical manifest compiles to both Sandhi and
   Victor runtime contracts.

## Current audit findings

### Sandhi

- Desired server state is JSON under `SANDHI_CONFIG`.
- OIDC is private JSON under `SANDHI_OIDC_CONFIG`.
- Runtime selection uses many environment variables.
- Backends already separate provider, credential, protocol (`endpoint_family`), dialect
  (`endpoint_profile`), URL, and models, but names are not yet canonical.
- Group profiles use `max_tokens` where the semantic is output tokens.
- Before this change, `config/sandhi.json` contained removed singular budget syntax. The
  committed sample now uses the canonical hierarchy and is covered by a parser test.

### Victor

- User-facing files include `profiles.yaml`, `profiles.d/*.yaml`, `config.yaml`,
  `api_keys.yaml`, `settings.yaml`, and `routing_profiles.yaml`.
- Bundled package data includes more than fifteen YAML registries.
- `ProfileConfig` permits arbitrary extra fields.
- Provider aliases overlap (`zai`/`zhipu`/`zhipuai`, `qwen`/`alibaba`/`dashscope`,
  `groqcloud`/`groq`, `xai`/`grok`).
- Endpoint selection can be hidden in a model suffix.
- `base_url` participates in direct-versus-gateway suppression rules.
- Generation output is named `max_tokens`.

## Sequence

1. Fix the stale Sandhi example and test it against the current parser.
2. Land ADR/TD and the canonical example without changing runtime behavior.
3. Introduce schema plus renderer behind CLI commands.
4. Add importers and adversarial fixtures.
5. Compile Sandhi first because it owns authoritative routing and accounting.
6. Compile Victor profiles against those identifiers.
7. Deprecate hand-written duplicates only after both compilers and migration reports are qualified.
