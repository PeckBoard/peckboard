/**
 * Maps V8 JS coverage from a Playwright page back to `web/src/**` files.
 *
 * The e2e suite drives the RELEASE binary, which serves the minified vite
 * bundle — so raw coverage offsets mean nothing on their own. We decode the
 * bundle's sourcemap once per worker process and resolve each executed
 * function's start offset to its original source path.
 *
 * We only care about the SET of source files a test touched, never about
 * line-level coverage, so mapping one offset per executed function is
 * enough (and far cheaper than mapping every range).
 *
 * Requires the bundle to be built with sourcemaps:
 * `PECKBOARD_E2E_COVERAGE=1 npm run build` (see web/vite.config.ts).
 */
import { readFileSync } from 'node:fs'
import path from 'node:path'
import { TraceMap, originalPositionFor } from '@jridgewell/trace-mapping'

type CoverageEntry = {
  url: string
  source?: string
  functions: { ranges: { startOffset: number; endOffset: number; count: number }[] }[]
}

/** Decoded sourcemap + a line-start index for the generated file. */
type Resolver = { map: TraceMap; lineStarts: number[] }

// One decode per bundle chunk per worker process; specs re-enter this
// module for every test, and decoding a 1.2 MB bundle map is not cheap.
const resolvers = new Map<string, Resolver | null>()

const distDir = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..', '..', 'dist')

/** Byte offsets of every line start, so an offset becomes a line/column. */
function indexLines(source: string): number[] {
  const starts = [0]
  for (let i = 0; i < source.length; i++) {
    if (source.charCodeAt(i) === 10) starts.push(i + 1)
  }
  return starts
}

function resolverFor(entry: CoverageEntry): Resolver | null {
  const name = entry.url.split('/').pop() ?? ''
  if (resolvers.has(name)) return resolvers.get(name) ?? null
  let resolver: Resolver | null
  try {
    const raw = readFileSync(path.join(distDir, 'assets', `${name}.map`), 'utf8')
    resolver = { map: new TraceMap(JSON.parse(raw)), lineStarts: indexLines(entry.source ?? '') }
  } catch {
    // Not one of our chunks (or built without sourcemaps) — skip it.
    resolver = null
  }
  resolvers.set(name, resolver)
  return resolver
}

/** Binary search: byte offset -> 1-based line, 0-based column. */
function positionOf(lineStarts: number[], offset: number): { line: number; column: number } {
  let lo = 0
  let hi = lineStarts.length - 1
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1
    if (lineStarts[mid] <= offset) lo = mid
    else hi = mid - 1
  }
  return { line: lo + 1, column: offset - lineStarts[lo] }
}

/**
 * The set of repo-relative `web/src/**` paths whose code actually ran.
 * Paths are returned repo-relative (e.g. `web/src/components/Modal.tsx`)
 * so they can be compared directly against `git diff --name-only`.
 */
export function sourceFilesTouched(entries: CoverageEntry[]): string[] {
  const touched = new Set<string>()
  for (const entry of entries) {
    const resolver = resolverFor(entry)
    if (!resolver) continue
    for (const fn of entry.functions) {
      for (const range of fn.ranges) {
        if (range.count === 0) continue
        const pos = positionOf(resolver.lineStarts, range.startOffset)
        const origin = originalPositionFor(resolver.map, pos)
        if (!origin.source) continue
        // Sourcemap sources are relative to web/ — e.g. `../src/App.tsx`
        // or `src/App.tsx` depending on the chunk. Normalise to
        // `web/src/...`, and drop anything outside our own source tree
        // (node_modules, virtual vite modules).
        const idx = origin.source.lastIndexOf('src/')
        if (idx === -1) continue
        if (origin.source.includes('node_modules')) continue
        touched.add(`web/${origin.source.slice(idx)}`)
        break // one hit per function is enough to name the file
      }
    }
  }
  return [...touched].sort()
}
