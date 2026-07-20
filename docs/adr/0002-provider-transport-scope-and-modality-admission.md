# ADR-0002: `sandhi-providers` scope — chat-completion transport, and the discipline for admitting a new modality

Date: 2026-07-20

## Status

Accepted. Refines **ADR-0001** (which established `sandhi-providers` as the unified provider
transport). Ratifies the boundary decision that came out of a first-principles design review of
the deferred **embedding modality** (PR #13, closed). Sibling context: ProximaDB **ADR-067** /
**TD-SANDHI-1**, AnvaiOps **ADR-0047 D1** and **ADR-0020 D1** (the "don't build shared infra
ahead of ≥2 consumers" doctrine).

## Context

ADR-0001 gives `sandhi-providers` a single job: the provider **wire layer** that victor,
ProximaDB, and AnvaiOps delegate to, so usage/cache-token parsing is single-sourced at the point
of the call where metering trust is decided. Concretely today that is the **chat-completion**
`Provider` trait (`complete` / `stream`), wrapped by the metering + circuit-breaker + retry
**decorator** stack (`ResilientProvider`, and the metering decorator to come). That decorator
stack — not the raw HTTP call — is sandhi's reason to exist.

In 2026-07 an **embedding modality** was prototyped (PR #13): an `EmbeddingProvider` trait plus
OpenAI `/embeddings` and Cohere `/v2/embed` adapters, so ProximaDB could delegate its embedding
egress to sandhi in-process. An adversarial co-design review declined it before merge, and this
ADR records **why**, so the modality is not re-added prematurely.

The review's findings (verified against the code, not the PR narrative):

1. **One consumer.** Only ProximaDB would have used the embedding transport; victor does not, and
   sandhi's own proxy does not route embeddings. That is a shared OSS abstraction with a single
   consumer — squarely against the ≥2-consumer doctrine both parent repos hold.
2. **Outside the decorator stack.** `EmbeddingProvider` sat *outside* the resilience/metering
   decorators, so the embed path got **no retry, no circuit breaker, no timeout** — strictly less
   than a direct blocking client would (the tell: the consumer's error mapping handled a
   `CircuitOpen` variant that could never occur on that path). Adopting "the gateway" bought
   *fewer* operational guarantees, not more.
3. **Complexity for nothing.** It required a git dependency into a production engine + an
   async→sync bridge, to replace ~40 lines of blocking HTTP that already had a working in-repo
   template (ProximaDB's `azure_openai.rs`). ADR-067 had itself marked this consolidation
   "optional… a follow-up, not required."

ProximaDB instead meters OpenAI/Cohere embeddings from real usage via its own direct clients; its
KEU meter stays authoritative (ADR-0020 D6). Nothing crosses the repo boundary.

## Decision

### 1. `sandhi-providers`' scope is chat-completion transport + its decorator stack

The crate owns the `Provider` (`complete`/`stream`) modality and the metering/resilience
decorators wrapping it. A modality that does not enter that decorator stack is "just an HTTP
client" and does not belong here — a consumer can write that itself in ~40 lines.

### 2. Admitting a new modality requires **all three** of:

- **≥2 real consumers.** One consumer implements the transport locally instead (ADR-0047 D1 /
  ADR-0020 D1). The count is *actual adopters*, not anticipated ones.
- **It enters the decorator stack** — metering (neutral-event assembly) + circuit-breaker + retry
  + timeout apply uniformly, the same way they do for chat. Raw transport is out of scope.
- **Its own ADR** ratifies it, naming the ≥2 consumers and whether it is in-process-only or also
  proxy-routed.

### 3. Embeddings, specifically, are **deferred**

Prototyped in PR #13 (closed); preserved on branch `feat/embedding-modality`. Reopen when §2
holds — most likely when the ADR-0047 D10 chat `LLMClient` consolidation lands a second Rust
consumer *and* embeddings are brought under the decorator stack.

## Consequences

- **Positive:** sandhi stays a focused *usage gateway*, not a generic provider SDK; consumers do
  not take a premature dependency or a resilience regression; the boundary is explicit for
  contributors, so the next well-meaning "just add embeddings" PR has a clear bar to clear.
- **Negative:** a real second consumer of embedding transport must re-do the (preserved) #13 work
  and shepherd its ADR — deliberately, that friction is the ≥2-consumer gate doing its job.
- **Neutral:** no code change; this ADR ratifies the status quo (chat-only `sandhi-providers`).
