import { useMemo, useState } from 'react'
import type { Card } from '../types/api'
import type { TodoItem } from '../types/todo'

interface ProjectTodoSummaryProps {
  cards: Card[]
  /** cardId -> that card's latest todo snapshot (cards with none are absent). */
  todosByCard: Record<string, TodoItem[]>
}

interface CardGroup {
  cardId: string
  cardTitle: string
  items: TodoItem[]
}

/** in_progress items first so the active work stays most visible, then
 * pending. Stable within each bucket via the original index. */
function sortActive(items: TodoItem[]): TodoItem[] {
  const indexed = items.map((item, idx) => ({ item, idx }))
  indexed.sort((a, b) => {
    const rank = (s: TodoItem['status']) => (s === 'in_progress' ? 0 : 1)
    const r = rank(a.item.status) - rank(b.item.status)
    return r !== 0 ? r : a.idx - b.idx
  })
  return indexed.map((e) => e.item)
}

/**
 * Project-level roll-up of *active* work items, grouped by card. Renders one
 * section per card that still has in-progress or pending todos; cards with
 * only done items (or none at all) are omitted, and the panel hides itself
 * entirely when nothing is active.
 *
 * The standalone `/projects/:id/todos` page is the place to view the full
 * picture including done items — this docked panel is the "what's left" peek.
 */
export default function ProjectTodoSummary({ cards, todosByCard }: ProjectTodoSummaryProps) {
  const [collapsed, setCollapsed] = useState(false)

  const groups: CardGroup[] = useMemo(() => {
    const list: CardGroup[] = []
    for (const card of cards) {
      const todos = todosByCard[card.id]
      if (!todos) continue
      const active = todos.filter((t) => t.status !== 'done')
      if (active.length === 0) continue
      list.push({ cardId: card.id, cardTitle: card.title, items: sortActive(active) })
    }
    return list
  }, [cards, todosByCard])

  if (groups.length === 0) return null

  const totalActive = groups.reduce((sum, g) => sum + g.items.length, 0)

  return (
    <div className="project-todo-summary" data-testid="project-todo-summary">
      <button
        className="todo-panel-header"
        onClick={() => setCollapsed((c) => !c)}
        type="button"
        aria-expanded={!collapsed}
      >
        <span className="todo-panel-caret">{collapsed ? '▸' : '▾'}</span>
        <span className="todo-panel-title">Tasks</span>
        <span className="todo-panel-count" data-testid="todo-panel-count">
          {totalActive} active
        </span>
      </button>
      {!collapsed && (
        <div className="project-todo-summary-body">
          {groups.map((g) => (
            <section
              key={g.cardId}
              className="project-todo-summary-card"
              data-testid="project-todo-summary-card"
              data-card-id={g.cardId}
            >
              <h3 className="project-todo-summary-card-title">{g.cardTitle}</h3>
              <ul className="project-todo-summary-items">
                {g.items.map((item, idx) => (
                  <li
                    key={idx}
                    className={`todo-item todo-item-${item.status}`}
                    data-testid="todo-item"
                    data-status={item.status}
                  >
                    <span className="todo-item-marker" aria-hidden="true">
                      {item.status === 'in_progress' ? '◐' : '○'}
                    </span>
                    <span className="todo-item-text">
                      {item.status === 'in_progress' && item.activeForm
                        ? item.activeForm
                        : item.content}
                    </span>
                  </li>
                ))}
              </ul>
            </section>
          ))}
        </div>
      )}
    </div>
  )
}
