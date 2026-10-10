import { useMemo, type ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import type { InfoWidgetProps } from './types'
import { humanize, useDashboardData } from './useDashboardData'
import { DashEmpty, DashError, DashHeading, DashLoading, DashNoMatch } from './DashParts'
import { FilterBar, FilterButton, useWidgetFilters, type FilterDef } from './filters'

interface DepNode {
  card_id: string
  title: string
  step: string
  blocked: boolean
  done: boolean
}

interface Deps {
  nodes: DepNode[]
  edges: { card_id: string; depends_on: string }[]
  top_blockers: { card_id: string; title: string; blocks: number }[]
}

const NODE_W = 150
const NODE_H = 24
const COL_GAP = 44
const ROW_GAP = 8
const PAD = 4

function clip(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s
}

/** Layered layout: a card's column is the length of its longest
 *  dependency chain, so prerequisites sit left of what waits on them.
 *  Rows within a column follow the mean row of their dependencies
 *  (one barycenter pass) to cut edge crossings. Cycles are cut. */
function layout(data: Deps) {
  const ids = new Set(data.nodes.map((n) => n.card_id))
  const deps = new Map<string, string[]>()
  for (const e of data.edges) {
    if (!ids.has(e.card_id) || !ids.has(e.depends_on)) continue
    const list = deps.get(e.card_id) ?? []
    list.push(e.depends_on)
    deps.set(e.card_id, list)
  }
  const depth = new Map<string, number>()
  const visiting = new Set<string>()
  const depthOf = (id: string): number => {
    const known = depth.get(id)
    if (known !== undefined) return known
    if (visiting.has(id)) return 0
    visiting.add(id)
    let d = 0
    for (const p of deps.get(id) ?? []) d = Math.max(d, depthOf(p) + 1)
    visiting.delete(id)
    depth.set(id, d)
    return d
  }
  const cols: DepNode[][] = []
  for (const n of data.nodes) {
    const d = depthOf(n.card_id)
    ;(cols[d] ??= []).push(n)
  }
  const row = new Map<string, number>()
  cols.forEach((col, ci) => {
    if (ci > 0) {
      const key = (n: DepNode) => {
        const ps = (deps.get(n.card_id) ?? []).map((p) => row.get(p) ?? 0)
        return ps.length ? ps.reduce((a, b) => a + b, 0) / ps.length : 0
      }
      col.sort((a, b) => key(a) - key(b))
    }
    col.forEach((n, ri) => row.set(n.card_id, ri))
  })
  const rows = Math.max(1, ...cols.map((c) => c?.length ?? 0))
  const pos = new Map<string, { x: number; y: number }>()
  cols.forEach((col, ci) =>
    col?.forEach((n, ri) => {
      // Centre short columns vertically against the tallest one.
      const offset = ((rows - col.length) * (NODE_H + ROW_GAP)) / 2
      pos.set(n.card_id, {
        x: PAD + ci * (NODE_W + COL_GAP),
        y: PAD + offset + ri * (NODE_H + ROW_GAP),
      })
    }),
  )
  const width = PAD * 2 + cols.length * NODE_W + Math.max(0, cols.length - 1) * COL_GAP
  const height = PAD * 2 + rows * NODE_H + (rows - 1) * ROW_GAP
  return { pos, width, height, deps }
}

function DepGraph({ data, onOpen }: { data: Deps; onOpen: () => void }) {
  const { pos, width, height, deps } = useMemo(() => layout(data), [data])
  const blockedIds = new Set(data.nodes.filter((n) => n.blocked).map((n) => n.card_id))
  return (
    <svg
      className="dash-dag"
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="xMidYMin meet"
      // Never scale past 1:1, so labels stay list-sized on a roomy widget.
      style={{ maxWidth: width, maxHeight: height }}
      role="img"
      aria-label={`Dependency graph of ${data.nodes.length} cards`}
    >
      <g className="dash-dag-edges">
        {[...deps.entries()].flatMap(([id, ps]) =>
          ps.map((p) => {
            const a = pos.get(p)
            const b = pos.get(id)
            if (!a || !b) return null
            const x1 = a.x + NODE_W
            const y1 = a.y + NODE_H / 2
            const x2 = b.x
            const y2 = b.y + NODE_H / 2
            const mx = (x1 + x2) / 2
            return (
              <path
                key={`${p}->${id}`}
                d={`M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}`}
                className={blockedIds.has(id) ? 'dash-dag-edge is-blocked' : 'dash-dag-edge'}
              />
            )
          }),
        )}
      </g>
      {data.nodes.map((n) => {
        const p = pos.get(n.card_id)
        if (!p) return null
        const state = n.done ? 'is-done' : n.blocked ? 'is-blocked' : ''
        return (
          <g
            key={n.card_id}
            className={`dash-dag-node ${state}`}
            transform={`translate(${p.x},${p.y})`}
            role="button"
            tabIndex={0}
            data-card-id={n.card_id}
            onClick={onOpen}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault()
                onOpen()
              }
            }}
          >
            <title>{`${n.title}\n${humanize(n.step)}${n.blocked ? ' · blocked' : ''}${n.done ? ' · done' : ''}`}</title>
            <rect width={NODE_W} height={NODE_H} rx={4} />
            <text x={8} y={NODE_H / 2} dominantBaseline="central">
              {clip(n.title, 22)}
            </text>
          </g>
        )
      })}
    </svg>
  )
}

