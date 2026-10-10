/* eslint-disable react-refresh/only-export-components --
   The filter primitive is one module by contract (tmp-widgets4-contract.md):
   hook, helpers and components are imported together by every widget. */
import { useEffect, useRef, useState } from 'react'
import { MenuButton, type MenuItem } from '../../Dropdown'
import type { FilterValue, Filters } from '../../../store/views'
import type { InfoWidgetProps } from './types'
import '../../../styles/dashboard-filters.css'

export type { FilterValue, Filters } from '../../../store/views'

type Option = { value: string; label: string }

export type FilterDef =
  | { key: string; kind: 'search'; placeholder?: string }
  /** Boolean chip. */
  | { key: string; kind: 'toggle'; label: string }
  /** Multi-select chips over a small fixed set. */
  | { key: string; kind: 'chips'; label: string; options: Option[] }
  /** Single choice over a small set; '' = any. */
  | { key: string; kind: 'select'; label: string; options: Option[] }
  /** Searchable multi-select for dynamic / large sets (sessions, tags…). */
  | { key: string; kind: 'combo'; label: string; options: Option[] }

type SetFilter = (key: string, v: FilterValue | undefined) => void

/** Search debounce before a keystroke is persisted. */
const SEARCH_DEBOUNCE_MS = 200

function isActive(v: FilterValue | undefined): boolean {
  if (Array.isArray(v)) return v.length > 0
  if (typeof v === 'string') return v.trim() !== ''
  return v === true
}

function countActive(f: Filters): number {
  return Object.values(f).filter(isActive).length
}

/** Reads/writes `widget.filters` through `InfoWidgetProps.onChange`.
 *  `open` / `toggle` drive the bar's local (unpersisted) visibility; it
 *  starts open when any filter is active. */
export function useWidgetFilters(props: InfoWidgetProps): {
  filters: Filters
  set: SetFilter
  clear: (() => void) & { epoch: number }
  activeCount: number
  open: boolean
  toggle: () => void
} {
  const { widget, onChange } = props
  const filters = widget.filters ?? {}
  const activeCount = countActive(filters)
  // Latest value, so two sets before the next render don't drop one.
  const latest = useRef(filters)
  useEffect(() => {
    latest.current = widget.filters ?? {}
  }, [widget.filters])
  const [open, setOpen] = useState(activeCount > 0)

  const set: SetFilter = (key, v) => {
    const next: Filters = { ...latest.current }
    if (v === undefined || !isActive(v)) delete next[key]
    else next[key] = v
    latest.current = next
    onChange({ filters: Object.keys(next).length > 0 ? next : null })
  }
  // Each render's clear is tagged with the current `epoch`, bumped by every
  // clear; FilterBar hands it to the search box, which then drops a
  // debounced keystroke still pending from before the clear instead of
  // re-saving it afterwards.
  const [epoch, setEpoch] = useState(0)
  function clear() {
    latest.current = {}
    onChange({ filters: null })
    setEpoch((e) => e + 1)
  }
  clear.epoch = epoch
  return { filters, set, clear, activeCount, open, toggle: () => setOpen((o) => !o) }
}

/** Header toggle for the filter bar: funnel icon + active-count badge. */
export function FilterButton({
  activeCount,
  open,
  onToggle,
}: {
  activeCount: number
  open: boolean
  onToggle: () => void
}) {
  return (
    <button
      type="button"
      className={`widget-filter-btn${activeCount > 0 ? ' widget-filter-btn-active' : ''}`}
      aria-pressed={open}
      aria-label={activeCount > 0 ? `Filters (${activeCount} active)` : 'Filters'}
      title={open ? 'Hide filters' : 'Show filters'}
      data-testid="widget-filter-button"
      onClick={onToggle}
    >
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
        <path
          d="M2 3h12l-4.5 5.5V13l-3 1.5v-6z"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.3"
          strokeLinejoin="round"
        />
      </svg>
      {activeCount > 0 && <span className="widget-filter-badge">{activeCount}</span>}
    </button>
  )
}

/** Search box that keeps its own text and persists it debounced. */
function SearchFilter({
  id,
  value,
  placeholder,
  onCommit,
  resetEpoch,
}: {
  id: string
  value: string
  placeholder?: string
  onCommit: (v: string) => void
  /** Bumps on Clear: abandons a pending keystroke and resyncs the text. */
  resetEpoch: number
}) {
  const [text, setText] = useState(value)
  const [dirty, setDirty] = useState(false)
  const [seen, setSeen] = useState(value)
  const [seenEpoch, setSeenEpoch] = useState(resetEpoch)
  if (resetEpoch !== seenEpoch) {
    // Clearing dirty re-runs the debounce effect, whose cleanup cancels
    // the pending commit.
    setSeenEpoch(resetEpoch)
    setSeen(value)
    setDirty(false)
    setText(value)
  } else if (value !== seen) {
    // An outside change (another tab's save) replaces the text unless the
    // user is mid-edit.
    setSeen(value)
    if (!dirty) setText(value)
  }
  // Latest callback, read when the debounce fires, so a parent re-render
  // doesn't restart the timer.
  const commit = useRef(onCommit)
  useEffect(() => {
    commit.current = onCommit
  })
  useEffect(() => {
    if (!dirty) return
    const t = window.setTimeout(() => {
      setDirty(false)
      commit.current(text)
    }, SEARCH_DEBOUNCE_MS)
    return () => window.clearTimeout(t)
  }, [text, dirty])
  return (
    <input
      type="search"
      className="widget-filter-search"
      value={text}
      placeholder={placeholder ?? 'Search…'}
      aria-label={placeholder ?? 'Search'}
      data-testid={`widget-filter-${id}`}
      onChange={(e) => {
        setText(e.target.value)
        setDirty(true)
      }}
    />
  )
}

