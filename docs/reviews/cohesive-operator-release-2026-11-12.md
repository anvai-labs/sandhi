# Cohesive operator console and documentation checkpoint

The prepared 0.17.0 console and MkDocs now explain the same request path: client
dialect, authorized gateway decisions, backend binding/cache compilation and source
evidence. The console adds a Routing & cache guide and Overview task shortcuts;
README, homepage, cache/token guide, operator guide and changelog use matching
terms. Updated synthetic screenshots show the current interface without an admin
credential. Navigation remains the existing local-asset workspace application.

Observed units and Run usage tree replace monetary-sounding display labels.
Existing API fields and arithmetic are unchanged: raw event/category totals and
reasoning overlap remain separate from frozen weighted charges. Recorded overlap
takes precedence; historical rows retain the existing fallback when that flag is
absent. Read share is an input-composition measure, not currency saved. The guide
also distinguishes original source evidence from compatible client wire counts,
including an explicitly illustrative Gemini cache-write conversion.

## Validation

The integrated actual-gateway/API/SQLite/browser suite passed **49 cases** in
97.72 seconds. It covers existing auth clearing, escaping, inventory and evidence
flows plus desktop/mobile guide, keyboard focus, history and neutral-label cases.
After independent review corrected historical-overlap wording, the three affected
browser cases passed again. JavaScript syntax, strict MkDocs and diff checks pass.

AgentBrowser verified the restored synthetic preview and refreshed the committed
Overview/guide images from its final served assets. Desktop and mobile layouts
were visually inspected; the 390-pixel page had no document overflow and the admin
field was empty. The independent preliminary reviewer additionally checked
320-pixel dark/reduced-motion, keyboard, Back/reload and protected-data behavior.
The [sanitized artifact](artifacts/cohesive-operator-release-2026-11-12.json) pins
changed files, logs, served assets, executable and screenshot hashes, distinguishing
the earlier full run from the final wording checks.

An earlier author test run had 48 passes and one new-fixture failure: its run lookup
selected a nonexistent run. The corrected fixture seeds a real synthetic request;
focused and full follow-ups passed. That retained failure is not a backend product
failure or live-provider acceptance.

## Release boundary

These changes unify the release presentation; they do not publish Sandhi 0.17.0 or
Victor 0.29.0, upgrade a service, or clear gateway peer/default, provider-budget
identity, broader cache/state/subscription, packaging or retained reliability gates.
The guide explicitly distinguishes observations, delivery and settlement, and
does not imply every protocol has identical egress inspection or native-state
support. This is synthetic UI evidence, not a monetary saving or live cache claim.
