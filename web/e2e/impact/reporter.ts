/**
 * Records each test's wall-clock window so the server's route log can be
 * attributed back to a spec file.
 *
 * `workers: 1` plus one server per shard means exactly one test is in
 * flight at a time, so "which test was running at time T" has a single
 * answer. Anything the server did outside every window (boot work, the
 * repeating-task sweep) is simply unattributed.
 *
 * Only active under `scripts/e2e-impact-map.sh`, which sets
 * `PECKBOARD_E2E_IMPACT_DIR`.
 */
import type { Reporter, TestCase, TestResult } from '@playwright/test/reporter'
import { appendFileSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const E2E_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')

export default class ImpactReporter implements Reporter {
  onTestEnd(test: TestCase, result: TestResult) {
    const dir = process.env.PECKBOARD_E2E_IMPACT_DIR
    if (!dir) return
    // A test that never ran (skipped) exercised nothing.
    if (result.status === 'skipped') return
    const start = result.startTime.getTime()
    // Per shard, for the same reason as the coverage records: separate
    // processes appending to one file can interleave a long line.
    const shard = process.env.PECKBOARD_E2E_SHARD ?? '1'
    mkdirSync(dir, { recursive: true })
    appendFileSync(
      path.join(dir, `timing-${shard}.jsonl`),
      `${JSON.stringify({
        spec: path.relative(E2E_DIR, test.location.file),
        shard,
        start,
        end: start + result.duration,
      })}\n`,
    )
  }
}
