# Native proof work: bounded contention and lifecycle ownership

Base: `641ff491019d63351baf61862face2a51ea8481e`. This correction handles native
reference metadata as owned request work while preserving the same original source
attempt, budget charge, receipt and fail-closed reference binding.

## Retained failure and diagnosis

The base's public CI passed all native-platform jobs and all 735 SDK tests (three
skips), but SentinelPass-feature Rust testing failed while reading the Codex native
continuation stream: HTTP 200 preceded a generic reference-proof body error.
The dependent aggregate also failed. No merge or release was performed.

A controlled real SQLite writer reproduces that surfaced failure: a valid existing
reference remains readable through WAL, while issuance fails after 100–101 ms.
Releasing the writer lets the identical frame succeed; failed new issuance grants
nothing. This establishes a failure mode, not the exact cause of the CI occurrence.

A separate baseline regression confirmed that canceling a metadata waiter left
lifecycle idle with active count zero while its blocking worker still owned work.
The test failed before the correction; the corrected test passes.

## Resulting contract

One proof-work deadline is frozen from the selected request source policy plus
its existing admission/observation/settlement headroom, starting with preparation.
The deadline is never restarted per frame. An immutable shutdown deadline can
shorten both the async waiter and actual worker after they start.

An inert, privately issued completion authority grants no model dispatch and does
not count an unpolled response as active work. Actual queued/running metadata jobs
hold lifecycle guards until the worker finishes, including when its waiter is
canceled or times out. Serving seals completion admission after connection owners
finish, before final idle and inventory checks. A retained worker yields honest
incomplete shutdown instead of false clean completion.

Metadata serialization uses deadline-aware locking. Only SQLite Busy/Locked errors
are retried under the original budget; transactions roll back before retry.
Ownership, expiry, capacity and permanent failures remain terminal. Native bytes
are withheld until their proof transaction commits. The original source is drained
and accounted without another inference. Public store error compatibility is kept;
diagnostics contain bounded static reasons, without SQL, reference values or secrets.

## Qualification

[Artifact pins and report/log hashes](artifacts/native-proof-work-2026-11-13.json)
retain the base failure and corrected evidence. The complete corrected workspace
passes **1,300 tests**, with zero failures and 13 separately qualified/explicitly
ignored cases. The exact failing feature lane passes **717 proxy/store tests**.
All-target workspace Clippy passes with warnings denied.

The reference filter passes 23 cases, including ten new regressions; the store
filter passes seven. These cover real transient writer contention, no grant on
deadline failure, the same budget across frames, dynamic shutdown shortening,
canceled and queued workers, inert bodies, sealed late admission, and public
serving refusing clean completion while actual proof work remains blocked.

A new immutable gateway snapshot (`b43a641becc32535fcece302c94a0a8016bc68a1c5a9e14b950f3632810561fd`)
passes all **48 live GLM calls** across the 24 client/backend/delivery combinations.
Chat/Responses use default thinking; Messages explicitly disables thinking for
portable output. All SDK usage and original paired evidence match. Positive source
cache-read observations appear on all three endpoints. These remain neutral unit
observations, not guaranteed warm-cache behavior or dollar savings.

This local result does not make the failed base CI green. The corrected exact
commit needs its own public CI and protected promotion before the authorized
0.17.0 tag and all-target publication. Earlier historical native-exit/timing
causes remain explicitly unproven.
