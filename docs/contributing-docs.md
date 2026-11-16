# Maintain the documentation

| Task | Command |
|---|---|
| Install the qualified toolchain | `python -m pip install -r requirements-docs.txt` |
| Build and reject warnings | `python -m mkdocs build --strict` |
| Preview locally | `python -m mkdocs serve -a 127.0.0.1:8000` |

Run these from the repository root in an isolated virtual environment.

The `Documentation` workflow checks links and builds the site for every PR and for
pushes to `develop` and `main`. Enable Pages with **GitHub Actions** as the source
and allow the `github-pages` environment to deploy `main`. PR and develop builds have
read-only permissions and cannot deploy; publication happens only after the reviewed
develop-to-main promotion. The published banner identifies the selected release
state; publishing documentation does not certify a managed deployment or
live-provider soak.

## One source, several views

- Edit canonical Markdown in `docs/`; do not copy runtime contracts into a second site tree.
- Keep diagrams beside a readable table or list that states the same important facts.
- Lead operator pages with status, configuration and outcomes. Keep detailed rationale in ADRs/TDs.
- Source links to files outside `docs/`, directories and AsciiDoc open the repository;
  MkDocs does not pretend to render those formats. Missing targets remain build errors.
- Label source-only work explicitly. A green unit test, a source merge, a published artifact
  and an accepted deployment are different evidence.

## Family styling

The site wears the Anvai family brand shared by every Anvai docs site:
`docs/stylesheets/tokens.css` is a verbatim copy of AnvaiOps `design/tokens.css`
(at `e8215fc`) and `docs/stylesheets/brand.css` is the shared Material mapping (ink
brand bar, teal accent). Both are copied files, not a dependency on the commercial
control plane; `scripts/check_brand_tokens.py` pins them by digest, so re-copy them
rather than editing here. Light, dark and system modes share the same content and
navigation. Inter is self-hosted under `docs/stylesheets/fonts/` (SIL OFL 1.1), so
there are no external font requests; reduced motion and keyboard focus remain explicit.

Use Material's native [palette configuration](https://squidfunk.github.io/mkdocs-material/setup/changing-the-colors/)
and [Mermaid diagrams](https://squidfunk.github.io/mkdocs-material/reference/diagrams/).
Mermaid rendering requires the browser to load Material's diagram dependency;
the adjacent tables remain the text equivalent.
