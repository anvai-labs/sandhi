# Maintain the documentation

| Task | Command |
|---|---|
| Install the qualified toolchain | `python -m pip install -r requirements-docs.txt` |
| Build and reject warnings | `python -m mkdocs build --strict` |
| Preview locally | `python -m mkdocs serve -a 127.0.0.1:8000` |

Run these from the repository root in an isolated virtual environment.

The `Documentation` workflow checks links and builds the site for every PR. Only
trusted `develop` pushes (or an explicit dispatch on `develop`) can publish the
artifact to GitHub Pages. Enable Pages with **GitHub Actions** as the source and
allow the `github-pages` environment to deploy `develop`. PR builds have read-only
permissions and cannot deploy. The banner labels this as development documentation;
publishing it does not promote a runtime release.

## One source, several views

- Edit canonical Markdown in `docs/`; do not copy runtime contracts into a second site tree.
- Keep diagrams beside a readable table or list that states the same important facts.
- Lead operator pages with status, configuration and outcomes. Keep detailed rationale in ADRs/TDs.
- Source links to files outside `docs/`, directories and AsciiDoc open the repository;
  MkDocs does not pretend to render those formats. Missing targets remain build errors.
- Label source-only work explicitly. A green unit test, a source merge, a published artifact
  and an accepted deployment are different evidence.

## Family styling

The local CSS uses AnvaiOps' neutral teal/slate palette from
`site/assets/brand.css` at `46b8afcc3030a5a83ed70f4f8e1e5acd7d8ddc39`.
It is a small OSS styling surface, not a dependency on the commercial control plane.
Light, dark and system modes share the same content and navigation. System fonts
avoid external font requests; reduced motion and keyboard focus remain explicit.

Use Material's native [palette configuration](https://squidfunk.github.io/mkdocs-material/setup/changing-the-colors/)
and [Mermaid diagrams](https://squidfunk.github.io/mkdocs-material/reference/diagrams/).
Mermaid rendering requires the browser to load Material's diagram dependency;
the adjacent tables remain the text equivalent.