const FILTER_DEFS: FilterDef[] = [
  { key: 'hide_done', kind: 'toggle', label: 'Hide done' },
  { key: 'blocked', kind: 'toggle', label: 'Blocked only' },
]

/** Applies Hide done / Blocked only: hidden nodes take their edges with
 *  them. A blocker is rarely blocked itself, so under Blocked only the top
 *  blockers keep the cards a shown card waits on. */
function filterDeps(data: Deps, hideDone: boolean, blockedOnly: boolean): Deps {
  if (!hideDone && !blockedOnly) return data
  const nodes = data.nodes.filter((n) => !(hideDone && n.done) && !(blockedOnly && !n.blocked))
  const ids = new Set(nodes.map((n) => n.card_id))
  const edges = data.edges.filter((e) => ids.has(e.card_id) && ids.has(e.depends_on))
  const done = new Set(data.nodes.filter((n) => n.done).map((n) => n.card_id))
  const holding = new Set(data.edges.filter((e) => ids.has(e.card_id)).map((e) => e.depends_on))
  const top_blockers = data.top_blockers.filter(
    (b) =>
      !(hideDone && done.has(b.card_id)) &&
      (blockedOnly ? holding.has(b.card_id) : ids.has(b.card_id)),
  )
  return { nodes, edges, top_blockers }
}

/** Card Dependencies: the cards holding up the most work, and the project's
 *  dependency graph (optionally rooted at one card). */
export default function DependenciesWidget(props: InfoWidgetProps) {
  const { widget, ctx, menuItems, scopeProjectId, scopeName, onOpenProject } = props
  const path = scopeProjectId
    ? `/api/dashboard/dependencies${widget.cardId ? `?card_id=${encodeURIComponent(widget.cardId)}` : ''}`
    : null
  const { data, error, reload } = useDashboardData<Deps>(path, scopeProjectId, [
    'card-update',
    'card-delete',
  ])
  const openBoard = () => {
    if (scopeProjectId) onOpenProject(scopeProjectId)
  }
  const {
    filters,
    set,
    clear,
    activeCount,
    open: filterOpen,
    toggle: toggleFilters,
  } = useWidgetFilters(props)
  const hideDone = filters.hide_done === true
  const blockedOnly = filters.blocked === true
  // Memoized so the graph's layout only reruns when the data or filters change.
  const shown = useMemo(
    () => (data ? filterDeps(data, hideDone, blockedOnly) : null),
    [data, hideDone, blockedOnly],
  )

  let body: ReactNode
  if (!scopeProjectId) {
    body = (
      <DashEmpty testId="dash-dependencies-empty">
        Pick a project with <strong>Configure…</strong> in the widget menu.
      </DashEmpty>
    )
  } else if (!data) {
    body = error ? <DashError message={error} onRetry={() => void reload()} /> : <DashLoading />
  } else if (data.edges.length === 0) {
    body = <DashEmpty testId="dash-dependencies-empty">No card dependencies</DashEmpty>
  } else if (!shown || shown.nodes.length === 0) {
    body = <DashNoMatch onClear={clear} />
  } else {
    body = (
      <div className="dash-deps" data-testid="dash-dependencies-list">
        {shown.top_blockers.length > 0 && (
          <section className="dash-group dash-deps-blockers">
            <DashHeading>Top blockers</DashHeading>
            <ul className="dash-list">
              {shown.top_blockers.map((b) => (
                <li key={b.card_id} data-card-id={b.card_id}>
                  <button type="button" className="dash-row" onClick={openBoard} title={b.title}>
                    <span className="dash-row-main">
                      <span className="dash-row-title">{b.title}</span>
                    </span>
                    <span className="dash-blocks" title={`${b.blocks} cards waiting on this`}>
                      blocks {b.blocks}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}
        <div className="dash-dag-wrap">
          <DepGraph data={shown} onOpen={openBoard} />
        </div>
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="dependencies"
      widgetId={widget.id}
      title={scopeName ? `Dependencies · ${scopeName}` : 'Card Dependencies'}
      statusSlot={
        scopeProjectId && data ? (
          <FilterButton activeCount={activeCount} open={filterOpen} onToggle={toggleFilters} />
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {filterOpen && scopeProjectId && data && (
        <FilterBar defs={FILTER_DEFS} filters={filters} set={set} clear={clear} />
      )}
      {body}
    </WidgetFrame>
  )
}
