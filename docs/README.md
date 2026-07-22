# PeckBoard Docs Site

This directory is the source for the public documentation site, built
with Jekyll (just-the-docs theme) and deployed to GitHub Pages via
GitHub Actions. The `architecture/`, `api/`, `frontend/`, `tasks/`,
`proposals/` directories and `EPIC.md` are internal development docs
and are excluded from the published site (see `exclude:` in
`_config.yml`).

## Local Preview

Requires Ruby 3.x with Bundler (e.g. `brew install ruby`).

```bash
cd docs
bundle install
bundle exec jekyll serve
```

Then open <http://127.0.0.1:4000/>. The site's `baseurl` is empty (it
is served from the root of <https://peckboard.com/>), so no override is
needed locally.

## Build Without Ruby (Docker)

The exact image GitHub Actions uses can build the site locally; run
from the repository root:

```bash
docker run --rm -v "$PWD:/github/workspace" -w /github/workspace \
  -e GITHUB_WORKSPACE=/github/workspace \
  -e INPUT_SOURCE=./docs -e INPUT_DESTINATION=./docs/_site \
  ghcr.io/actions/jekyll-build-pages:v1.0.13
```

Output lands in `docs/_site/`, ready to serve from the domain root —
`baseurl` is empty.

## Rendering Tests

`web/e2e/docs/docs.spec.ts` drives Playwright over the built site:
every page's heading and sidebar, internal links, images, Mermaid
diagrams, collapsible blocks, and search. Build the site first (either
method above), then:

```bash
cd web
npm run e2e:docs
```

## Building Only

```bash
cd docs
bundle exec jekyll build
```

Output lands in `docs/_site/` (gitignored).

## Deployment

`.github/workflows/docs.yml` builds this directory with
`actions/jekyll-build-pages` and deploys it with `actions/deploy-pages`
on every push to `main` that touches `docs/**` (or via manual
`workflow_dispatch`). No `gh-pages` branch is involved.

The published site is <https://peckboard.com/> — a custom domain set in
Settings → Pages (source: **GitHub Actions**, i.e.
`build_type=workflow`), with HTTPS enforced and DNS fronted by
Cloudflare.

### If the Site Suddenly Shows a Rendered README

That is GitHub's legacy branch build (the `pages build and deployment`
workflow) having deployed a Jekyll render of the repository root over
the docs artifact; it can fire when Pages settings change. Confirm the
source is still GitHub Actions —

```bash
gh api repos/PeckBoard/peckboard/pages -X PUT -F build_type=workflow
```

— then re-run the "Deploy Docs to GitHub Pages" workflow to put the
docs artifact back.
