"""Create a new evaluator project with local-worker and Flask entrypoints."""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--name", default="example.keyword.v1")
    args = parser.parse_args()
    if not 1 <= len(args.name) <= 64 or any(
        not c.isascii() or not (c.isalnum() or c in "._-") for c in args.name
    ):
        parser.error(
            "name must contain 1..64 ASCII letters, digits, dot, underscore or hyphen"
        )
    source = Path(__file__).resolve().parents[1] / "templates/python-evaluator"
    target = args.destination.absolute()
    # Never overwrite an existing project or follow a destination symlink.
    target.mkdir(mode=0o700)
    for entry in source.iterdir():
        if entry.name in {"__pycache__", ".pytest_cache", ".runtime", "build"}:
            continue
        if entry.is_dir():
            shutil.copytree(
                entry,
                target / entry.name,
                ignore=shutil.ignore_patterns("__pycache__", ".pytest_cache", "*.pyc"),
            )
        else:
            shutil.copyfile(entry, target / entry.name)
    (target / "evaluator.json").write_text(json.dumps({"name": args.name}) + "\n")
    subprocess.run([sys.executable, str(target / "build_bundle.py")], check=True)


if __name__ == "__main__":
    main()
