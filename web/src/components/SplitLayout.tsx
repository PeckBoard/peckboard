import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type DragEvent,
  type KeyboardEvent,
  type PointerEvent,
  type ReactNode,
} from 'react'
import { MenuButton, type MenuItem } from './Dropdown'
import { getStatusDotClass, getStatusLabel, type AgentStatus } from './chat/events'
import { useReducedMotion, prefersReducedMotion } from '../hooks/useReducedMotion'
import {
  getNode,
  layoutGeometry,
  moveLeaf,
  removeLeaf,
  resizePair,
  setRatiosAt,
  swapLeaves,
  type DividerEntry,
  type DropEdge,
  type LayoutNode,
  type LeafEntry,
  type Rect,
} from '../lib/layoutTree'

/** Per-pane header content, supplied by the owning view. */
export interface PaneInfo {
  title: string
  /** Drives the header's status dot; omit for no dot. */
  status?: AgentStatus | null
  /** Live status element (dot / badge) that subscribes to its own data —
   *  replaces `status` + `badge` when set, so a streaming pane doesn't
   *  re-render the whole layout per token. */
  statusSlot?: ReactNode
  /** Small trailing chip, e.g. "Done". */
  badge?: string
  /** Owner-specific items (the canonical session menu); the layout appends
   *  Maximize / Open as tab / Close pane. */
  menuItems?: MenuItem[]
  /** Offer "Open as tab" (calls `onOpenAsTab`). */
  canOpenAsTab?: boolean
  /** Hide "Close pane" (e.g. the primary pane). Defaults to closable. */
  closable?: boolean
}

export interface PaneContext {
  focused: boolean
  /** True whenever the pane shows chrome (more than one pane on screen). */
  compact: boolean
}

interface SplitLayoutProps {
  layout: LayoutNode | null
  onChange: (next: LayoutNode | null) => void
  getPaneInfo: (entry: LeafEntry) => PaneInfo
  renderPane: (entry: LeafEntry, ctx: PaneContext) => ReactNode
  onOpenAsTab?: (entry: LeafEntry) => void
  /** Override the default close (remove the leaf and collapse the tree). */
  onClosePane?: (entry: LeafEntry) => void
  /** Drag a pane header onto another pane to re-split / swap. */
  rearrangeable?: boolean
  /** A lone pane renders bare (no header, no border) — the normal session
   *  view looks exactly like a plain ChatView until a second pane arrives. */
  bareWhenSingle?: boolean
  onFocusChange?: (key: string) => void
  emptyState?: ReactNode
  testId?: string
}

/** Smallest pane edge a divider drag may leave, in px. */
const MIN_PANE_PX = 180
/** Below this container width the split collapses to a pane switcher. */
const NARROW_PX = 900
/** Keep in sync with `--split-anim` in split-layout.css. */
const ANIM_MS = 250
const KEY_STEP = 0.02
const PANE_DRAG_TYPE = 'application/x-peckboard-pane'

function collapsedRect(r: Rect, from: DropEdge): Rect {
  switch (from) {
    case 'left':
      return { ...r, w: 0 }
    case 'right':
      return { ...r, x: r.x + r.w, w: 0 }
    case 'top':
      return { ...r, h: 0 }
    case 'bottom':
      return { ...r, y: r.y + r.h, h: 0 }
  }
}

function rectStyle(r: Rect): CSSProperties {
  return {
    left: `${r.x * 100}%`,
    top: `${r.y * 100}%`,
    width: `${r.w * 100}%`,
    height: `${r.h * 100}%`,
  }
}

const ZONES: (DropEdge | 'center')[] = ['left', 'right', 'top', 'bottom', 'center']

/**
 * Shared multi-pane engine: renders a `LayoutNode` tree as tiled panes with
 * draggable dividers. Panes render FLAT (absolutely positioned siblings keyed
 * by leaf key), so re-splitting the tree never remounts a pane's content —
 * a ChatView keeps its scroll position, draft and socket subscription while
 * panes come and go around it. Used by the session view's auto subagent
 * panes and by saved multi-session Views.
 */
