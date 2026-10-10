import { useMemo } from 'react'
import { parseTodoItems, type TodoItem } from '../../../types/todo'
import WidgetFrame from '../WidgetFrame'
import { DashCount, DashEmpty, DashError, DashLoading, DashNoMatch } from './DashParts'
import { useDashboardData } from './useDashboardData'
import type { InfoWidgetProps } from './types'
import {
  FilterBar,
  FilterButton,
  inSet,
  matchesSearch,
  useWidgetFilters,
  type FilterDef,
} from './filters'
import '../../../styles/dashboard-info.css'

interface CardTodos {
  card_id: string
  card_title: string
  todos: TodoItem[]
}

const GLYPH: Record<TodoItem['status'], string> = { done: '✓', in_progress: '◐', pending: '○' }

/** Status chip value per todo status (the contract says `completed`). */
const STATUS_KEY: Record<TodoItem['status'], string> = {
  pending: 'pending',
  in_progress: 'in_progress',
  done: 'completed',
}

const FILTER_DEFS: FilterDef[] = [
  { key: 'q', kind: 'search', placeholder: 'Search todos…' },
  {
    key: 'status',
    kind: 'chips',
    label: 'Status',
    options: [
      { value: 'pending', label: 'Pending' },
      { value: 'in_progress', label: 'In progress' },
      { value: 'completed', label: 'Completed' },
    ],
  },
]

/** Worker todo lists across one project's cards (`GET /api/projects/{id}/todos`),
 *  each card with a progress bar; open items first. */
export default function TodosWidget(props: InfoWidgetProps) {
  const { widget, ctx, menuItems, scopeProjectId, scopeName, onOpenProject } = props
  const { data, error, reload } = useDashboardData<{ cards?: unknown }>(
    scopeProjectId ? `/api/projects/${encodeURIComponent(scopeProjectId)}/todos` : null,
    null,
    ['card-update', 'card-delete'],
  )
  const groups = useMemo<CardTodos[]>(() => {
    const raw = Array.isArray(data?.cards) ? (data.cards as Record<string, unknown>[]) : []
    return raw
      .filter((c) => c && typeof c.card_id === 'string')
      .map((c) => ({
        card_id: c.card_id as string,
        card_title: typeof c.card_title === 'string' ? c.card_title : '',
        todos: parseTodoItems(c.todos),
      }))
      .filter((g) => g.todos.length > 0)
  }, [data])
  const open = groups.reduce((n, g) => n + g.todos.filter((t) => t.status !== 'done').length, 0)
  const { filters, set, clear, activeCount, open: barOpen, toggle } = useWidgetFilters(props)
  // Filters narrow items; a card shows while any of its items match. The
  // progress bar still counts the card's whole list.
  const shown = groups
    .map((g) => ({
      ...g,
      items: g.todos.filter(
        (t) =>
          inSet(filters.status, STATUS_KEY[t.status]) &&
          matchesSearch(filters.q, t.content, t.activeForm, g.card_title),
      ),
    }))
    .filter((g) => g.items.length > 0)

  let body
  if (error && !data) body = <DashError message={error} onRetry={() => void reload()} />
  else if (!data) body = <DashLoading />
  else if (groups.length === 0)
    body = (
      <DashEmpty testId="dash-todos-empty">
        <p>No todos yet</p>
        <p className="form-hint">Todos appear when a card&apos;s worker reports them.</p>
      </DashEmpty>
    )
  else if (shown.length === 0) body = <DashNoMatch onClear={clear} />
  else
    body = (
      <div className="dash-scroll">
        <ul className="dash-todo-groups" data-testid="dash-todos-list">
          {shown.map((g) => {
            const done = g.todos.filter((t) => t.status === 'done').length
            const order = { in_progress: 0, pending: 1, done: 2 }
            const items = g.items.slice().sort((a, b) => order[a.status] - order[b.status])
            return (
              <li key={g.card_id} className="dash-todo-group" data-card-id={g.card_id}>
                <div className="dash-todo-head">
                  <span className="dash-item-title" title={g.card_title}>
                    {g.card_title || 'Untitled card'}
                  </span>
                  <span className="dash-item-num">
                    {done}/{g.todos.length}
                  </span>
                </div>
                <div
                  className="dash-progress"
                  role="progressbar"
                  aria-valuemin={0}
                  aria-valuemax={g.todos.length}
                  aria-valuenow={done}
                  aria-label={`${g.card_title}: ${done} of ${g.todos.length} done`}
                >
                  <span style={{ width: `${(done / g.todos.length) * 100}%` }} />
                </div>
                <ul className="dash-todo-items">
                  {items.map((t, i) => (
                    <li key={i} className={`dash-todo dash-todo-${t.status}`}>
                      <span className="dash-todo-glyph" aria-hidden="true">
                        {GLYPH[t.status]}
                      </span>
                      <span className="dash-todo-text">
                        {t.status === 'in_progress' && t.activeForm ? t.activeForm : t.content}
                      </span>
                    </li>
                  ))}
                </ul>
              </li>
            )
          })}
        </ul>
      </div>
    )

  return (
    <WidgetFrame
      kind="todos"
      widgetId={widget.id}
      title={scopeName ? `${scopeName} — Todos` : 'Todos'}
      onTitleClick={scopeProjectId ? () => onOpenProject(scopeProjectId) : undefined}
      statusSlot={data && <DashCount n={open} label={`${open} open`} />}
      actions={<FilterButton activeCount={activeCount} open={barOpen} onToggle={toggle} />}
      menuItems={menuItems}
      ctx={ctx}
      dataAttrs={{ 'data-project-id': scopeProjectId ?? undefined }}
    >
      {barOpen && <FilterBar defs={FILTER_DEFS} filters={filters} set={set} clear={clear} />}
      {body}
    </WidgetFrame>
  )
}
