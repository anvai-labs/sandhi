"""Import isolation for shared/self-hosted CI runners (see PR #180 for the workflow-side
half of this defense).

The wheel `pip install dist/*.whl` just placed is the module under test — but a stale
`sandhi_gateway` anywhere earlier on `sys.path` (a globally-set PYTHONPATH, a user-site
left by another job, a stray checkout) shadows it *silently*: the old module satisfies
every pre-existing assertion and only newly admitted catalog slugs fail, which reads as a
product bug instead of an environment bug. Force the interpreter's own site directories to
the front of `sys.path` so the freshly installed wheel always wins, and drop any copy that
was imported before pytest collected the tests.
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
