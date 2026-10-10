/**
 * Pure grid math for the widget dashboard (saved Views). A 12-column grid of
 * integer rects; bounds match the server contract (`x + w <= 12`, `w` 1..12,
 * `h` 2..40, `y >= 0`, no overlaps).
 *
 * Layout model: vertical compaction ("gravity up"). Every layout this module
 * returns is fully compacted — each widget sits directly under the lowest
 * widget above it in its columns, or at row 0. Ordering is by vertical
 * centre, which for a compacted layout is a linear extension of the "is
 * above" relation between column-overlapping widgets, so re-compacting a
 * compacted layout is a no-op. Moving a widget just gives it a new desired
 * centre: it swaps past a neighbour once its centre crosses the neighbour's,
 * and every widget below gets pushed down (or pulled up) to make room.
 */

export const GRID_COLS = 12
export const MIN_W = 1
export const MAX_W = GRID_COLS
export const MIN_H = 2
export const MAX_H = 40
export const MAX_WIDGETS = 24

export interface GridRect {
  x: number
  y: number
  w: number
  h: number
}

export interface GridItem extends GridRect {
  id: string
}

const clampInt = (v: number, lo: number, hi: number) =>
  Math.min(hi, Math.max(lo, Math.round(Number.isFinite(v) ? v : lo)))

/** Snap a rect to integers inside the contract bounds. */
export function clampRect(r: GridRect): GridRect {
  const w = clampInt(r.w, MIN_W, MAX_W)
  const h = clampInt(r.h, MIN_H, MAX_H)
  return { x: clampInt(r.x, 0, GRID_COLS - w), y: Math.max(0, Math.round(r.y) || 0), w, h }
}

export function collides(a: GridRect, b: GridRect): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

const overlapsX = (a: GridRect, b: GridRect) => a.x < b.x + b.w && b.x < a.x + a.w

/** Total rows the layout occupies. */
export function gridHeight(items: GridRect[]): number {
  return items.reduce((m, i) => Math.max(m, i.y + i.h), 0)
}

/**
 * Compact `items` upward. `keys` overrides the sort key (vertical centre) of
 * specific widgets — how a dragged widget claims its spot. Ties go to the
 * overridden widget, then left-to-right, then id, so the result is stable.
 */
function compactBy<T extends GridItem>(items: T[], keys: Record<string, number> = {}): T[] {
  const keyOf = (i: T) => keys[i.id] ?? i.y + i.h / 2
  const order = [...items].sort((a, b) => {
    const d = keyOf(a) - keyOf(b)
    if (d !== 0) return d
    const pa = a.id in keys ? 0 : 1
    const pb = b.id in keys ? 0 : 1
    if (pa !== pb) return pa - pb
    return a.x - b.x || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)
  })
  const placed: T[] = []
  for (const item of order) {
    let y = 0
    for (const p of placed) if (overlapsX(p, item)) y = Math.max(y, p.y + p.h)
    placed.push({ ...item, y })
  }
  const byId = new Map(placed.map((p) => [p.id, p]))
  return items.map((i) => byId.get(i.id)!)
}

/** Clamp every rect and compact; resolves any overlaps a stored layout has. */
export function compact<T extends GridItem>(items: T[]): T[] {
  return compactBy(items.map((i) => ({ ...i, ...clampRect(i) })))
}

/**
 * Move `id` toward `rect` (desired grid position, possibly overlapping
 * others): colliding widgets are pushed down, the rest close up. `key`
 * overrides the widget's sort centre (keyboard moves use it to step past
 * exactly one neighbour).
 */
export function moveItem<T extends GridItem>(
  items: T[],
  id: string,
  rect: GridRect,
  key?: number,
): T[] {
  const r = clampRect(rect)
  const next = items.map((i) => (i.id === id ? { ...i, ...r } : i))
  return compactBy(next, { [id]: key ?? r.y + r.h / 2 })
}

/**
 * Keyboard move: one column left/right, or past the nearest neighbour
 * above/below in the widget's columns.
 */
