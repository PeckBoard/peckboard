import TodoPanel from './TodoPanel'
import type { TodoItem } from '../types/todo'

interface ProjectTodoSummaryProps {
  /** cardId -> that card's latest todo snapshot (cards with none are absent). */
  todosByCard: Record<string, TodoItem[]>
}

/**
 * Project-level roll-up of work items across every card's worker session.
 * Flattens each card's snapshot into one list and reuses the shared TodoPanel
 * to group it by Pending / In Progress / Done, so the board shows the same
 * grouped layout the chat-session panel does. Renders nothing when no card has
 * any todos.
 */
export default function ProjectTodoSummary({ todosByCard }: ProjectTodoSummaryProps) {
  const allTodos = Object.values(todosByCard).flat()
  if (allTodos.length === 0) return null

  return (
    <div className="project-todo-summary" data-testid="project-todo-summary">
      <TodoPanel todos={allTodos} />
    </div>
  )
}
