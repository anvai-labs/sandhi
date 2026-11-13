# Archived InferFlux, Sandhi and Victor integration plan

This historical plan motivated the catalog-owned session-affinity and usage
contracts. It is not a current backlog or deployment runbook. Use the
[operator guide](../operator/proxy-guide.adoc), [backend configuration](../operator/backends.md)
and [protocol matrix](../product/protocol-and-credential-matrix.md) for supported
setup, and [ADR-0008](../adr/0008-inferflux-admission-and-session-affinity.md)
for the original admission decision.

## Component boundaries

| Component | Responsibility |
| --- | --- |
| InferFlux | Authenticate its callers, execute the selected model and report original usage/cache/timing fields |
| Sandhi | Authorize the model/backend, preserve compatible request/session metadata, enforce admission and record neutral usage |
| Victor | Supply the selected gateway route and per-call correlation; execute its own agent/tool workflow |
| Downstream control plane | Consume usage events and apply its own pricing or business policy |

Source integration and artifact publication are distinct. A consumer must qualify
its exact published binding and chosen gateway version; a source checkout or
successful model request does not establish complete compatibility.

## Acceptance carried forward

- Use the same ready backend/model when comparing direct and gateway requests.
- Check both native and translated paths, including buffered and streaming usage.
- Match request/session/run metadata according to the documented contract; do not
  infer a distributed trace from a request ID alone.
- Preserve inclusive cache arithmetic and original timing provenance.
- Qualify actual credentials, model readiness, expiry and denial behavior for each
  deployment, without treating gateway failure as permission to call another route.

The original session derivation, request-correlation and trace-header changes
landed after this plan. Their maintained contracts are in
[per-call transport context](../td/TD-0022-per-call-transport-context.md),
[origin co-design](../td/TD-0027-three-way-origin-codesign.md) and
[upstream usage fields](inferflux-issue-drafts.md).
