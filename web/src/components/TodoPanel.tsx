import { useState } from 'react'
import type { TodoItem, TodoStatus } from '../types/todo'

/** Render order: active work first so it's most visible, then what's queued,
 * then what's already finished. */
const GROUP_ORDER: { status: TodoStatus; label: string }[] = [
  { status: 'in_progress', label: 'In Progress' },
  { status: 'pending', label: 'Pending' },
  { status: 'done', label: 'Done' },
]

const MARKER: Record<TodoStatus, string> = {
  pending: '○', // ○
  in_progress: '◐', // ◐
  done: '✓', // ✓
}

interface TodoPanelProps {
  todos: TodoItem[]
}

/**
 * Collapsible panel that renders the current todo snapshot grouped by
 * lifecycle state. Renders nothing when there are no todos, so it stays out of
 * the way for sessions that never report any. Reusable across the chat session
 * view and the project-page aggregate view.
 */
export default function TodoPanel({ todos }: TodoPanelProps) {
  const [collapsed, setCollapsed] = useState(false)

  if (todos.length === 0) return null

  const doneCount = todos.filter((t) => t.status === 'done').length

  return (
    <div className="todo-panel" data-testid="todo-panel">
      <button
        className="todo-panel-header"
        onClick={() => setCollapsed((c) => !c)}
        type="button"
        aria-expanded={!collapsed}
      >
        <span className="todo-panel-caret">{collapsed ? '▸' : '▾'}</span>
        <span className="todo-panel-title">Tasks</span>
        <span className="todo-panel-count" data-testid="todo-panel-count">
          {doneCount}/{todos.length} done
        </span>
      </button>
      {!collapsed && (
        <div className="todo-panel-body">
          {GROUP_ORDER.map(({ status, label }) => {
            const group = todos.filter((t) => t.status === status)
            if (group.length === 0) return null
            return (
              <div key={status} className={`todo-group todo-group-${status}`}>
                <div className="todo-group-label">
                  <span>{label}</span>
                  <span className="todo-group-badge">{group.length}</span>
                </div>
                <ul className="todo-group-items">
                  {group.map((item, idx) => (
                    <li
                      key={idx}
                      className={`todo-item todo-item-${status}`}
                      data-testid="todo-item"
                      data-status={status}
                    >
                      <span className="todo-item-marker" aria-hidden="true">
                        {MARKER[status]}
                      </span>
                      <span className="todo-item-text">
                        {status === 'in_progress' && item.activeForm
                          ? item.activeForm
                          : item.content}
                      </span>
                    </li>
                  ))}
                </ul>
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}
