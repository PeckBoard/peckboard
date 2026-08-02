import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { useTabsStore, type Tab } from '../store/tabs'
import { useContextMenu } from '../hooks/useContextMenu'
import { MenuButton, type MenuItem } from './Dropdown'
import { tabDefaultLabel, type TabKindRegistry } from './tabKinds'

interface TabBarProps {
  /** Per-tab-kind glue. The TabBar is kind-agnostic; everything it
   *  needs to render and dispatch a tab comes from this registry,
   *  which the parent (App.tsx) builds from its stores. Adding a new
   *  tab kind = adding a new entry here — no TabBar changes needed. */
  kinds: TabKindRegistry
  /** Open the New Session modal. Renders as a trailing `+` button. */
  onNewSession: () => void
}

/**
 * Top tab strip. Persists server-side via `useTabsStore` so the same
 * set shows up on every device. The Sessions / Projects list entries
 * live in the navigation rail — keeping them out of here means the
 * strip can use all of its horizontal space for tabs, which matters on
 * mobile where the rail is the bottom toolbar.
 *
 * Kind-agnostic: every per-kind decision (label, badges, icon, menu,
 * navigation) is delegated to the [[TabKindRegistry]] passed in by the
 * parent. The TabBar's only job is layout, the long-press / right-click
 * context menu glue, the close affordance, and reordering.
 *
 * Close UX:
 *   Desktop: an X button on each tab (visible on hover/active); also
 *     right-click → context menu with Close tab + the kind's items.
 *   Mobile: the X button stays visible (no hover to reveal it) and
 *     long-press opens the same context menu.
 *   Closing the active tab clears the kind's active id and navigates
 *   to the list view (via `kind.onClose`) so App.tsx's open-on-active
 *   effect can't immediately re-open the tab from the stale URL.
 *
 * Reorder UX:
 *   Desktop: drag a chip onto another to drop it into that slot (HTML5
 *     drag-and-drop).
 *   Everywhere (incl. touch, where native drag doesn't fire): the
 *     context menu's Move left / Move right entries.
 */
