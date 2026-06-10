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
