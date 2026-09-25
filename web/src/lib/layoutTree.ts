/** Pure tree model behind `SplitLayout`: a pane layout is a tree of splits
 *  (row = side by side, col = stacked) whose leaves each show one session.
 *  Every op here is immutable and never clones a leaf object, so callers
 *  (and `moveLeaf`) can track a leaf by reference across edits. The shape is
 *  also what `/api/me/views` persists, so keep it JSON-plain. */

export type SplitDir = 'row' | 'col'

export type LayoutNode =
  | { kind: 'split'; dir: SplitDir; children: LayoutNode[]; ratios: number[] }
  | { kind: 'leaf'; sessionId: string | null }

export type LeafNode = Extract<LayoutNode, { kind: 'leaf' }>
export type SplitNode = Extract<LayoutNode, { kind: 'split' }>
export type Path = number[]
export type DropEdge = 'left' | 'right' | 'top' | 'bottom'
export type StarterLayout = 'columns' | 'rows' | 'grid' | 'main-stack'

/** Server-enforced limits for saved views (mirrored client-side). */
export const MAX_LEAVES = 16
export const MAX_DEPTH = 8

/** Rectangle in container fractions (0..1). */
export interface Rect {
  x: number
  y: number
  w: number
  h: number
}

export interface LeafEntry {
  key: string
  leaf: LeafNode
  sessionId: string | null
  path: Path
  rect: Rect
  /** Split edge a freshly inserted pane grows out of. */
  enterFrom: DropEdge
}

export interface DividerEntry {
  /** Path of the split node the divider belongs to. */
  path: Path
  /** Divider sits between children `index` and `index + 1`. */
  index: number
  dir: SplitDir
  /** The split node's rect. */
  nodeRect: Rect
  /** Absolute container fraction along the split axis. */
  pos: number
}

export function leaf(sessionId: string | null): LeafNode {
  return { kind: 'leaf', sessionId }
}

/** Stable identity for a leaf. Session leaves key by session id (views keep
 *  ids unique); empty leaves fall back to their path. */
export function leafKey(node: LeafNode, path: Path): string {
  return node.sessionId ?? `empty:${path.join('.')}`
}

export function equalRatios(n: number): number[] {
  return Array.from({ length: n }, () => 1 / n)
}

function normRatios(ratios: number[], n: number): number[] {
  if (ratios.length !== n || ratios.some((r) => !Number.isFinite(r) || r <= 0)) {
    return equalRatios(n)
  }
  const sum = ratios.reduce((a, b) => a + b, 0)
  return ratios.map((r) => r / sum)
}

export function countLeaves(node: LayoutNode | null): number {
  if (!node) return 0
  if (node.kind === 'leaf') return 1
  return node.children.reduce((n, c) => n + countLeaves(c), 0)
}

/** Nesting depth; a lone leaf is depth 1. */
export function treeDepth(node: LayoutNode | null): number {
  if (!node) return 0
  if (node.kind === 'leaf') return 1
  return 1 + Math.max(0, ...node.children.map(treeDepth))
}

export function sessionIds(node: LayoutNode | null): string[] {
  return leafEntries(node)
    .map((l) => l.sessionId)
    .filter((id): id is string => id !== null)
}

/** Drop single-child splits and merge a child split into a parent of the
 *  same direction, renormalising ratios. Leaf objects pass through as-is. */
export function collapse(node: LayoutNode): LayoutNode {
  if (node.kind === 'leaf') return node
  const ratios = normRatios(node.ratios, node.children.length)
  const children: LayoutNode[] = []
  const outRatios: number[] = []
  node.children.forEach((c, i) => {
    const cc = collapse(c)
    if (cc.kind === 'split' && cc.dir === node.dir) {
      cc.children.forEach((gc, j) => {
        children.push(gc)
        outRatios.push(ratios[i] * cc.ratios[j])
      })
    } else {
      children.push(cc)
      outRatios.push(ratios[i])
    }
  })
  if (children.length === 1) return children[0]
  return { kind: 'split', dir: node.dir, children, ratios: normRatios(outRatios, children.length) }
}

