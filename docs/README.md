# PeckBoard Docs Site

This directory is the source for the public documentation site, built
with Jekyll (just-the-docs theme) and deployed to GitHub Pages via
GitHub Actions. The `architecture/`, `api/`, `frontend/`, `tasks/`
directories and `EPIC.md` are internal development docs and are
excluded from the published site (see `exclude:` in `_config.yml`).

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
the free plan. The repo must be public (or the org on a paid plan)
before Pages can be enabled — until then the deploy job will fail.
