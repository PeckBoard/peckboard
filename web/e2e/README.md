# Peckboard e2e tests

Playwright scaffolding. No specs exist yet — this directory only holds the
infrastructure that future tests will share.

## Running

```bash
cd web
npm install                 # picks up @playwright/test
npm run e2e:install         # one-time: download browser binaries
npm run e2e                 # runs anything in web/e2e/tests/
```

`global-setup.ts` rebuilds the frontend and the release binary before the
suite runs. The Playwright `webServer` block then launches the binary with
a fresh `mktemp -d` data dir so prior runs cannot bleed into the next.

## Using the mock provider

The binary registers the `mock` agent provider alongside `claude`, which
means tests can drive deterministic agent runs without needing the real
`claude` CLI on PATH or an LLM bill. Send a message with model
`mock:echo`, `mock:happy-path`, `mock:tool-use`, `mock:crash`, or
`mock:ask` and the mock provider will emit a scripted `ProviderEvent`
sequence. See `src/provider/mock/mod.rs` for the full list.