/** Parse an untrusted (API / storage) layout. Returns null for an empty or
 *  unrecognisable value. */
export function sanitizeLayout(raw: unknown): LayoutNode | null {
  const walk = (v: unknown, depth: number): LayoutNode | null => {
    if (!v || typeof v !== 'object' || depth > MAX_DEPTH) return null
    const o = v as Record<string, unknown>
    if (o.kind === 'leaf') {
      return leaf(typeof o.sessionId === 'string' && o.sessionId ? o.sessionId : null)
    }
    if (o.kind === 'split' && Array.isArray(o.children)) {
      const kids: LayoutNode[] = []
      const ratios: number[] = []
      const rawRatios = Array.isArray(o.ratios) ? o.ratios : []
      o.children.forEach((c, i) => {
        const n = walk(c, depth + 1)
        if (!n) return
        kids.push(n)
        const r = rawRatios[i]
        ratios.push(typeof r === 'number' ? r : NaN)
      })
      if (kids.length === 0) return null
      const dir: SplitDir = o.dir === 'col' ? 'col' : 'row'
      return { kind: 'split', dir, children: kids, ratios: normRatios(ratios, kids.length) }
    }
    return null
  }
  const n = walk(raw, 1)
  return n ? collapse(n) : null
}

export function getNode(root: LayoutNode, path: Path): LayoutNode | null {
  let n: LayoutNode = root
  for (const i of path) {
    if (n.kind !== 'split' || !n.children[i]) return null
    n = n.children[i]
  }
  return n
}

function setNode(root: LayoutNode, path: Path, next: LayoutNode): LayoutNode {
  if (path.length === 0) return next
  if (root.kind !== 'split') return root
  const [i, ...rest] = path
  const children = root.children.slice()
  children[i] = setNode(children[i], rest, next)
  return { ...root, children }
}

/** Flattened geometry: every leaf's rect plus every divider. */
export function layoutGeometry(root: LayoutNode | null): {
  leaves: LeafEntry[]
  dividers: DividerEntry[]
} {
  const leaves: LeafEntry[] = []
  const dividers: DividerEntry[] = []
  const walk = (n: LayoutNode, path: Path, rect: Rect, enterFrom: DropEdge) => {
    if (n.kind === 'leaf') {
      leaves.push({ key: leafKey(n, path), leaf: n, sessionId: n.sessionId, path, rect, enterFrom })
      return
    }
    const ratios = normRatios(n.ratios, n.children.length)
    let off = 0
    n.children.forEach((c, i) => {
      const r = ratios[i]
      const childRect =
        n.dir === 'row'
          ? { x: rect.x + off * rect.w, y: rect.y, w: r * rect.w, h: rect.h }
          : { x: rect.x, y: rect.y + off * rect.h, w: rect.w, h: r * rect.h }
      const from: DropEdge =
        n.dir === 'row' ? (i === 0 ? 'left' : 'right') : i === 0 ? 'top' : 'bottom'
      walk(c, [...path, i], childRect, from)
      off += r
      if (i < n.children.length - 1) {
        dividers.push({
          path,
          index: i,
          dir: n.dir,
          nodeRect: rect,
          pos: n.dir === 'row' ? rect.x + off * rect.w : rect.y + off * rect.h,
        })
      }
    })
  }
  if (root) walk(root, [], { x: 0, y: 0, w: 1, h: 1 }, 'right')
  return { leaves, dividers }
}

export function leafEntries(root: LayoutNode | null): LeafEntry[] {
  return layoutGeometry(root).leaves
}

export function findLeafPath(root: LayoutNode | null, key: string): Path | null {
  return leafEntries(root).find((l) => l.key === key)?.path ?? null
}

function findLeafByRef(root: LayoutNode | null, target: LeafNode): Path | null {
  return leafEntries(root).find((l) => l.leaf === target)?.path ?? null
}

