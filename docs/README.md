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
bundle exec jekyll serve --baseurl ""
```

Then open <http://127.0.0.1:4000/>.

The `--baseurl ""` flag overrides the production `baseurl: "/peckboard"`
so links work at the local server root.

## Build Without Ruby (Docker)

The exact image GitHub Actions uses can build the site locally; run
from the repository root:

```bash
docker run --rm -v "$PWD:/github/workspace" -w /github/workspace \
  -e GITHUB_WORKSPACE=/github/workspace \
  -e INPUT_SOURCE=./docs -e INPUT_DESTINATION=./docs/_site \
  ghcr.io/actions/jekyll-build-pages:v1.0.13
```

Output lands in `docs/_site/` with the production `/peckboard` baseurl
baked in.

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

### One-Time Manual Step: Enable GitHub Pages

The repo's Pages source must be set to **GitHub Actions** before the
deploy job can succeed:

```bash
gh api repos/PeckBoard/peckboard/pages -X POST -f build_type=workflow
```

(or Settings → Pages → Source → "GitHub Actions" in the web UI).

**Note:** GitHub Pages is not available for private repositories on
the free plan, so this step is deferred until the owner makes the repo
public. Until then the workflow fails at the "Setup Pages" step — this
is expected. No code change is needed when the repo goes public: enable
Pages with the command above and the workflow starts succeeding
automatically on the next push to `main`.