export default function TabBar({ kinds, onNewSession }: TabBarProps) {
  const tabs = useTabsStore((s) => s.tabs)
  const closeTab = useTabsStore((s) => s.closeTab)
  const moveTab = useTabsStore((s) => s.moveTab)

  // Cmd/Ctrl+1..9 activates the Nth visible tab (global shortcut; the
  // cheat sheet in ShortcutsModal lists it). Lives here rather than in
  // App.tsx because this component owns the kind registry that knows how
  // to activate each tab type. The registry object is rebuilt on every
  // parent render, so the handler reads it through a ref (updated in an
  // effect) instead of re-binding the listener each time.
  const kindsRef = useRef(kinds)
  useEffect(() => {
    kindsRef.current = kinds
  })
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return
      if (e.key < '1' || e.key > '9') return
      const reg = kindsRef.current
      const shown = useTabsStore.getState().tabs.filter((t) => reg[t.itemType])
      const tab = shown[Number(e.key) - 1]
      if (!tab) return
      e.preventDefault()
      reg[tab.itemType].onActivate(tab)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])
  // Index of the chip currently being dragged. A ref (not state)
  // because it only needs to survive from dragstart to drop and
  // shouldn't trigger re-renders; `dragOver` *is* state so the drop
  // target can show an insertion cue.
  const dragFrom = useRef<number | null>(null)
  const [dragOver, setDragOver] = useState<number | null>(null)

  const handleDrop = (to: number) => {
    const from = dragFrom.current
    dragFrom.current = null
    setDragOver(null)
    if (from !== null) moveTab(from, to)
  }

  // Roving tabindex: the whole strip is ONE Tab stop. Exactly one chip is
  // tabbable — the one focus is on, falling back to the active tab — and
  // Left/Right/Home/End move focus between chips from there. Activation
  // stays manual (arrows only move focus, Enter/Space activates) because
  // activating a tab navigates the whole app, and focus-follows-selection
  // would fire a navigation on every arrow press.
  const stripRef = useRef<HTMLDivElement>(null)
  const [focusedKey, setFocusedKey] = useState<string | null>(null)
  // Keys, not indices: closing or reordering a tab shifts every index.
  // A tab whose kind isn't in the registry renders nothing, so the roving
  // key has to be one that does — otherwise nothing in the strip would be
  // tabbable at all.
  const shownKeys = tabs.filter((t) => kinds[t.itemType]).map((t) => `${t.itemType}:${t.itemId}`)
  const activeTab = tabs.find((t) => kinds[t.itemType]?.isActive(t))
  const activeKey = activeTab ? `${activeTab.itemType}:${activeTab.itemId}` : null
  const rovingKey =
    (focusedKey && shownKeys.includes(focusedKey) ? focusedKey : null) ??
    (activeKey && shownKeys.includes(activeKey) ? activeKey : null) ??
    shownKeys[0] ??
    null

  const chips = () =>
    Array.from(stripRef.current?.querySelectorAll<HTMLElement>('[role="tab"]') ?? [])

  const onStripKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return
    // Only keys pressed ON a chip steer the strip. A chip's context menu is
    // a React portal: its DOM node lives on `document.body`, but its events
    // still bubble up the React tree to this handler, and swallowing the
    // menu's own Home/End would break its keyboard navigation.
    const chip = (e.target as HTMLElement).closest<HTMLElement>('[role="tab"]')
    if (!chip) return
    const all = chips()
    if (all.length === 0) return
    const from = all.indexOf(chip)
    const cur = from >= 0 ? from : 0
    const next =
      e.key === 'Home'
        ? 0
        : e.key === 'End'
          ? all.length - 1
          : (cur + (e.key === 'ArrowRight' ? 1 : -1) + all.length) % all.length
    e.preventDefault()
    // Moving focus fires onFocusCapture below, which is what actually
    // moves the roving tabindex — no separate bookkeeping.
    all[next].focus()
  }
  // Chip text. `||` (not `??`) is intentional: openTab's optimistic insert
  // stores `name: ''` for the brief window between the local insert and the
  // upsert response landing, and the empty string must fall through to the
  // placeholder rather than render as a label-less chip. Same reason the
  // live name falls back to `t.name`.
  const labelOf = (t: Tab) =>
    kinds[t.itemType]?.getLiveName(t) || t.name || tabDefaultLabel[t.itemType]

  // Overflow menu. The strip scrolls horizontally, so past roughly eight
  // tabs some chips sit outside its box with nothing saying so — scrubbing
  // sideways is the only way to find them. `hiddenKeys` is the set of chips
  // the current scroll position clips; the `»` trigger beside `+` lists
  // them and jumps straight to one.
  const [hiddenKeys, setHiddenKeys] = useState<string[]>([])

  const measureOverflow = useCallback(() => {
    const strip = stripRef.current
    const bar = strip?.parentElement
    if (!strip || !bar) return
    const box = strip.getBoundingClientRect()
    // The `»` trigger takes its width out of the strip only once it is
    // rendered. Measuring against the raw box while it is hidden would
    // flip-flop forever: show the trigger → the strip narrows → the chip
    // that fits no longer does. So reserve the space it is about to claim
    // (it is sized like `.tab-new`, plus the bar's gap). Over-reserving is
    // safe; under-reserving is what oscillates.
    const reserve = bar.querySelector('.tab-overflow')
      ? 0
      : (bar.querySelector('.tab-new')?.getBoundingClientRect().width ?? 32) + 8
    const right = box.right - reserve
    const next: string[] = []
    for (const el of Array.from(strip.querySelectorAll<HTMLElement>('[data-tab-id]'))) {
      const key = el.dataset.tabId
      if (!key) continue
      const r = el.getBoundingClientRect()
      // 1px slack: sub-pixel layout rounding otherwise reports a fully
      // visible edge chip as clipped.
      if (r.left < box.left - 1 || r.right > right + 1) next.push(key)
    }
    // Same-value guard — this runs on every scroll frame, and a fresh array
    // identity each time would re-render the whole strip.
    setHiddenKeys((prev) =>
      prev.length === next.length && prev.every((k, i) => k === next[i]) ? prev : next,
    )
  }, [])

  // Re-subscribe whenever the chip set changes: the observer watches the
  // individual chips, which is what catches a label arriving late from
  // `getLiveName` and re-flowing the row.
  const shownKeysSig = shownKeys.join('|')
  useLayoutEffect(() => {
    const strip = stripRef.current
    if (!strip) return
    measureOverflow()
    strip.addEventListener('scroll', measureOverflow, { passive: true })
    const ro = new ResizeObserver(measureOverflow)
    ro.observe(strip)
    for (const child of Array.from(strip.children)) ro.observe(child)
    return () => {
      strip.removeEventListener('scroll', measureOverflow)
      ro.disconnect()
    }
  }, [measureOverflow, shownKeysSig])

  const hiddenSet = new Set(hiddenKeys)
  const hiddenTabs = tabs.filter(
    (t) => kinds[t.itemType] && hiddenSet.has(`${t.itemType}:${t.itemId}`),
  )

  // Jumping to a clipped tab both activates it and scrolls it into view —
  // activating alone would leave the chip off screen.
  const revealTab = (t: Tab) => {
    kinds[t.itemType]?.onActivate(t)
    stripRef.current
      ?.querySelector<HTMLElement>(`[data-tab-id="${CSS.escape(`${t.itemType}:${t.itemId}`)}"]`)
      ?.scrollIntoView({ inline: 'nearest', block: 'nearest' })
  }

  // Always render the strip — even with zero tabs — so the trailing `+`
  // button stays reachable as the user's entry point to creating a new
  // session.
  return (
    <div className="tabbar">
      {/* The tablist owns ONLY the chips — a `tablist` may not have
          non-`tab` children, so the trailing `+` sits outside it. */}
      <div
        className="tab-strip"
        role="tablist"
        aria-label="Open tabs"
        ref={stripRef}
        onKeyDown={onStripKeyDown}
        onFocusCapture={(e) => {
          const chip = (e.target as HTMLElement).closest<HTMLElement>('[role="tab"]')
          if (chip?.dataset.tabKey) setFocusedKey(chip.dataset.tabKey)
        }}
        onBlur={(e) => {
          // Focus left the strip entirely: hand the roving tabindex back
          // to the active tab so tabbing in lands on the current view.
          if (!e.currentTarget.contains(e.relatedTarget)) setFocusedKey(null)
        }}
      >
        {tabs.map((t, i) => {
          const kind = kinds[t.itemType]
          if (!kind) return null
          const label = labelOf(t)
          const active = kind.isActive(t)
          const badges = kind.getBadges(t, active)
          const closeTitle = kind.getCloseTitle?.(t) ?? 'Close tab'
          // Reorder without a pointer drag — the only way to reorder on
          // touch. Layered before the kind's items; "Close tab" is
          // prepended inside OpenedTab.
          const reorderItems: MenuItem[] = [
            { label: 'Move left', onSelect: () => moveTab(i, i - 1), disabled: i === 0 },
            {
              label: 'Move right',
              onSelect: () => moveTab(i, i + 1),
              disabled: i === tabs.length - 1,
            },
          ]
          return (
            <OpenedTab
              key={`${t.itemType}:${t.itemId}`}
              label={label}
              active={active}
              closeTitle={closeTitle}
              running={badges.running}
              unread={badges.unread}
              icon={kind.getIcon(t)}
              menuItems={[...reorderItems, ...kind.getMenuItems(t)]}
              tabKey={`${t.itemType}:${t.itemId}`}
              posInSet={i + 1}
              setSize={tabs.length}
              focusable={rovingKey === `${t.itemType}:${t.itemId}`}
              dragOver={dragOver === i}
              onClick={() => kind.onActivate(t)}
              onClose={() => {
                // Clear the active id first (navigate away) so the stale
                // URL can't re-open the tab, then drop it from the strip.
                kind.onClose(t)
                closeTab(t.itemType, t.itemId)
              }}
              onDragStart={() => {
                dragFrom.current = i
              }}
              onDragEnter={() => {
                if (dragFrom.current !== null) setDragOver(i)
              }}
              onDrop={() => handleDrop(i)}
              onDragEnd={() => {
                dragFrom.current = null
                setDragOver(null)
              }}
            />
          )
        })}
      </div>
      {/* Off-screen tabs. Rendered only while the strip is actually
          clipping something, and BEFORE `+` so the new-session button
          stays the last child of the bar. */}
      {hiddenTabs.length > 0 && (
        <MenuButton
          items={hiddenTabs.map((t) => ({ label: labelOf(t), onSelect: () => revealTab(t) }))}
          ariaLabel={`${hiddenTabs.length} ${hiddenTabs.length === 1 ? 'tab' : 'tabs'} off screen`}
          title="Tabs off screen"
          testId="tab-overflow"
          triggerClassName="tab-overflow"
        >
          &raquo;
        </MenuButton>
      )}
      <button
        type="button"
        className="tab-new"
        title="New session"
        aria-label="New session"
        onClick={onNewSession}
      >
        +
      </button>
    </div>
  )
}