function asList(v: FilterValue | undefined): string[] {
  if (Array.isArray(v)) return v
  return typeof v === 'string' && v ? [v] : []
}

/** Searchable multi-select: each pick toggles one value. */
function ComboFilter({
  def,
  value,
  set,
}: {
  def: Extract<FilterDef, { kind: 'combo' }>
  value: string[]
  set: SetFilter
}) {
  const picked = new Set(value)
  const labelOf = (v: string) => def.options.find((o) => o.value === v)?.label ?? v
  const toggle = (v: string) =>
    set(def.key, picked.has(v) ? value.filter((x) => x !== v) : [...value, v])
  // Picked values the options no longer list stay visible so they can be removed.
  const extra = value.filter((v) => !def.options.some((o) => o.value === v))
  const items: MenuItem[] = [
    ...def.options.map((o) => ({
      label: o.label,
      searchText: o.value,
      active: picked.has(o.value),
      onSelect: () => toggle(o.value),
    })),
    ...extra.map((v) => ({ label: v, active: true, onSelect: () => toggle(v) })),
  ]
  if (value.length > 0)
    items.push({ divider: true }, { label: 'Any', onSelect: () => set(def.key, undefined) })
  const summary =
    value.length === 0
      ? 'Any'
      : value.length === 1
        ? labelOf(value[0])
        : `${labelOf(value[0])} +${value.length - 1}`
  return (
    <MenuButton
      items={items}
      searchable
      haspopup="listbox"
      align="left"
      ariaLabel={`${def.label}: ${summary}`}
      listLabel={def.label}
      searchPlaceholder={`Search ${def.label.toLowerCase()}…`}
      searchTestId={`widget-filter-${def.key}-search`}
      emptyLabel="Nothing to pick"
      minWidth={180}
      testId={`widget-filter-${def.key}`}
      triggerClassName={`widget-filter-chip widget-filter-combo${value.length > 0 ? ' on' : ''}`}
    >
      <span className="widget-filter-chip-key">{def.label}</span>
      <span className="widget-filter-combo-value">{summary}</span>
      <span className="widget-filter-caret" aria-hidden="true">
        ▾
      </span>
    </MenuButton>
  )
}

/** Compact Datadog-style filter bar for the top of a widget body. Wraps in
 *  narrow widgets. */
export function FilterBar({
  defs,
  filters,
  set,
  clear,
}: {
  defs: FilterDef[]
  filters: Filters
  set: SetFilter
  clear: (() => void) & { epoch?: number }
}) {
  const active = countActive(filters)
  const resetEpoch = clear.epoch ?? 0

  return (
    <div
      className="widget-filter-bar"
      data-testid="widget-filter-bar"
      role="toolbar"
      aria-label="Filters"
    >
      {defs.map((def) => {
        const v = filters[def.key]
        switch (def.kind) {
          case 'search':
            return (
              <SearchFilter
                key={def.key}
                id={def.key}
                value={typeof v === 'string' ? v : ''}
                placeholder={def.placeholder}
                onCommit={(text) => set(def.key, text)}
                resetEpoch={resetEpoch}
              />
            )
          case 'toggle':
            return (
              <button
                key={def.key}
                type="button"
                className={`widget-filter-chip${v === true ? ' on' : ''}`}
                aria-pressed={v === true}
                data-testid={`widget-filter-${def.key}`}
                onClick={() => set(def.key, v === true ? undefined : true)}
              >
                {def.label}
              </button>
            )
          case 'chips': {
            const sel = asList(v)
            return (
              <span
                key={def.key}
                className="widget-filter-group"
                role="group"
                aria-label={def.label}
                data-testid={`widget-filter-${def.key}`}
              >
                {def.options.map((o) => {
                  const on = sel.includes(o.value)
                  return (
                    <button
                      key={o.value}
                      type="button"
                      className={`widget-filter-chip${on ? ' on' : ''}`}
                      aria-pressed={on}
                      data-value={o.value}
                      onClick={() =>
                        set(def.key, on ? sel.filter((x) => x !== o.value) : [...sel, o.value])
                      }
                    >
                      {o.label}
                    </button>
                  )
                })}
              </span>
            )
          }
          case 'select':
            return (
              <select
                key={def.key}
                className={`widget-filter-select${typeof v === 'string' && v ? ' on' : ''}`}
                aria-label={def.label}
                value={typeof v === 'string' ? v : ''}
                data-testid={`widget-filter-${def.key}`}
                onChange={(e) => set(def.key, e.target.value)}
              >
                <option value="">{def.label}: any</option>
                {def.options.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
            )
          case 'combo':
            return <ComboFilter key={def.key} def={def} value={asList(v)} set={set} />
        }
      })}
      {active > 0 && (
        <button
          type="button"
          className="widget-filter-clear"
          data-testid="widget-filter-clear"
          onClick={clear}
        >
          Clear
        </button>
      )}
    </div>
  )
}

/** True when every whitespace-separated term of `q` appears in one of
 *  `fields` (case-insensitive). An empty query matches everything. */
export function matchesSearch(
  q: FilterValue | undefined,
  ...fields: (string | null | undefined)[]
): boolean {
  if (typeof q !== 'string') return true
  const terms = q.toLowerCase().split(/\s+/).filter(Boolean)
  if (terms.length === 0) return true
  const hay = fields
    .filter((f): f is string => !!f)
    .join('\n')
    .toLowerCase()
  return terms.every((t) => hay.includes(t))
}

/** True when `value` is in the selection; an empty selection matches all. */
export function inSet(sel: FilterValue | undefined, value: string | null | undefined): boolean {
  const list = asList(sel)
  if (list.length === 0) return true
  return value != null && list.includes(value)
}
