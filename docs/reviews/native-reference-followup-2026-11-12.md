# Native reference initialization and effective-payload follow-up

This author follow-up corrects review findings in the first native-reference candidate. It is an incremental patch against the captured, uncommitted integration snapshot whose Git HEAD was `ce70fdc652eb0e2d2fb46e5faf33590108f11376`; it is not a review approval or a claim that the first candidate was safe. The snapshot manifest captures all 651 Git-visible source entries twice with identical inventories. The earlier candidate and its evidence remain preserved. No schema deployment or migration from a released ownership registry is claimed.

## Shared initialization

The reference store and its four nonqueued slots now live privately in the same `Buffered` owner published through the existing `OnceLock`. Initialization obtains the actual path from the validated original single-file durable ledger. It opens reference metadata there before publishing the complete owner. There is no separately configurable reference database, public optional registry field, main-only setter or second ledger. Public serving therefore supports its first native response, subsequent same-owner continuation and continuation after reopening the same database without a hidden initialization step.

Malformed or read-only reference storage refuses initialization before listener binding. A failed initialization publishes no partial owner and can be retried after storage repair. Read-only `build_app` still permits health/readiness probes without enabling execution. Serving recovery/finish lifecycle changes are a separate coordinated increment; this patch does not claim to implement that supervisor.

## Ownership of the actual backend request

Proof inspection uses the selected backend's effective payload. Raw delivery inspects the cleaned, model-selected, cache-patched native body. Typed delivery uses the actual factory/profile encoder with the frozen cache plan, also used by its execution adapter. It does not select a codec from a diagnostic provider label or infer authority from ingress dialect. References in a selected backend extension bucket therefore require the same proof as references supplied natively. Unselected buckets are inert, and admitted canonical history cannot be replaced by an extension's `input` member. No extra provider request is made.

Existing cross-family fidelity refusal remains HTTP 400 with its bounded reason. Ownership failure is HTTP 403. A reference discovered in selected extras also needs the existing established compatible logical session; an owned reference cannot create a new session pin. Fixed backend callers still need matching authenticated ownership proof.

The existing operator generation default is set before candidate codec preflight, then clamped to the selected model's declared maximum. Explicit caller limits remain unchanged. The finalized payload plane is inspected before confirming a pin or taking execution leases. Count-only local estimation skips generation defaults, cache compilation, generation codec preflight, pin confirmation, execution admission and proof issuance. It does not call a provider or dereference provider resources.

## Renewal and bounded expiry cleanup

Only the existing account-fenced Codex credential contract excludes credential generation and old access-token expiry from reference binding/TTL. Backend ID/revision, credential reference, actual account/origin/all-static-header fingerprint, family/profile, model, usage convention and authenticated owner remain fenced. Proof remains bounded by 24 hours and key expiry. Every new generation request checks its current credential lease before proof lookup/admission; transport keeps its existing dispatch-time guard. An account-only Codex handle without any lease is rejected before HTTP admission, including an unbound public-library handle. Its routing digest remains unchanged; direct FFI behavior remains separate. Ordinary credentials retain generation and issuing-lease expiry. A generic account header or expiry wrapper does not create the Codex fence.

The renewal regression models source dispatch admitted 31 seconds earlier, then waits for a real three-second old-token expiry. It verifies same-account new-token/new-generation authorization afterwards, expired-current-lease refusal, and account/endpoint/static-header/backend/revision/credential-reference/profile/model fences. This is synthetic source issuance and clock derivation, not a live subscription renewal claim.

Issuance cleanup still deletes at most 256 expired rows atomically. Up to 128 deletions prioritize the current owner's quota; up to 128 indexed deletions remove expired resource IDs actually reissued by this source, with the remainder used for oldest global expiry. Authorization performs no cleanup, adoption or TTL extension. Fresh source issuance can replace an expired foreign resource-ID proof behind a larger backlog; a live conflicting owner still refuses and rolls back the whole transaction. Independent self-contained opaque issuance retains its separate owner-proof policy.

## Validation and limits

The [artifact](artifacts/native-reference-followup-2026-11-12.json) records exact focused results and the public source context. Store tests include the independently reproduced 1,000-expired-row counterexample, atomic rollback and full-owner-quota progress. HTTP tests cover both delivery modes, public serving/restart, read-only/malformed storage, cross-dialect selected references, inactive buckets, canonical-input preservation, explicit/default output bounds, count-only, established-session fencing and fidelity status preservation. Source accounting and zero-send/lease assertions remain in the earlier ownership matrices.

An initial provider-unit attempt lacked sandbox permission to bind localhost fixtures; its socket failures are retained separately from the network-enabled result. A feature-check invocation used an invalid feature spelling and is likewise retained as an invocation error. Neither is classified as a product regression or a passing check.

No full workspace, long SDK rerun, live vendor, Windows runtime or original shutdown/admission failure resolution is claimed for this follow-up. Root integration still needs independent review and its coordinated regression gates. Existing spontaneous accounting waits, native Python exit occurrence, original shutdown timing occurrence and broader unsupported external-resource/adoption capabilities remain open. This is not merge, release, deploy or full-goal approval.
