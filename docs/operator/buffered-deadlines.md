# Buffered upstream deadlines

The optional `buffered_deadlines` section of `SANDHI_CONFIG` sets buffered
inference limits. Without it, behavior is unchanged: buffered calls have a
120-second transport limit, stream setup 30 seconds, and stream idle gaps
90 seconds. Streaming configuration is outside this increment; unknown fields
inside the new policy are rejected, including streaming knobs.

```json
{
  "buffered_deadlines": {
    "ceiling_ms": 600000,
    "default_ms": 120000,
    "endpoints": {
      "inferflux:victor-rocm-validation": {
        "default_ms": 180000,
        "models": {"qwen3-coder-30b": 240000}
      }
    }
  }
}
```

This is an example, not an accepted InferFlux workload configuration. Record a
changed-timeout run separately from the retained 120-second acceptance baseline.

For each authorized request, precedence is the exact model within its selected
credential reference, then that endpoint's `default_ms`, then the global
`default_ms` (or the built-in 120000 ms if omitted). Endpoint keys are credential
references, **not URLs or provider-wide aliases**. Model IDs are case-sensitive;
wildcards and duplicate keys are rejected. Every value must be a positive integer
in milliseconds and must fit the required `ceiling_ms`. The inherited built-in
default must also fit. Invalid values fail configuration; nothing is clamped.

Endpoint references must already be registered at startup, from environment
configuration or the persisted vault. Provision/apply new provider credentials
before restarting with a policy referring to them. The policy does not create a
second provider registry or rebuild pooled connections for each model.

## Activation and inspection

1. Back up the existing private configuration and retain the current process
   identity and rollback command. Edit `SANDHI_CONFIG`, keeping secrets out of it.
2. As an administrator, use `GET /admin/config`. Its
   `buffered_deadlines` object shows `active`, `desired`, and `restart required`.
   Each reported effective default/override includes milliseconds and its source:
   `built_in`, `global`, `endpoint`, or `model`.
3. Restart the proxy with the same state/vault/OIDC configuration. Validate the
   process identity, readiness and protected config preview. `POST
   /admin/config/apply` does **not** hot-reload deadlines; it reports that a restart
   is required. Removing the section likewise requires a restart.

The section is absent from default admin responses when neither an active nor a
desired policy exists. Inspection retains the existing admin/OIDC role checks;
this feature does not grant dashboard users additional privileges. Request
headers and client timeouts cannot raise an operator deadline.

## What is bounded

The gateway resolves policy after identity, model and attribution authorization.
After budget admission, one absolute monotonic deadline is carried through the
existing transparent or translated transport. It covers buffered request and
response-body transfer, including error bodies; nested transport layers do not
restart it. Earlier body/authentication/admission work and later accounting and
response serialization are not an end-to-end service deadline. The independent
10-second connection limit remains. Custom host-owned providers that cannot
consume this contract are rejected for configured buffered calls.

Built-in standalone inference POST retries remain off. The transport scope also
prevents an explicitly retrying library caller from resetting the absolute limit
or waiting through backoff beyond it. Downstream application retries are separate
and may still issue additional calls. A gateway 504 or disconnect does not prove
that origin GPU work was cancelled.

The policy ceiling cannot exceed the 900-second reservation TTL minus 60 seconds
of settlement headroom. Before dispatch, the gateway checks the **actual** lease
expiry, conservatively rounded to its persisted second precision. If reservation
work consumed too much time, it returns 503, releases the lease and sends no
upstream request or usage event. It never silently reduces the configured limit.
The nominal maximum of 840000 ms consequently cannot fit after admission spends
any time; choose a lower operational ceiling, such as the example's 600000 ms.

Headroom is an opportunity for settlement, **not a guarantee**: settlement can
still wait on ledger/SQLite contention. Longer calls need a separate lease renewal
and bounded settlement contract. Streaming idle limits do not bound total stream
lifetime. An opt-in Rust [body lifetime owner](stream-body-lifetime.md) exists;
standalone streaming policy and live lifecycle acceptance remain open.

## Regression ownership

The existing config suite owns validation and inheritance; the proxy integration
suite owns authorized routing, both transport planes, startup-only status and
pre-dispatch refusal. Existing raw and resilience suites own timer enforcement;
one scope test owns concurrent/nested/cancellation isolation. The existing HTTP
adapter timeout tests remain distinct from unit doubles. No duplicate owner was
identified or deleted. These local tests are not live model acceptance evidence.
