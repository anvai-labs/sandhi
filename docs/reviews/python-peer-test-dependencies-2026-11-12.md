# Python peer tests require explicit asyncio support

The public CI run for `12d7291` passed Rust, Node and the complete SDK suite, but both Python jobs failed: 52 new asynchronous peer cases could not run because the jobs installed pytest alone. The x86 and first ARM interpreter each passed the 271 synchronous cases and failed those 52 async cases; later ARM interpreters were not reached. This is a test dependency failure, not passing peer qualification.

One pinned `bindings/python/requirements-test.txt` now supplies pytest and pytest-asyncio to the regular wheel job, every ARM interpreter and the binding coverage harness. Local testing instructions use the same file. Runtime dependencies and binding implementation are unchanged by this increment.

A fresh isolated Python 3.12 environment, without system packages, reproduces all 52 async failures against archived exact `12d7291` tests and its reviewed development wheel. Installing the shared manifest then passes all 323 cases in 16.13 seconds. Workflow path/public-runner guards and shell syntax also pass. The [artifact](artifacts/python-peer-test-dependencies-2026-11-12.json) retains failure and correction hashes, source scope and qualification limits.

Corrected public CI and independent exact-tip review remain required. This increment contains none of the separately prepared native-source, provider-namespace, canonical-accounting or native-reference-ownership changes.
