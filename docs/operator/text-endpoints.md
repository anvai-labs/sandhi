# Governed text endpoints

`POST /v1/embeddings` and `POST /v1/completions` use the same virtual-key/OIDC identity,
model allowlist, attribution checks, rate limiter, durable policy receipts, budget
ledger, transport deadlines, usage settlement and alert registry as chat requests.
They require an OpenAI-compatible raw upstream, including InferFlux. Cross-family
translation is rejected before dispatch.

## Supported requests

Embeddings accept `input` as text or a nonempty batch of at most 2,048 text items,
with optional `encoding_format` (`float` or `base64`), `dimensions` (1–65,536), and
`user`. Empty embedding strings, token-ID arrays and unknown fields are rejected.
Embedding vectors pass through unchanged; the gateway does not deserialize vectors
into its chat response contract or charge them as output tokens.

Completions accept `prompt` as text or a nonempty text batch of at most 2,048 items.
They support buffered and SSE responses, `suffix`, `max_tokens`, `n`, `best_of`,
`temperature`, `top_p`, `logprobs`, `echo`, `stop`, penalties, `logit_bias`, `user`,
`seed`, and `stream_options.include_usage`. Controls are type/range checked. `n`
and `best_of` are bounded to 1–128, with `best_of >= n`; max_tokens is 0–1,048,576.
The upstream may impose tighter model-specific limits or reject a control combination.
Token-ID prompts, ambiguous duplicate keys and unknown extensions are rejected.

Requests with an explicit completion limit and all embedding requests preserve their
wire bytes. Missing `max_tokens` is explicitly set to 16 and the body is reserialized;
this avoids switching to a chat encoder or relying on an unbounded provider default.
Responses preserve the native endpoint shape and the existing response-header allowlist.

## Inspection and accounting

The policy engine receives the original endpoint shape with an explicit trusted
format. It checks every text item plus string-bearing options such as suffix, stop,
and user. Format selection comes from the route, never caller-supplied metadata.
Unknown or opaque input cannot silently skip inspection. Receipts contain decisions
and rule IDs, not prompt text or embedding vectors. Existing policies remain opt-in;
input validation is always enabled on these routes.

Embedding reservation exposure has zero output tokens. Completion output exposure
is `max_tokens × prompt_count × max(n, best_of)`; input bytes include the wire body plus repeated suffix context for every additional
prompt, then multiply by the number of generated alternatives before token estimation.
The body already includes every prompt; a shared suffix does not. Repeated context is
counted arithmetically without allocating duplicate strings.
Measured upstream usage settles the lease. Input token estimates remain heuristic,
not a strict proof of an upper bound; these routes do not turn the existing budget
ledger into exact provider tokenization. Missing/partial usage keeps the existing
metering semantics and must not be represented as an exact measurement.

The canonical chat request is only an internal admission/accounting view. These routes
never dispatch it through a chat encoder. Model capability still belongs to the
upstream: use an embedding model for embeddings and a generation model for completions.
Sandhi has no native `/v1/classify`; Victor can classify through governed chat calls.

The policy `max_output_tokens` threshold inspects the explicit per-completion field,
not aggregate batch liability. The budget calculation above is separate. Embeddings
have no explicit output limit, so select such threshold rules only for generation
models; applying an explicit-limit rule to embeddings fails closed. Body-byte policy
thresholds inspect forwarded wire size, not expanded batch context.

## Rollout gate

Run `cargo test -p sandhi-proxy --test text_endpoints`. Validate actual client text
shapes, both embedding encodings, completion streaming, policy denials and budget
settlement against the candidate before removing direct InferFlux access. Tokenized
clients require a separate tokenizer-aware inspection contract; do not disable policy
to hide incompatibility. TLS, WSL firewall/port mapping and client migration are separate
rollout steps, not implied by these routes being implemented.