interface OpenedTabProps {
  label: string
  closeTitle: string
  active: boolean
  running: boolean
  unread: boolean
  icon: React.ReactNode
  menuItems: MenuItem[]
  tabKey: string
  /** 1-based position in the strip, for `aria-posinset`. */
  posInSet: number
  setSize: number
  /** True for the single chip that carries `tabindex=0`. */
  focusable: boolean
  dragOver: boolean
  onClick: () => void
  onClose: () => void
  onDragStart: () => void
  onDragEnter: () => void
  onDrop: () => void
  onDragEnd: () => void
}

function OpenedTab({
  label,
  closeTitle,
  active,
  running,
  unread,
  icon,
  menuItems,
  tabKey,
  posInSet,
  setSize,
  focusable,
  dragOver,
  onClick,
  onClose,
  onDragStart,
  onDragEnter,
  onDrop,
  onDragEnd,
}: OpenedTabProps) {
  // The right-click / long-press menu always starts with "Close tab" —
  // it's tab-strip chrome, not a property of the underlying item.
  // Per CLAUDE.md "Component Reuse", the per-kind items below mirror
  // the closest 3-dot menu surface (sessions match ChatView, etc.).
  const items: MenuItem[] = [{ label: 'Close tab', onSelect: onClose }, ...menuItems]
  const { triggerProps, menu, consumeLongPressClick } = useContextMenu(() =>
    items
      .filter((m) => !m.divider && !m.hidden)
      .map((m) => ({
        label: m.label ?? '',
        onSelect: () => m.onSelect?.(),
        danger: m.danger,
        disabled: m.disabled,
      })),
  )

  return (
    <div
      role="presentation"
      className={`tab-wrap ${dragOver ? 'tab-dragover' : ''}`}
      data-tab-id={tabKey}
      draggable
      onDragStart={(e) => {
        // Firefox refuses to start a drag unless data is set; the
        // payload is unused (the dragged index lives in TabBar).
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', tabKey)
        onDragStart()
      }}
      onDragEnter={onDragEnter}
      onDragOver={(e) => e.preventDefault()}
      onDrop={(e) => {
        e.preventDefault()
        onDrop()
      }}
      onDragEnd={onDragEnd}
    >
      <button
        role="tab"
        id={`tab-${tabKey}`}
        aria-selected={active}
        aria-controls="view-panel"
        aria-posinset={posInSet}
        aria-setsize={setSize}
        data-tab-key={tabKey}
        tabIndex={focusable ? 0 : -1}
        className={`tab tab-opened ${active ? 'tab-active' : ''}`}
        onClick={(e) => {
          if (consumeLongPressClick(e)) return
          onClick()
        }}
        {...triggerProps}
        onKeyDown={(e) => {
          // Compose with the hook's Shift+F10 / Context-Menu handling
          // rather than replacing it.
          triggerProps.onKeyDown(e)
          if (e.defaultPrevented) return
          // The close button is out of the Tab order (the strip is one
          // Tab stop), so Delete/Backspace on the focused chip is the
          // keyboard route to closing a tab — the context menu's
          // "Close tab" is the other.
          if (e.key === 'Delete' || e.key === 'Backspace') {
            e.preventDefault()
            onClose()
          }
        }}
      >
        {icon}
        {running ? (
          <span className="tab-dot tab-dot-running" role="img" aria-label="Running" />
        ) : unread ? (
          <span className="tab-dot tab-dot-unread" role="img" aria-label="Unread output" />
        ) : null}
        <span className="tab-label">{label}</span>
      </button>
      <button
        type="button"
        tabIndex={-1}
        className="tab-close"
        aria-label={`Close ${label}`}
        title={closeTitle}
        onClick={(e) => {
          e.stopPropagation()
          onClose()
        }}
      >
        &#10005;
      </button>
      {menu}
    </div>
  )
}