/** Remove the node at `path`, collapsing the tree. Null when nothing is left. */
function removeAt(root: LayoutNode, path: Path): LayoutNode | null {
  if (path.length === 0) return null
  const parentPath = path.slice(0, -1)
  const idx = path[path.length - 1]
  const parent = getNode(root, parentPath)
  if (!parent || parent.kind !== 'split') return root
  const children = parent.children.filter((_, i) => i !== idx)
  const ratios = normRatios(parent.ratios, parent.children.length).filter((_, i) => i !== idx)
  const nextParent: LayoutNode =
    children.length === 0
      ? leaf(null)
      : { ...parent, children, ratios: normRatios(ratios, children.length) }
  if (children.length === 0) return removeAt(root, parentPath)
  return collapse(setNode(root, parentPath, nextParent))
}

export function removeLeaf(root: LayoutNode | null, key: string): LayoutNode | null {
  if (!root) return null
  const path = findLeafPath(root, key)
  return path ? removeAt(root, path) : root
}

/** Put `newLeaf` beside the leaf at `path`, splitting along `dir`. Joins the
 *  parent split when it already runs that way (no needless nesting). */
function splitLeafAt(
  root: LayoutNode,
  path: Path,
  dir: SplitDir,
  newLeaf: LeafNode,
  before: boolean,
): LayoutNode {
  const target = getNode(root, path)
  if (!target) return root
  const parentPath = path.slice(0, -1)
  const parent = path.length > 0 ? getNode(root, parentPath) : null
  const joinParent = (p: SplitNode) => {
    const idx = path[path.length - 1]
    const ratios = normRatios(p.ratios, p.children.length)
    const half = ratios[idx] / 2
    const children = p.children.slice()
    const nextRatios = ratios.slice()
    nextRatios[idx] = half
    const at = before ? idx : idx + 1
    children.splice(at, 0, newLeaf)
    nextRatios.splice(at, 0, half)
    return collapse(setNode(root, parentPath, { ...p, children, ratios: nextRatios }))
  }
  if (parent && parent.kind === 'split' && parent.dir === dir) return joinParent(parent)
  const nested: LayoutNode = {
    kind: 'split',
    dir,
    children: before ? [newLeaf, target] : [target, newLeaf],
    ratios: [0.5, 0.5],
  }
  const next = collapse(setNode(root, path, nested))
  if (treeDepth(next) <= MAX_DEPTH) return next
  // Too deep: join the parent split whatever its direction.
  if (parent && parent.kind === 'split') return joinParent(parent)
  return root
}

/** tmux-style auto tiling: split the largest leaf along its longer axis.
 *  `aspect` is the container's width / height. */
export function insertAuto(
  root: LayoutNode | null,
  sessionId: string | null,
  aspect = 16 / 9,
): LayoutNode {
  const newLeaf = leaf(sessionId)
  if (!root) return newLeaf
  if (countLeaves(root) >= MAX_LEAVES) return root
  const entries = leafEntries(root)
  let best = entries[0]
  let bestArea = -1
  for (const e of entries) {
    const area = e.rect.w * aspect * e.rect.h
    if (area > bestArea + 1e-9) {
      best = e
      bestArea = area
    }
  }
  const dir: SplitDir = best.rect.w * aspect >= best.rect.h ? 'row' : 'col'
  return splitLeafAt(root, best.path, dir, newLeaf, false)
}

export function swapLeaves(root: LayoutNode, keyA: string, keyB: string): LayoutNode {
  const a = findLeafPath(root, keyA)
  const b = findLeafPath(root, keyB)
  if (!a || !b || keyA === keyB) return root
  const na = getNode(root, a)!
  const nb = getNode(root, b)!
  return setNode(setNode(root, a, nb), b, na)
}

