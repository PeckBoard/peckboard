/**
 * A dependency-free line diff, sized for the review screen's history tab.
 *
 * Two markdown versions of the same document are usually near-identical, so
 * the expensive part (the LCS table) only ever runs over the lines that
 * actually differ: the common prefix and suffix are trimmed first, and a
 * pathological middle falls back to "replace the block" rather than
 * allocating a table nobody has the memory for.
 *
 * Pure and side-effect free — the whole module is one function over two
 * strings, which is what makes it testable without a DOM.
 */

/** What happened to one line between the two versions. */
export type LineOp = 'context' | 'add' | 'del'

export interface DiffLine {
  op: LineOp
  text: string
  /** 1-based line number in the OLD version; null for an addition. */
  oldLine: number | null
  /** 1-based line number in the NEW version; null for a deletion. */
  newLine: number | null
}

/** A run of changes plus its surrounding context, as `@@ -a,b +c,d @@`. */
export interface DiffHunk {
  oldStart: number
  oldCount: number
  newStart: number
  newCount: number
  lines: DiffLine[]
}

export interface LineDiff {
  hunks: DiffHunk[]
  added: number
  removed: number
  /** True when the middle section was too large for an exact LCS and the
   *  changed block was emitted as a wholesale delete + insert. The diff is
   *  still correct — just not minimal. */
  approximate: boolean
}

/** Cells of the LCS table we're willing to allocate (one Uint32 each, so
 *  ~8 MB at the cap). Beyond that the two documents have nothing in common
 *  anyway and a minimal diff would be unreadable. */
const MAX_CELLS = 2_000_000

/** Lines either side of a change that stay in the hunk. */
const DEFAULT_CONTEXT = 3

/** Split on newlines, dropping the empty tail a trailing newline produces —
 *  otherwise every document ends with a phantom blank line. */
function splitLines(text: string): string[] {
  if (text === '') return []
  const lines = text.split('\n')
  if (lines[lines.length - 1] === '') lines.pop()
  return lines
}

/** Ops for the changed middle section, offset so line numbers are absolute. */
function diffMiddle(
  a: string[],
  b: string[],
  oldOffset: number,
  newOffset: number,
): { lines: DiffLine[]; approximate: boolean } {
  const n = a.length
  const m = b.length
  const out: DiffLine[] = []

  if (n === 0 || m === 0 || (n + 1) * (m + 1) > MAX_CELLS) {
    // Nothing to align (a pure insert / delete), or too big to align.
    a.forEach((text, i) => out.push({ op: 'del', text, oldLine: oldOffset + i + 1, newLine: null }))
    b.forEach((text, j) => out.push({ op: 'add', text, oldLine: null, newLine: newOffset + j + 1 }))
    return { lines: out, approximate: n > 0 && m > 0 }
  }

  // LCS lengths, filled from the end so the walk below can go forward and
  // keep equal-cost deletions before insertions (the shape a reader expects).
  const w = m + 1
  const dp = new Uint32Array((n + 1) * w)
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i * w + j] =
        a[i] === b[j]
          ? dp[(i + 1) * w + j + 1] + 1
          : Math.max(dp[(i + 1) * w + j], dp[i * w + j + 1])
    }
  }

  let i = 0
  let j = 0
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({
        op: 'context',
        text: a[i],
        oldLine: oldOffset + i + 1,
        newLine: newOffset + j + 1,
      })
      i++
      j++
    } else if (dp[(i + 1) * w + j] >= dp[i * w + j + 1]) {
      out.push({ op: 'del', text: a[i], oldLine: oldOffset + i + 1, newLine: null })
      i++
    } else {
      out.push({ op: 'add', text: b[j], oldLine: null, newLine: newOffset + j + 1 })
      j++
    }
  }
  for (; i < n; i++) out.push({ op: 'del', text: a[i], oldLine: oldOffset + i + 1, newLine: null })
  for (; j < m; j++) out.push({ op: 'add', text: b[j], oldLine: null, newLine: newOffset + j + 1 })

  return { lines: out, approximate: false }
}

/** Group the changed lines into hunks, keeping `context` lines either side
 *  and merging runs whose context windows touch. */
function toHunks(lines: DiffLine[], context: number): DiffHunk[] {
  const changed: number[] = []
  lines.forEach((l, idx) => {
    if (l.op !== 'context') changed.push(idx)
  })
  if (changed.length === 0) return []

  const ranges: [number, number][] = []
  for (const idx of changed) {
    const from = Math.max(0, idx - context)
    const to = Math.min(lines.length - 1, idx + context)
    const last = ranges[ranges.length - 1]
    // `+ 1` merges windows that merely abut: a one-line gap of context reads
    // better than two hunk headers.
    if (last && from <= last[1] + 1) last[1] = Math.max(last[1], to)
    else ranges.push([from, to])
  }

  return ranges.map(([from, to]) => {
    const slice = lines.slice(from, to + 1)
    const oldLines = slice.filter((l) => l.oldLine !== null)
    const newLines = slice.filter((l) => l.newLine !== null)
    return {
      oldStart: oldLines[0]?.oldLine ?? 0,
      oldCount: oldLines.length,
      newStart: newLines[0]?.newLine ?? 0,
      newCount: newLines.length,
      lines: slice,
    }
  })
}

/**
 * Diff two documents by line. Returns hunks ready to render, plus the
 * +/− totals for a summary line.
 */
export function diffLines(
  before: string,
  after: string,
  opts: { context?: number } = {},
): LineDiff {
  const context = opts.context ?? DEFAULT_CONTEXT
  const a = splitLines(before)
  const b = splitLines(after)

  let start = 0
  while (start < a.length && start < b.length && a[start] === b[start]) start++
  let endA = a.length
  let endB = b.length
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--
    endB--
  }

  const lines: DiffLine[] = []
  for (let i = 0; i < start; i++) {
    lines.push({ op: 'context', text: a[i], oldLine: i + 1, newLine: i + 1 })
  }
  const middle = diffMiddle(a.slice(start, endA), b.slice(start, endB), start, start)
  lines.push(...middle.lines)
  for (let i = endA; i < a.length; i++) {
    lines.push({ op: 'context', text: a[i], oldLine: i + 1, newLine: endB + (i - endA) + 1 })
  }

  return {
    hunks: toHunks(lines, context),
    added: lines.filter((l) => l.op === 'add').length,
    removed: lines.filter((l) => l.op === 'del').length,
    approximate: middle.approximate,
  }
}

/** `@@ -1,4 +1,6 @@` — the standard unified-diff hunk header. */
export function hunkHeader(h: DiffHunk): string {
  return `@@ -${h.oldStart},${h.oldCount} +${h.newStart},${h.newCount} @@`
}
