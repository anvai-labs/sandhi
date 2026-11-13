# Emergency exit and binary provenance checkpoint

The binary's emergency exit 124 now requests termination through
[Unix `_exit`](https://man7.org/linux/man-pages/man2/_exit.2.html), or Windows
[self-`TerminateProcess`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess).
These calls bypass ordinary C
exit callbacks and Rust exit serialization. They are confined to the existing
emergency paths after incomplete cleanup, deadline expiry or watchdog failure;
embedded libraries never call them. Successful explicit cleanup retains ordinary
process exit, so its later C callbacks are outside this emergency guarantee.

This addresses a demonstrated contract flaw, not a diagnosis of the earlier
[SDK shutdown timeout](sdk-shutdown-timeout-2026-11-12.md). The
[Rust exit contract](https://doc.rust-lang.org/std/process/fn.exit.html) permits
ordinary exit to run callbacks and serialize concurrent exits. Two isolated
regressions register a permanently blocked callback, either reached by the
watchdog itself or by a concurrent ordinary exit. Both fail against the old
emergency implementation at the existing bounded parent deadline. The corrected
implementation passes those and the existing cleanup/runtime watchdog cases.

## Integrated validation

| Check | Result |
| --- | --- |
| Emergency watchdog selection | Four tests passed, covering five child scenarios |
| Source/artifact provenance regressions | Nine passed |
| Actual synthetic HTTP/TLS shutdown suite | Sixteen passed |
| Combined SDK/provenance run | 25 passed in 44.31 seconds |
| Rust workspace | 1,230 passed; six opt-in ignores across 52 suites |
| Default and ONNX/SentinelPass all-target Clippy | Passed |
| Binary all-feature Clippy | Passed |

The process suite includes eight tracked buffered/streaming HTTP/TLS cells with
settled or deliberately unavailable usage, alongside existing admission, drain,
connection-capacity and deadline cases. Its six-second tracked process wait and
physical-request assertions were not widened or removed. Unknown usage retains
unsettled liability and cannot become measured zero or authorize a receipt.
These are synthetic provider tests, not live subscription acceptance.
The eight tracked cases also have exported exact request/execution/attempt joins:
four qualified executions produced four receipts settling eight reservations;
four unavailable executions retained both reservations without a receipt. The
zero actual placeholders in the latter group are not measured zero or released
liability.

The [sanitized evidence](artifacts/emergency-exit-and-binary-provenance-2026-11-12.json)
pins source parent, build-time source/diff inventories, compiler/configuration
metadata and the frozen executable hash. All sixteen shutdown launch manifests
verify that same executable immediately before launch. The stable temporary
evidence directory retains stderr, databases and launch metadata outside pytest's
rotating default temporary root.

## Provenance boundary

The session fixture selects the executable from Cargo's actual
[compiler-artifact message](https://doc.rust-lang.org/cargo/reference/external-tools.html#artifact-messages),
rather than guessing a default target directory or falling back to a stale release
binary. It fingerprints tracked contents, staged changes and nonignored untracked
files, and rejects HEAD, inventory or selected build-configuration changes across
the build. Only safe selected flags and configuration content hashes are recorded;
arbitrary environment and configuration values are omitted.

This records observed source and launched bytes. It is not a hermetic compiler
input read-set, proof that every inventoried byte was compiled, protection against
all timestamp/cache reuse, or proof of reproducibility. Ignored inputs, external
dependencies/generated inputs and directory-symlink contents remain outside the
inventory. The execution shown here is Linux; native macOS/Windows behavior is
not established by it. Windows deliberately aborts if OS self-termination
unexpectedly returns rather than re-entering ordinary exit.
The controlled tests establish callback bypass, not an absolute bound on kernel
I/O completion or OS scheduling.

The original accounting waits, Python signal exit and SDK timing occurrence remain
open reliability/merge gates. No storage durability, settlement rule, inferred
charge, accounting owner, provider retry or inference replay changed.

## Public CI follow-up: nested dependency checkout

The first public run of `3e3a3a7` passed twelve jobs, but its SDK job recorded
366 passes, one failure, 208 fixture errors and one skip. The failure and errors
share the same inventory assertion: CI creates `.inferflux-pinned/` as a nested
Git checkout, which Git lists as a directory rather than traversing as outer
untracked files. The fixture stopped before launching the affected gateway cases.
This is failed qualification evidence, not a new process-shutdown diagnosis.

The follow-up represents a nested repository by its HEAD and tracked-diff hash.
Its files and untracked/generated contents remain explicitly outside the outer
inventory. Embedded-checkout and tracked-gitlink regressions fail against the old
helper and pass with this handling. The complete public SDK run still needs to
validate that correction; the original failing run is retained in the
[follow-up artifact](artifacts/nested-checkout-provenance-2026-11-12.json).
