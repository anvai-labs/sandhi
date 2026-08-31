"""Import isolation for shared/self-hosted CI runners (see PR #180 for the workflow-side
half of this defense).

The wheel `pip install dist/*.whl` just placed is the module under test — but a stale
`sandhi_gateway` anywhere earlier on `sys.path` (a globally-set PYTHONPATH, a user-site
left by another job, a stray checkout) shadows it *silently*: the old module satisfies
every pre-existing assertion and only newly admitted catalog slugs fail, which reads as a
product bug instead of an environment bug. Force the interpreter's own site directories to
the front of `sys.path` so the wheel wins against PYTHONPATH and checkout copies, and drop
any copy that was imported before pytest collected the tests. Scope note: this half pins
`sysconfig` purelib/platlib only — a pip `--user` fallback install (non-writable
site-packages) lands outside them, which the workflow-side half of the defense (PR #180:
`PYTHONNOUSERSITE=1` plus a wheel-origin assertion) suppresses rather than this file.
"""

import sys
import sysconfig

_SITE_DIRS = [
    path
    for path in (
        sysconfig.get_paths().get("purelib"),
        sysconfig.get_paths().get("platlib"),
    )
    if path
]

for path in _SITE_DIRS:
    while path in sys.path:
        sys.path.remove(path)
for path in reversed(_SITE_DIRS):
    sys.path.insert(0, path)

sys.modules.pop("sandhi_gateway", None)
