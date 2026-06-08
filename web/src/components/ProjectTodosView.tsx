import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useProjectsStore } from '../store/projects'
import { parseTodoItems, type TodoItem } from '../types/todo'
import TodoPanel from './TodoPanel'

interface ProjectTodosViewProps {
  projectId: string
  onClose: () => void
}

interface CardTodosGroup {
  card_id: string
  card_title: string
  todos: TodoItem[]
}

/**
 * Dedicated view aggregating every card's todos in a project, grouped by card.
 * Unlike `ProjectTodoSummary` (which hides itself when there is nothing to
 * show), this view ALWAYS renders, with an explicit empty state — it is the
 * destination of the "Todos" button on the Kanban header, so reaching it and
 * seeing "no todos yet" is a valid outcome.
 */
export default function ProjectTodosView({ projectId, onClose }: ProjectTodosViewProps) {
  const project = useProjectsStore((s) => s.projects.find((p) => p.id === projectId))
  const [groups, setGroups] = useState<CardTodosGroup[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/projects/${projectId}/todos`)
      .then(async (res) => {
        if (!res.ok) throw new Error(`failed: ${res.status}`)
        return res.json()
      })
      .then((data: { cards?: unknown }) => {
        if (cancelled) return
        const cards = Array.isArray(data.cards) ? data.cards : []
        const parsed: CardTodosGroup[] = []
        for (const entry of cards) {
          if (!entry || typeof entry !== 'object') continue
          const obj = entry as Record<string, unknown>
          const card_id = typeof obj.card_id === 'string' ? obj.card_id : ''
          const card_title = typeof obj.card_title === 'string' ? obj.card_title : ''
          if (!card_id) continue
          parsed.push({
            card_id,
            card_title,
            todos: parseTodoItems(obj.todos),
          })
        }
        setGroups(parsed)
      })
      .catch((e) => {
        if (cancelled) return
        setError(String(e))
      })
    return () => {
      cancelled = true
    }
  }, [projectId])

  return (
    <div className="project-todos-view" data-testid="project-todos-view">
      <div className="project-todos-header">
        <button className="btn-secondary btn-sm" onClick={onClose} title="Back to board">
          ← Back
        </button>
        <h2 className="project-todos-title">
          {project ? `${project.name} — Todos` : 'Project Todos'}
        </h2>
      </div>
      <div className="project-todos-body">
        {error && <div className="project-todos-error">{error}</div>}
        {!error && groups === null && <div className="project-todos-loading">Loading todos…</div>}
        {!error && groups !== null && groups.length === 0 && (
          <div className="project-todos-empty" data-testid="project-todos-empty">
            <p>No todos yet</p>
            <p className="project-todos-empty-hint">
              Todos appear here when a card&apos;s worker reports them.
            </p>
          </div>
        )}
        {!error &&
          groups !== null &&
          groups.length > 0 &&
          groups.map((g) => (
            <section
              key={g.card_id}
              className="project-todos-card-group"
              data-testid="project-todos-card-group"
              data-card-id={g.card_id}
            >
              <h3 className="project-todos-card-title">{g.card_title}</h3>
              <TodoPanel todos={g.todos} />
            </section>
          ))}
      </div>
    </div>
  )
}
