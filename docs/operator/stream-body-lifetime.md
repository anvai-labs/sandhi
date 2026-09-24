# Streaming body lifetime ownership

Rust embedders can opt into `ProxyState.stream_body_lifetime` using
`StreamBodyLifetime::new(Duration)`. This intermediate library surface has no
standalone environment, CLI, admin, or `SANDHI_CONFIG` setting. The default is
`None`: streams remain pull-driven with the same wire bytes and finalization
ordering. No deployed gateway limit changes through this increment.

The opt-in owner starts when the gateway receives successful upstream headers.
It drives the existing transparent or translated usage parser independently of
client reads. One absolute body deadline covers source polling and bounded queue
sends. Dropping the receiver or reaching shutdown's existing **grace deadline**
also drops the source. Merely entering quiescence does not abort admitted streams.
Stream setup and inter-chunk idle remain the transport's separate existing limits.
Custom host-owned transports are rejected before dispatch under this policy.

The delivery queue holds at most four copied 16-KiB fragments (64 KiB). A pending
send can hold another 16 KiB; the current source frame, parser, HTTP transport and
OS buffers are additional. Splitting a frame copies its fragments so a queued
fragment cannot retain an arbitrarily large original allocation. This is not a
bound on synchronous parsing time or total per-request memory. On failure, the
receiver discards queued bytes and receives a body I/O error, independently of
settlement progress. On success, it drains queued bytes before EOF. Already
sent bytes or protocol terminal frames cannot be recalled; this is not a bound
on when the remote client consumes bytes buffered outside the controller.

The duration must be positive and no greater than the 900-second reservation TTL
minus 60 seconds of settlement headroom. The common actual-lease check runs before
dispatch and again after upstream setup, with the same persisted-second expiry
precision used by buffered policy. A pre-dispatch refusal returns 503 and releases
the reservation without a usage event. A post-setup refusal drops the upstream
and reports a streaming error with unavailable usage; the upstream was contacted
and may have incurred spend. No token measurement is invented. Neither check
renews a lease or guarantees settlement before expiry.

The owner drops the upstream before moving accounting into a blocking finalizer.
It retains the admission slot and lifecycle operation until that finalizer ends;
`sandhi_shutdown_active_operations` therefore still
reports blocked cleanup. Stream success and protocol DONE are **not durable
settlement receipts**. The opt-in controller may complete transport before the
finalizer finishes; the unchanged default still finalizes before the translated
OpenAI DONE frame. Finalizer task failures are logged explicitly. Observed final
usage stays final on a delivery timeout/disconnect, with the transport outcome
recorded separately from measurement completeness.

Remaining work includes configurable setup/idle/body policy resolution, lease
renewal or bounded settlement with atomic evidence, and live origin-cancellation
acceptance. A Tokio blocking task cannot be cancelled; ledger contention can
outlast headroom. The proxy still uses its existing settlement path, not the
store's separately implemented `settle_with_evidence_durable` primitive.
A closed gateway transport does not establish that origin GPU work stopped.

## Regression ownership

The existing terminal-accounting suite retains wire/SQLite/ledger conservation
and adds a minimal opt-in sample plus an unpolled real-TCP body case on both
planes. The controller unit suite owns full-queue expiry/disconnect, byte order,
producer panic, post-setup lease refusal, and shutdown grace while settlement is
blocked. Existing lease-reclamation, provider setup/idle and raw partial-cache
coverage remain separate owners. The audit found no redundant suite to remove.
These fixtures are local regressions, not Victor member or live model evidence.