/** Drag-and-drop re-split: move leaf `fromKey` to the `edge` side of `toKey`. */
export function moveLeaf(
  root: LayoutNode,
  fromKey: string,
  toKey: string,
  edge: DropEdge,
): LayoutNode {
  if (fromKey === toKey) return root
  const fromPath = findLeafPath(root, fromKey)
  const toPath = findLeafPath(root, toKey)
  if (!fromPath || !toPath) return root
  const moving = getNode(root, fromPath) as LeafNode
  const target = getNode(root, toPath) as LeafNode
  const without = removeAt(root, fromPath)
  if (!without) return root
  const targetPath = findLeafByRef(without, target)
  if (!targetPath) return root
  const dir: SplitDir = edge === 'left' || edge === 'right' ? 'row' : 'col'
  return splitLeafAt(without, targetPath, dir, moving, edge === 'left' || edge === 'top')
}

/** Shift the boundary between children `index` / `index + 1` by `delta`
 *  (node-relative fraction), keeping both at least `minFrac`. */
export function resizePair(
  ratios: number[],
  index: number,
  delta: number,
  minFrac: number,
): number[] {
  const r = normRatios(ratios, ratios.length)
  const pair = r[index] + r[index + 1]
  const min = Math.min(minFrac, pair / 2)
  const a = Math.min(pair - min, Math.max(min, r[index] + delta))
  const out = r.slice()
  out[index] = a
  out[index + 1] = pair - a
  return out
}

export function setRatiosAt(root: LayoutNode, path: Path, ratios: number[]): LayoutNode {
  const node = getNode(root, path)
  if (!node || node.kind !== 'split') return root
  return setNode(root, path, { ...node, ratios: normRatios(ratios, node.children.length) })
}

export function replaceLeafSession(
  root: LayoutNode,
  key: string,
  sessionId: string | null,
): LayoutNode {
  const path = findLeafPath(root, key)
  return path ? setNode(root, path, leaf(sessionId)) : root
}

/** Blank every leaf showing `sessionId` (the session was deleted). */
export function clearSession(root: LayoutNode | null, sessionId: string): LayoutNode | null {
  if (!root) return null
  if (root.kind === 'leaf') return root.sessionId === sessionId ? leaf(null) : root
  let changed = false
  const children = root.children.map((c) => {
    const n = clearSession(c, sessionId)!
    if (n !== c) changed = true
    return n
  })
  return changed ? { ...root, children } : root
}

/** Keep only leaves in `desired` (empty leaves are dropped too) and append
 *  missing ids in order via auto tiling. */
export function reconcile(
  root: LayoutNode | null,
  desired: string[],
  aspect: number,
): LayoutNode | null {
  const want = new Set(desired)
  let next = root
  for (const e of leafEntries(root)) {
    if (e.sessionId === null || !want.has(e.sessionId)) next = removeLeaf(next, e.key)
  }
  const have = new Set(sessionIds(next))
  for (const id of desired) {
    if (!have.has(id)) next = insertAuto(next, id, aspect)
  }
  return next
}

function evenSplit(dir: SplitDir, children: LayoutNode[]): LayoutNode {
  if (children.length === 1) return children[0]
  return { kind: 'split', dir, children, ratios: equalRatios(children.length) }
}

/** Initial layout for a new view. */
export function starterLayout(kind: StarterLayout, ids: (string | null)[]): LayoutNode | null {
  const leaves = ids.slice(0, MAX_LEAVES).map(leaf)
  if (leaves.length === 0) return null
  if (leaves.length === 1) return leaves[0]
  switch (kind) {
    case 'columns':
      return evenSplit('row', leaves)
    case 'rows':
      return evenSplit('col', leaves)
    case 'grid': {
      const cols = Math.ceil(Math.sqrt(leaves.length))
      const rows: LayoutNode[] = []
      for (let i = 0; i < leaves.length; i += cols) {
        rows.push(evenSplit('row', leaves.slice(i, i + cols)))
      }
      return collapse(evenSplit('col', rows))
    }
    case 'main-stack': {
      const [main, ...rest] = leaves
      return {
        kind: 'split',
        dir: 'row',
        children: [main, evenSplit('col', rest)],
        ratios: [0.6, 0.4],
      }
    }
  }
}
