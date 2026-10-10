import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type PointerEvent,
  type ReactNode,
} from 'react'
import {
  GRID_COLS,
  MAX_H,
  MIN_H,
  compact,
  gridHeight,
  moveItem,
  nudgeItem,
  resizeItem,
  sameLayout,
  type GridItem,
  type GridRect,
} from '../../lib/widgetGrid'

export const ROW_HEIGHT = 40
export const GRID_GAP = 8
/** Below this width the grid collapses to a single, non-draggable column. */
export const NARROW_WIDTH = 768
/** Pointer travel before a press on a header turns into a drag, so plain
 *  clicks (title links, focusing the widget) still work. */
const DRAG_THRESHOLD = 4

/** Spread onto the element that drags the widget (its header). */
export interface DragHandleProps {
  onPointerDown: (e: PointerEvent<HTMLElement>) => void
}

/** Spread onto the focusable grip for keyboard moves / resizes. */
export interface GripProps {
  onKeyDown: (e: KeyboardEvent<HTMLElement>) => void
}

export interface WidgetContext {
  /** Last widget the user pressed or focused — drives keyboard ownership. */
  focused: boolean
  /** Single-column phone layout: no drag, no resize. */
  narrow: boolean
  /** This widget is being dragged or resized. */
  active: boolean
  dragHandle: DragHandleProps | undefined
  grip: GripProps | undefined
}

interface Props<T extends GridItem> {
  items: T[]
  /** Called once per finished drag / resize / keyboard step with the new,
   *  compacted layout. */
  onChange: (next: T[]) => void
  renderWidget: (item: T, ctx: WidgetContext) => ReactNode
  emptyState?: ReactNode
  testId?: string
}

interface Drag<T extends GridItem> {
  id: string
  mode: 'move' | 'resize'
  pointerId: number
  startX: number
  startY: number
  started: boolean
  orig: T[]
  preview: T[]
  /** Live pixel offset of the dragged widget from its original rect. */
  dx: number
  dy: number
}

interface Metrics {
  colW: number
  rowH: number
  gap: number
}

function toPx(
  r: GridRect,
  m: Metrics,
): { left: number; top: number; width: number; height: number } {
  return {
    left: r.x * (m.colW + m.gap),
    top: r.y * (m.rowH + m.gap),
    width: r.w * m.colW + (r.w - 1) * m.gap,
    height: r.h * m.rowH + (r.h - 1) * m.gap,
  }
}

/**
 * Datadog-style widget dashboard: absolutely positioned cells on a
 * 12-column grid. Drag a widget by its header, resize it from the
 * bottom-right handle; a ghost shows where it will land and neighbours
 * reflow live (push-down + gravity, see `lib/widgetGrid`). Layout changes
 * are reported once, on drop. Under `NARROW_WIDTH` it is a plain stack.
 */