export default function SplitLayout({
  layout,
  onChange,
  getPaneInfo,
  renderPane,
  onOpenAsTab,
  onClosePane,
  rearrangeable = false,
  bareWhenSingle = false,
  onFocusChange,
  emptyState,
  testId,
}: SplitLayoutProps) {
  const reduceMotion = useReducedMotion()
  const stageRef = useRef<HTMLDivElement | null>(null)
  const [size, setSize] = useState({ w: 0, h: 0 })
  useLayoutEffect(() => {
    const el = stageRef.current
    if (!el) return
    const ro = new ResizeObserver(() => setSize({ w: el.clientWidth, h: el.clientHeight }))
    ro.observe(el)
    return () => ro.disconnect()
  }, [])
  const narrow = size.w > 0 && size.w < NARROW_PX

  // Latest layout for deferred work (the close animation's timer).
  const layoutRef = useRef(layout)
  useLayoutEffect(() => {
    layoutRef.current = layout
  })

  const [closing, setClosing] = useState<ReadonlySet<string>>(() => new Set())
  const fullGeom = useMemo(() => layoutGeometry(layout), [layout])
  // Everyone else lays out as if the closing panes were already gone, so
  // the neighbours grow into the space while the closing pane shrinks.
  const liveTree = useMemo(() => {
    let t = layout
    for (const k of closing) t = removeLeaf(t, k)
    return t
  }, [layout, closing])
  const geom = useMemo(() => layoutGeometry(liveTree), [liveTree])
  const keysSig = geom.leaves.map((l) => l.key).join('|')

  // Keys already painted at full size. A key missing from this set is new:
  // it renders collapsed at its split edge for one frame, then transitions
  // open once `seen` catches up.
  const [seen, setSeen] = useState<ReadonlySet<string>>(
    () => new Set(layoutGeometry(layout).leaves.map((l) => l.key)),
  )
  useEffect(() => {
    let raf2 = 0
    const raf1 = requestAnimationFrame(() => {
      raf2 = requestAnimationFrame(() => setSeen(new Set(keysSig ? keysSig.split('|') : [])))
    })
    return () => {
      cancelAnimationFrame(raf1)
      cancelAnimationFrame(raf2)
    }
  }, [keysSig])

  const [focusedRaw, setFocusedRaw] = useState<string | null>(null)
  const focusedKey =
    focusedRaw && geom.leaves.some((l) => l.key === focusedRaw)
      ? focusedRaw
      : (geom.leaves[0]?.key ?? null)
  const focus = useCallback(
    (key: string) => {
      setFocusedRaw(key)
      onFocusChange?.(key)
    },
    [onFocusChange],
  )
  const [maximizedRaw, setMaximizedRaw] = useState<string | null>(null)
  const maximizedKey =
    maximizedRaw && geom.leaves.some((l) => l.key === maximizedRaw) ? maximizedRaw : null

  const single = geom.leaves.length <= 1 && closing.size === 0
  const bare = bareWhenSingle && single
  // One pane on screen at a time: maximized, or the narrow-width switcher.
  const soloKey = maximizedKey ?? (narrow && !single ? focusedKey : null)

  const closePane = (entry: LeafEntry) => {
    const finish = () => {
      if (onClosePane) onClosePane(entry)
      else onChange(removeLeaf(layoutRef.current, entry.key))
    }
    if (prefersReducedMotion() || soloKey !== null) {
      finish()
      return
    }
    setClosing((s) => new Set(s).add(entry.key))
    window.setTimeout(() => {
      finish()
      setClosing((s) => {
        const n = new Set(s)
        n.delete(entry.key)
        return n
      })
    }, ANIM_MS)
  }

  // ── Divider drag / keyboard resize ──
  const dragRef = useRef<{
    d: DividerEntry
    start: number
    ratios: number[]
    extentPx: number
  } | null>(null)
  const [resizing, setResizing] = useState(false)
  const extentOf = (d: DividerEntry) =>
    d.dir === 'row' ? d.nodeRect.w * size.w : d.nodeRect.h * size.h
  const ratiosOf = (d: DividerEntry): number[] | null => {
    if (!layout) return null
    const node = getNode(layout, d.path)
    return node && node.kind === 'split' ? node.ratios : null
  }
  const onDividerDown = (e: PointerEvent<HTMLDivElement>, d: DividerEntry) => {
    const ratios = ratiosOf(d)
    if (!ratios || e.button !== 0) return
    e.preventDefault()
    e.currentTarget.setPointerCapture(e.pointerId)
    dragRef.current = {
      d,
      start: d.dir === 'row' ? e.clientX : e.clientY,
      ratios,
      extentPx: Math.max(1, extentOf(d)),
    }
    setResizing(true)
  }
  const onDividerMove = (e: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current
    if (!drag || !layout) return
    const pos = drag.d.dir === 'row' ? e.clientX : e.clientY
    const delta = (pos - drag.start) / drag.extentPx
    const next = resizePair(drag.ratios, drag.d.index, delta, MIN_PANE_PX / drag.extentPx)
    onChange(setRatiosAt(layout, drag.d.path, next))
  }
  const onDividerUp = (e: PointerEvent<HTMLDivElement>) => {
    if (!dragRef.current) return
    dragRef.current = null
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId)
    }
    setResizing(false)
  }
  const onDividerKey = (e: KeyboardEvent<HTMLDivElement>, d: DividerEntry) => {
    const back = d.dir === 'row' ? 'ArrowLeft' : 'ArrowUp'
    const fwd = d.dir === 'row' ? 'ArrowRight' : 'ArrowDown'
    if (e.key !== back && e.key !== fwd) return
    const ratios = ratiosOf(d)
    if (!ratios || !layout) return
    e.preventDefault()
    const extent = Math.max(1, extentOf(d))
    const next = resizePair(
      ratios,
      d.index,
      e.key === fwd ? KEY_STEP : -KEY_STEP,
      MIN_PANE_PX / extent,
    )
    onChange(setRatiosAt(layout, d.path, next))
  }
  const dividerValue = (d: DividerEntry): number => {
    const ratios = ratiosOf(d) ?? []
    const sum = ratios.slice(0, d.index + 1).reduce((a, b) => a + b, 0)
    return Math.round(sum * 100)
  }

  // ── Pane drag & drop (rearrange) ──
  const [dragKey, setDragKey] = useState<string | null>(null)
  const [hoverZone, setHoverZone] = useState<{ key: string; zone: DropEdge | 'center' } | null>(
    null,
  )
  const endPaneDrag = () => {
    setDragKey(null)
    setHoverZone(null)
  }
  const onZoneDrop = (e: DragEvent, target: LeafEntry, zone: DropEdge | 'center') => {
    e.preventDefault()
    const from = e.dataTransfer.getData(PANE_DRAG_TYPE) || dragKey
    endPaneDrag()
    if (!layout || !from || from === target.key) return
    onChange(
      zone === 'center'
        ? swapLeaves(layout, from, target.key)
        : moveLeaf(layout, from, target.key, zone),
    )
  }

  const paneMenu = (entry: LeafEntry, info: PaneInfo): MenuItem[] => {
    const own = info.menuItems ?? []
    const isMax = maximizedKey === entry.key
    return [
      ...own,
      ...(own.length > 0 ? [{ divider: true }] : []),
      {
        label: isMax ? 'Restore' : 'Maximize',
        onSelect: () => setMaximizedRaw(isMax ? null : entry.key),
        hidden: single,
        testId: 'split-pane-maximize',
      },
      {
        label: 'Open as tab',
        onSelect: () => onOpenAsTab?.(entry),
        hidden: !onOpenAsTab || !info.canOpenAsTab,
        testId: 'split-pane-open-tab',
      },
      {
        label: 'Close pane',
        onSelect: () => closePane(entry),
        hidden: info.closable === false,
        testId: 'split-pane-close-item',
      },
    ]
  }

  if (!layout) {
    return (
      <div className="split-layout split-layout-empty" data-testid={testId}>
        <div className="split-stage" ref={stageRef}>
          {emptyState}
        </div>
      </div>
    )
  }

  const closingEntries = fullGeom.leaves.filter((l) => closing.has(l.key))
  const rendered: { entry: LeafEntry; rect: Rect; state: 'live' | 'entering' | 'closing' }[] = [
    ...geom.leaves.map((entry) => {
      const entering = !reduceMotion && !seen.has(entry.key) && seen.size > 0
      return {
        entry,
        rect: entering ? collapsedRect(entry.rect, entry.enterFrom) : entry.rect,
        state: (entering ? 'entering' : 'live') as 'live' | 'entering',
      }
    }),
    ...closingEntries.map((entry) => ({
      entry,
      rect: collapsedRect(entry.rect, entry.enterFrom),
      state: 'closing' as const,
    })),
  ]
  // Stable DOM order (by key) so a re-split never reorders siblings.
  rendered.sort((a, b) => (a.entry.key < b.entry.key ? -1 : a.entry.key > b.entry.key ? 1 : 0))

  return (
    <div
      className={`split-layout${resizing ? ' split-resizing' : ''}${bare ? ' split-bare' : ''}${
        narrow ? ' split-narrow' : ''
      }`}
      data-testid={testId}
    >
      {narrow && !single && !maximizedKey && (
        <div className="split-switcher" role="tablist" aria-label="Panes">
          {geom.leaves.map((entry) => {
            const info = getPaneInfo(entry)
            const active = entry.key === focusedKey
            return (
              <button
                key={entry.key}
                type="button"
                role="tab"
                aria-selected={active}
                className={`split-switcher-tab${active ? ' active' : ''}`}
                data-testid="split-switcher-tab"
                data-pane-id={entry.key}
                onClick={() => focus(entry.key)}
              >
                {info.status && (
                  <span className={getStatusDotClass(info.status)} aria-hidden="true" />
                )}
                <span className="split-switcher-label">{info.title}</span>
              </button>
            )
          })}
        </div>
      )}
      <div className="split-stage" ref={stageRef}>
        {rendered.map(({ entry, rect, state }) => {
          const info = getPaneInfo(entry)
          const focused = entry.key === focusedKey && !single
          const hidden = soloKey !== null && entry.key !== soloKey
          const style =
            soloKey === entry.key ? rectStyle({ x: 0, y: 0, w: 1, h: 1 }) : rectStyle(rect)
          const showDrop = rearrangeable && dragKey !== null && dragKey !== entry.key && !hidden
          return (
            <div
              key={entry.key}
              className={`split-pane split-pane-${state} split-from-${entry.enterFrom}${
                focused ? ' focused' : ''
              }${hidden ? ' split-pane-hidden' : ''}${bare ? ' split-pane-bare' : ''}`}
              style={style}
              data-testid="split-pane"
              data-pane-id={entry.key}
              data-focused={focused || undefined}
              aria-hidden={hidden || undefined}
              onPointerDownCapture={() => {
                if (entry.key !== focusedKey) focus(entry.key)
              }}
              onFocusCapture={() => {
                if (entry.key !== focusedKey) focus(entry.key)
              }}
            >
              <div className="split-pane-frame">
                {!bare && (
                  <div
                    className="split-pane-header"
                    draggable={rearrangeable && !narrow}
                    data-testid="split-pane-header"
                    onDragStart={(e) => {
                      e.dataTransfer.setData(PANE_DRAG_TYPE, entry.key)
                      e.dataTransfer.effectAllowed = 'move'
                      // Mounting the drop overlays inside dragstart can abort
                      // the drag in some engines; defer a tick.
                      window.setTimeout(() => setDragKey(entry.key), 0)
                    }}
                    onDragEnd={endPaneDrag}
                  >
                    {!info.statusSlot && info.status && (
                      <span
                        className={getStatusDotClass(info.status)}
                        role="img"
                        aria-label={getStatusLabel(info.status)}
                        data-testid="split-pane-status"
                      />
                    )}
                    <h2 className="split-pane-title" title={info.title}>
                      {info.title}
                    </h2>
                    {info.statusSlot}
                    {!info.statusSlot && info.badge && (
                      <span className="split-pane-badge" data-testid="split-pane-badge">
                        {info.badge}
                      </span>
                    )}
                    <span className="split-pane-spacer" />
                    <MenuButton
                      items={paneMenu(entry, info)}
                      ariaLabel="Pane menu"
                      triggerClassName="split-pane-btn"
                      testId="split-pane-menu"
                    />
                    {!single && maximizedKey === entry.key && (
                      <button
                        type="button"
                        className="split-pane-btn"
                        aria-label="Restore pane"
                        title="Restore"
                        data-testid="split-pane-restore"
                        onClick={() => setMaximizedRaw(null)}
                      >
                        ⤡
                      </button>
                    )}
                    {info.closable !== false && (
                      <button
                        type="button"
                        className="split-pane-btn"
                        aria-label="Close pane"
                        title="Close pane"
                        data-testid="split-pane-close"
                        onClick={() => closePane(entry)}
                      >
                        ×
                      </button>
                    )}
                  </div>
                )}
                <div className="split-pane-body">
                  {renderPane(entry, { focused: entry.key === focusedKey, compact: !bare })}
                </div>
                {showDrop && (
                  <div className="split-drop-overlay" data-testid="split-drop-overlay">
                    {ZONES.map((zone) => (
                      <div
                        key={zone}
                        className={`split-drop-zone split-drop-zone-${zone}${
                          hoverZone?.key === entry.key && hoverZone.zone === zone ? ' active' : ''
                        }`}
                        data-testid={`split-drop-zone-${zone}`}
                        onDragOver={(e) => {
                          e.preventDefault()
                          e.dataTransfer.dropEffect = 'move'
                          if (hoverZone?.key !== entry.key || hoverZone.zone !== zone) {
                            setHoverZone({ key: entry.key, zone })
                          }
                        }}
                        onDrop={(e) => onZoneDrop(e, entry, zone)}
                      />
                    ))}
                  </div>
                )}
              </div>
            </div>
          )
        })}
        {!soloKey &&
          geom.dividers.map((d) => {
            const style: CSSProperties =
              d.dir === 'row'
                ? {
                    left: `${d.pos * 100}%`,
                    top: `${d.nodeRect.y * 100}%`,
                    height: `${d.nodeRect.h * 100}%`,
                  }
                : {
                    top: `${d.pos * 100}%`,
                    left: `${d.nodeRect.x * 100}%`,
                    width: `${d.nodeRect.w * 100}%`,
                  }
            return (
              <div
                key={`${d.path.join('.')}:${d.index}`}
                className={`split-divider split-divider-${d.dir}`}
                style={style}
                role="separator"
                tabIndex={0}
                aria-orientation={d.dir === 'row' ? 'vertical' : 'horizontal'}
                aria-label="Resize panes"
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={dividerValue(d)}
                data-testid="split-divider"
                data-dir={d.dir}
                onPointerDown={(e) => onDividerDown(e, d)}
                onPointerMove={onDividerMove}
                onPointerUp={onDividerUp}
                onPointerCancel={onDividerUp}
                onKeyDown={(e) => onDividerKey(e, d)}
              />
            )
          })}
      </div>
    </div>
  )
}
