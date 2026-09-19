# ADR-0011: Bounded, admin-only persisted usage diagnostics

Date: 2026-09-18

## Status and scope

Accepted design for TD-0028 C4 following independent security/bounds review; implementation
and CI remain separate gates. This is a read-only projection of existing SQLite usage rows,
not a new capture, retention, trace collection or release mechanism.

## Decision

Add `POST /admin/usage/diagnostics` with exactly one tagged selector:

```json
{"selector":{"kind":"request","value":"example-id"},"limit":100}
```

Kinds are `request`, `session`, or `run`. Authenticate with the existing admin gate before
decoding or querying; virtual keys and correlation IDs are not authorization. The optional
public-dashboard setting must never open this endpoint. Every response is `no-store`.
POST keeps identifiers out of URL query strings; this endpoint performs no mutation.

Hard limits: 4 KiB request body; selector 1–256 UTF-8 bytes, rejecting controls and
whitespace-only values; default 100, maximum 500 rows; final serialized JSON at most
256 KiB including envelope, escaping and warnings. Reject unknown/duplicate fields and
invalid limits. No wildcards, unfiltered scan, count-all query, pagination or remote upload.

Use three fixed indexed SQL selections, bound parameters, newest insertion first
(`ORDER BY rowid DESC LIMIT limit+1`). Preserve duplicate matches. Session/run indexes
exist; add an idempotent request-ID index. Bound persisted text before materializing it,
reject invalid SQLite types and omit invalid/oversized fields with fixed field-name warnings.
Stop at the byte budget; explicitly distinguish row and byte truncation. Index creation
can cost time on old large databases; it is not a historical data backfill.

Permit one diagnostic query per gateway at a time, fail-fast with no waiting queue.
Run SQLite work on `spawn_blocking`, moving the permit into that task so cancellation
of the HTTP future cannot admit another query while the first still runs. Shutdown
admission also applies: reject work after cutoff and retain its lifecycle
operation guard inside the blocking task until SQLite exits, including after disconnect.
Bound body reads to five seconds within admission. Existing
SQLite connection locking and busy timeout remain; do not promise a hard query deadline.
Return generic errors; do not echo SQL, selector values, database paths or response bodies
in error messages or logs.

## Evidence and privacy boundaries

Return an explicit bounded projection, never an entire UsageEvent: persisted
request/session/run/step/parent IDs, timestamp, provider/model, normalized counters,
reasoning inclusion, validated cache-read observation, duration and TTFT with their
persisted sources. Exclude credential fields, virtual-key IDs, subject/group attribution,
trace context, arbitrary metadata, URLs, headers, prompts, response bodies and raw usage.
Identifiers and model strings are caller/origin-controlled and can themselves be sensitive;
this is not guaranteed anonymization or authorization for sharing the export externally.

The persisted request_id is upstream-preferred, otherwise admission/late-minted.
Its provenance and a separate upstream/admission ID are not persisted, and it is not
unique. Do not relabel it as necessarily Sandhi-minted or deduplicate matching records.

Explicitly list unavailable historical evidence: raw origin usage, completeness, basis,
outcome, physical attempts, request-ID provenance and separate upstream/admission IDs.
Never substitute Final, success, one attempt or reconstructed raw prompt counts.
This is a best-effort logical-call snapshot: active streams may have no row, late usage
and aborts retain current finalization behavior, and missing rows do not prove no traffic.
Cache reporting metadata does not prove backend reuse. No prompt/body capture is added.

Expose a thin `sandhi diagnose` command using the same selector and row constraints.
Bound response reads before parsing, keep nonzero failure exits, suppress raw error
bodies, and do not create files or upload exports automatically.

## Verification gates

Test admin authorization before body/DB work, public-dashboard isolation, no-store,
missing-store/generic-error behavior, unknown/duplicate fields and all size boundaries.
Test selectors and SQL-injection strings literally, duplicate IDs, indexed query plans,
insertion ordering, exact serialized byte bounds, row/byte truncation, legacy and malformed
SQLite values, negative counts and source/status pairs. Test credential-field canaries,
cancelled HTTP futures retaining admission until blocking work completes, and bounded CLI
reads and errors. Actual-member replay remains TD-0028 C5, not a diagnostic endpoint test.