export default function WidgetGrid<T extends GridItem>({
  items,
  onChange,
  renderWidget,
  emptyState,
  testId,
}: Props<T>) {
  const rootRef = useRef<HTMLDivElement | null>(null)
  const [width, setWidth] = useState(0)
  const [focusedId, setFocusedId] = useState<string | null>(null)
  const [drag, setDrag] = useState<Drag<T> | null>(null)

  useLayoutEffect(() => {
    const el = rootRef.current
    if (!el) return
    setWidth(el.clientWidth)
    const ro = new ResizeObserver(() => setWidth(el.clientWidth))
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  const narrow = width > 0 && width < NARROW_WIDTH
  const m: Metrics = {
    colW: Math.max(0, (width - GRID_GAP * (GRID_COLS - 1)) / GRID_COLS),
    rowH: ROW_HEIGHT,
    gap: GRID_GAP,
  }
  const mRef = useRef(m)
  useEffect(() => {
    mRef.current = m
  })
  const onChangeRef = useRef(onChange)
  useEffect(() => {
    onChangeRef.current = onChange
  })

  // Window-level listeners for the life of a press: the pointer may leave
  // the header (and cross xterm canvases / iframes) mid-drag. `live` is the
  // press's own copy of the drag state, so handlers never read stale state.
  useEffect(() => {
    if (!drag) return
    let live: Drag<T> | null = drag
    const pointerId = drag.pointerId
    const update = (d: Drag<T> | null) => {
      live = d
      setDrag(d)
    }
    const onMove = (e: globalThis.PointerEvent) => {
      const d = live
      if (!d || e.pointerId !== pointerId) return
      const dx = e.clientX - d.startX
      const dy = e.clientY - d.startY
      if (!d.started && Math.hypot(dx, dy) < DRAG_THRESHOLD) return
      e.preventDefault()
      const mm = mRef.current
      const cur = d.orig.find((i) => i.id === d.id)
      if (!cur) return
      const stepX = mm.colW + mm.gap
      const stepY = mm.rowH + mm.gap
      let preview: T[]
      if (d.mode === 'move') {
        const px = toPx(cur, mm)
        preview = moveItem(d.orig, d.id, {
          ...cur,
          x: Math.round((px.left + dx) / stepX),
          y: Math.round((px.top + dy) / stepY),
        })
      } else {
        const px = toPx(cur, mm)
        preview = resizeItem(d.orig, d.id, {
          w: Math.round((px.width + dx + mm.gap) / stepX),
          h: Math.round((px.height + dy + mm.gap) / stepY),
        })
      }
      update({ ...d, started: true, dx, dy, preview })
    }
    const onUp = (e: globalThis.PointerEvent) => {
      const d = live
      if (!d || e.pointerId !== pointerId) return
      update(null)
      if (d.started && !sameLayout(d.preview, d.orig)) onChangeRef.current(d.preview)
    }
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') update(null)
    }
    window.addEventListener('pointermove', onMove)
    window.addEventListener('pointerup', onUp)
    window.addEventListener('pointercancel', onUp)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('pointermove', onMove)
      window.removeEventListener('pointerup', onUp)
      window.removeEventListener('pointercancel', onUp)
      window.removeEventListener('keydown', onKey)
    }
    // Re-bind only per press, not per pointermove.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [drag?.pointerId, drag === null])

  const begin = (e: PointerEvent<HTMLElement>, id: string, mode: Drag<T>['mode']) => {
    if (e.button !== 0 || narrow) return
    const base = compact(items)
    setDrag({
      id,
      mode,
      pointerId: e.pointerId,
      startX: e.clientX,
      startY: e.clientY,
      started: false,
      orig: base,
      preview: base,
      dx: 0,
      dy: 0,
    })
  }

  const onGripKey = (e: KeyboardEvent<HTMLElement>, id: string) => {
    const dirs: Record<string, 'left' | 'right' | 'up' | 'down'> = {
      ArrowLeft: 'left',
      ArrowRight: 'right',
      ArrowUp: 'up',
      ArrowDown: 'down',
    }
    const dir = dirs[e.key]
    if (!dir) return
    e.preventDefault()
    const base = compact(items)
    let next: T[]
    if (e.shiftKey) {
      const cur = base.find((i) => i.id === id)
      if (!cur) return
      next = resizeItem(base, id, {
        w: cur.w + (dir === 'right' ? 1 : dir === 'left' ? -1 : 0),
        h: Math.min(MAX_H, Math.max(MIN_H, cur.h + (dir === 'down' ? 1 : dir === 'up' ? -1 : 0))),
      })
    } else {
      next = nudgeItem(base, id, dir)
    }
    if (!sameLayout(next, items)) onChange(next)
  }

  const layout = drag?.started ? drag.preview : compact(items)
  const rows = gridHeight(layout)

  const activeId = drag?.started ? drag.id : null
  const origOf = (id: string) => drag?.orig.find((i) => i.id === id)
  // While dragging, leave room below the content to drop into.
  const stageRows = rows + (activeId ? 4 : 0)
  const stageHeight = Math.max(0, stageRows * (m.rowH + m.gap) - m.gap)
  const mode = items.length === 0 ? 'empty' : narrow ? 'narrow' : 'grid'

  let content: ReactNode
  if (mode === 'empty') {
    content = emptyState
  } else if (mode === 'narrow') {
    content = [...layout]
      .sort((a, b) => a.y - b.y || a.x - b.x)
      .map((item) => (
        <div
          key={item.id}
          className="widget-cell"
          style={{ height: Math.max(6, item.h) * ROW_HEIGHT }}
          onPointerDownCapture={() => setFocusedId(item.id)}
          onFocusCapture={() => setFocusedId(item.id)}
        >
          {renderWidget(item, {
            focused: focusedId === item.id,
            narrow: true,
            active: false,
            dragHandle: undefined,
            grip: undefined,
          })}
        </div>
      ))
  } else {
    const target = activeId ? layout.find((i) => i.id === activeId) : undefined
    content = (
      <div className="widget-stage" style={{ height: stageHeight }}>
        {target && (
          <div
            className="widget-ghost"
            style={toPx(target, m)}
            data-testid="widget-ghost"
            aria-hidden="true"
          />
        )}
        {width > 0 &&
          items.map((raw) => {
            const item = layout.find((i) => i.id === raw.id) ?? raw
            const isActive = item.id === activeId
            let style: CSSProperties = toPx(item, m)
            if (isActive && drag) {
              const o = origOf(item.id) ?? item
              const p = toPx(o, m)
              style =
                drag.mode === 'move'
                  ? { ...p, left: p.left + drag.dx, top: p.top + drag.dy }
                  : {
                      ...p,
                      width: Math.max(m.colW, p.width + drag.dx),
                      height: Math.max(MIN_H * m.rowH, p.height + drag.dy),
                    }
            }
            return (
              <div
                key={item.id}
                className={`widget-cell${isActive ? ' widget-cell-active' : ''}`}
                style={style}
                data-widget-id={item.id}
                onPointerDownCapture={() => {
                  if (focusedId !== item.id) setFocusedId(item.id)
                }}
                onFocusCapture={() => {
                  if (focusedId !== item.id) setFocusedId(item.id)
                }}
              >
                {renderWidget(item, {
                  focused: focusedId === item.id,
                  narrow: false,
                  active: isActive,
                  dragHandle: { onPointerDown: (e) => begin(e, item.id, 'move') },
                  grip: { onKeyDown: (e) => onGripKey(e, item.id) },
                })}
                <div
                  className="widget-resize-handle"
                  data-testid="widget-resize-handle"
                  aria-hidden="true"
                  onPointerDown={(e) => {
                    e.preventDefault()
                    e.stopPropagation()
                    begin(e, item.id, 'resize')
                  }}
                />
              </div>
            )
          })}
      </div>
    )
  }

  // One root element across modes, so the ResizeObserver stays attached.
  return (
    <div
      className={`widget-grid${mode === 'grid' ? '' : ` widget-grid-${mode}`}${
        activeId ? ' widget-grid-dragging' : ''
      }`}
      ref={rootRef}
      data-testid={testId}
      data-mode={mode}
    >
      {content}
    </div>
  )
}
