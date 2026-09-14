/**
 * The `test` / `expect` every spec imports.
 *
 * It is a thin re-export of `@playwright/test` in normal runs — importing
 * from here instead of straight from the package costs nothing and changes
 * no behaviour.
 *
 * When `PECKBOARD_E2E_IMPACT_DIR` is set (only `scripts/e2e-impact-map.sh`
 * does that), it additionally records, per spec file, which `web/src/**`
 * modules actually executed. That is the frontend half of the impact map:
 * e2e specs never *import* app source — they drive a browser — so there is
 * no static edge from `web/src/components/Modal.tsx` to
 * `modal-portal.spec.ts`. Runtime coverage is the only thing that knows.
 *
 * The backend half needs no fixture: `impact/reporter.ts` records each
 * test's wall-clock window and the server logs every matched route with a
 * timestamp. One server per shard at `workers: 1` means exactly one test is
 * in flight at a time, so the join is exact.
 */
import { test as base } from '@playwright/test'
import { appendFileSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

export * from '@playwright/test'

const IMPACT_DIR = process.env.PECKBOARD_E2E_IMPACT_DIR
// One file per shard: shards are separate processes, and an appended JSONL
// line longer than a pipe-buffer write is not atomic, so a shared file
// would interleave into unparseable records.
const SHARD = process.env.PECKBOARD_E2E_SHARD ?? '1'
const HERE = path.dirname(fileURLToPath(import.meta.url))

export const test = base.extend({
  // The fixture callback's second argument is Playwright's "hand the value
  // to the test" hook. It is conventionally named `use`, but that trips
  // eslint's react-hooks rule, so: `provide`.
  page: async ({ page }, provide, testInfo) => {
    if (!IMPACT_DIR) {
      await provide(page)
      return
    }
    // resetOnNavigation: false — a spec that reloads mid-test must not
    // lose the modules the first load exercised.
    await page.coverage.startJSCoverage({ resetOnNavigation: false })
    let entries: Awaited<ReturnType<typeof page.coverage.stopJSCoverage>> = []
    try {
      await provide(page)
    } finally {
      try {
        entries = await page.coverage.stopJSCoverage()
      } catch {
        // Page already closed by the spec — nothing to collect.
      }
    }
    if (!entries.length) return
    // Imported lazily so normal runs never pay to load the sourcemap decoder.
    const { sourceFilesTouched } = await import('./impact/coverage')
    const sources = sourceFilesTouched(entries)
    if (!sources.length) return
    mkdirSync(IMPACT_DIR, { recursive: true })
    appendFileSync(
      path.join(IMPACT_DIR, `frontend-${SHARD}.jsonl`),
      `${JSON.stringify({ spec: path.relative(HERE, testInfo.file), sources })}\n`,
    )
  },
})