export function nudgeItem<T extends GridItem>(
  items: T[],
  id: string,
  dir: 'left' | 'right' | 'up' | 'down',
): T[] {
  const cur = items.find((i) => i.id === id)
  if (!cur) return items
  if (dir === 'left' || dir === 'right') {
    return moveItem(items, id, { ...cur, x: cur.x + (dir === 'left' ? -1 : 1) })
  }
  const c = cur.y + cur.h / 2
  const centres = items
    .filter((i) => i.id !== id && overlapsX(i, cur))
    .map((i) => i.y + i.h / 2)
    .filter((ic) => (dir === 'up' ? ic < c : ic > c))
  if (centres.length === 0) return items
  const target = dir === 'up' ? Math.max(...centres) - 0.5 : Math.min(...centres) + 0.5
  return moveItem(items, id, cur, target)
}

/**
 * Resize `id` to `size` keeping its top-left. Its order is anchored on its
 * current centre so growing pushes neighbours below down rather than
 * leapfrogging them.
 */
export function resizeItem<T extends GridItem>(
  items: T[],
  id: string,
  size: { w: number; h: number },
): T[] {
  const cur = items.find((i) => i.id === id)
  if (!cur) return items
  const r = clampRect({ x: cur.x, y: cur.y, w: size.w, h: size.h })
  const next = items.map((i) => (i.id === id ? { ...i, ...r } : i))
  return compactBy(next, { [id]: cur.y + cur.h / 2 })
}

/** First top-left-most free slot for a `w`×`h` widget (row-major scan). */
export function findFreeSlot(items: GridRect[], w: number, h: number): GridRect {
  const size = clampRect({ x: 0, y: 0, w, h })
  const bottom = gridHeight(items)
  for (let y = 0; y <= bottom; y++) {
    for (let x = 0; x + size.w <= GRID_COLS; x++) {
      const r = { x, y, w: size.w, h: size.h }
      if (!items.some((i) => collides(i, r))) return r
    }
  }
  return { x: 0, y: bottom, w: size.w, h: size.h }
}

/** Same rects (ignoring order and non-geometry fields)? */
export function sameLayout(a: GridItem[], b: GridItem[]): boolean {
  if (a.length !== b.length) return false
  const m = new Map(b.map((i) => [i.id, i]))
  return a.every((i) => {
    const o = m.get(i.id)
    return !!o && o.x === i.x && o.y === i.y && o.w === i.w && o.h === i.h
  })
}

export type StarterShape = 'columns' | 'rows' | 'grid' | 'main-stack'

/** `total` split into `k` near-equal integer widths. */
function splitWidths(k: number, total = GRID_COLS): number[] {
  const base = Math.floor(total / k)
  const extra = total - base * k
  return Array.from({ length: k }, (_, i) => base + (i < extra ? 1 : 0))
}

function rowsOf(n: number, perRow: number, h: number): GridRect[] {
  const out: GridRect[] = []
  for (let row = 0; out.length < n; row++) {
    const k = Math.min(perRow, n - out.length)
    let x = 0
    for (const w of splitWidths(k)) {
      out.push({ x, y: row * h, w, h })
      x += w
    }
  }
  return out
}

/** Rects for a new view of `n` widgets laid out in a starter shape. */
export function starterRects(shape: StarterShape, n: number): GridRect[] {
  if (n <= 0) return []
  switch (shape) {
    case 'rows':
      return rowsOf(n, 1, 8)
    case 'grid': {
      const perRow = Math.min(4, Math.ceil(Math.sqrt(n)))
      return rowsOf(n, perRow, 10)
    }
    case 'main-stack': {
      if (n === 1) return [{ x: 0, y: 0, w: 12, h: 16 }]
      const h = Math.max(MIN_H, Math.floor(16 / (n - 1)))
      return [
        { x: 0, y: 0, w: 8, h: Math.max(16, h * (n - 1)) },
        ...Array.from({ length: n - 1 }, (_, i) => ({ x: 8, y: i * h, w: 4, h })),
      ]
    }
    default:
      return rowsOf(n, 4, 16)
  }
}

/** Client-generated widget id (the server keeps it). `randomUUID` needs a
 *  secure context; plain-http LAN installs fall back to a random string. */
export function newWidgetId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    try {
      return `w-${crypto.randomUUID()}`
    } catch {
      // insecure context
    }
  }
  return `w-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`
}
