from pathlib import Path
import re

import yaml


ROOT = Path(__file__).resolve().parents[2]
EXPECTED_CODEQL_SHA = "33d7b85da1a04171e211befe2c739303b9a4c066"


def test_codeql_actions_use_one_valid_immutable_pin():
    document = yaml.load(
        (ROOT / ".github/workflows/codeql.yml").read_text(), Loader=yaml.BaseLoader
    )
    actions = [
        step["uses"]
        for job in document["jobs"].values()
        for step in job.get("steps", [])
        if "uses" in step
    ]
    codeql = [action for action in actions if action.startswith("github/codeql-action/")]
    assert len(codeql) == 2
    assert {action.rsplit("@", 1)[1] for action in codeql} == {EXPECTED_CODEQL_SHA}
    assert all(re.fullmatch(r"[^@]+@[0-9a-f]{40}", action) for action in actions)
    cells = document["jobs"]["analyze"]["strategy"]["matrix"]["include"]
    assert {(cell["language"], cell["build-mode"]) for cell in cells} == {
        ("rust", "none"),
        ("python", "none"),
        ("javascript-typescript", "none"),
    }
