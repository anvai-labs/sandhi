"""Exercise source/site boundaries without copying repository documentation."""

import importlib.util
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from mkdocs.config.defaults import MkDocsConfig
from mkdocs.structure.files import Files

spec = importlib.util.spec_from_file_location(
    "docs_links", Path(__file__).resolve().parents[2] / "scripts/docs_links.py"
)
links = importlib.util.module_from_spec(spec)
spec.loader.exec_module(links)


class DocumentationLinksTest(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        self.docs = self.root / "docs"
        for name in (
            "README.md",
            "crates/core/lib.rs",
            "docs/README.md",
            "docs/operator/start.md",
            "docs/operator/next.md",
            "docs/product/evidence/run.json",
            "docs/design.adoc",
        ):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name)
        for name, value in (("ROOT", self.root), ("DOCS", self.docs)):
            patcher = patch.object(links, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.page = SimpleNamespace(
            file=SimpleNamespace(abs_src_path=str(self.docs / "operator/start.md"))
        )

    def test_preserves_local_external_missing_and_literal_links(self):
        source = (
            "[Next](next.md#heading) [Missing](missing.md) "
            "[External](https://example.org/a) [Anchor](#here)\n"
            "```markdown\n[Example](../../README.md)\n```\n"
        )
        self.assertEqual(links.on_page_markdown(source, self.page), source)

    def test_links_non_site_sources_to_repository_without_losing_fragments(self):
        for target, expected in (
            ("../../crates/core/lib.rs#L1", "blob/develop/crates/core/lib.rs#L1"),
            ("../../crates/core", "tree/develop/crates/core"),
            (
                "../product/evidence/run.json",
                "blob/develop/docs/product/evidence/run.json",
            ),
            ("../design.adoc", "blob/develop/docs/design.adoc"),
            ("../../README.md", "blob/develop/README.md"),
        ):
            with self.subTest(target=target):
                self.assertEqual(
                    links.on_page_markdown(f"[Source]({target})", self.page),
                    f"[Source](https://github.com/anvai-labs/sandhi/{expected})",
                )

    def test_documentation_map_reuses_authoritative_readme(self):
        config = MkDocsConfig()
        config.load_dict({"site_name": "Test", "docs_dir": str(self.docs)})
        errors, _warnings = config.validate()
        self.assertEqual(errors, [])
        # MkDocs' event dispatcher sets this before invoking a build hook.
        config.plugins._current_plugin = "docs_links"
        files = links.on_files(Files([]), config)
        self.assertEqual(
            files.get_file_from_path("map.md").content_string, "docs/README.md"
        )
        self.assertEqual(
            links.on_page_markdown("[Map](../README.md#status)", self.page),
            "[Map](../map.md#status)",
        )


if __name__ == "__main__":
    unittest.main()
