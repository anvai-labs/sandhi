"""Keep repository-relative source links useful in the generated docs site."""

from pathlib import Path
import posixpath
import re
from urllib.parse import quote, urlsplit, urlunsplit
from mkdocs.structure.files import File

ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs"
LINK = re.compile(r"(?P<prefix>!?\[[^\]\n]*\]\()(?P<url>[^\s)]+)(?P<suffix>\))")


def on_files(files, config):
    # README and index otherwise both map to index.html. Reuse the authoritative
    # source file under a distinct site route, without maintaining a copied page.
    files.append(File.generated(config, "map.md", abs_src_path=str(DOCS / "README.md")))
    return files


def on_page_markdown(markdown, page, **_kwargs):
    """Link non-rendered repository files to source; keep site links local."""
    parent = Path(page.file.abs_src_path).parent

    def rewrite(match):
        parsed = urlsplit(match["url"])
        if (
            parsed.scheme
            or parsed.netloc
            or not parsed.path
            or parsed.path.startswith("/")
        ):
            return match[0]
        target = (parent / parsed.path).resolve()
        if not target.is_relative_to(ROOT) or not target.exists():
            return match[0]  # MkDocs must report broken links; do not hide them.
        if target == DOCS / "README.md":
            path = posixpath.relpath("map.md", parent.relative_to(DOCS).as_posix())
            return (
                match["prefix"]
                + urlunsplit(("", "", path, parsed.query, parsed.fragment))
                + match["suffix"]
            )
        outside = not target.is_relative_to(DOCS)
        directory = target.is_dir()
        evidence = target.is_relative_to(DOCS / "product" / "evidence")
        if outside or directory or evidence or target.suffix == ".adoc":
            kind = "tree" if directory else "blob"
            path = quote(target.relative_to(ROOT).as_posix(), safe="/")
            url = urlunsplit(
                (
                    "https",
                    "github.com",
                    f"/anvai-labs/sandhi/{kind}/develop/{path}",
                    parsed.query,
                    parsed.fragment,
                )
            )
            return match["prefix"] + url + match["suffix"]
        return match[0]

    # Never alter literal examples inside fenced code blocks.
    parts = re.split(
        r"(^```[^\n]*\n.*?^```[^\n]*$)", markdown, flags=re.MULTILINE | re.DOTALL
    )
    return "".join(
        part if index % 2 else LINK.sub(rewrite, part)
        for index, part in enumerate(parts)
    )
