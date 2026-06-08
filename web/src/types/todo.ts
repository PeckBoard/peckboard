// Frontend representation of the backend `todo` event's work items.
// Kept in sync with `src/todo.rs` (wire form uses snake_case status tokens).

export type TodoStatus = 'pending' | 'in_progress' | 'done'

export interface TodoItem {
  content: string
  status: TodoStatus
  /** Present-tense form a provider shows while an item is in progress. */
  activeForm?: string
}

/**
 * Normalize a raw todos array (from a `todo` event's `data.todos` or the
 * `/todos` endpoint) into typed items. Unknown statuses degrade to `pending`
 * so a stray token never makes an item silently disappear — mirrors the
 * backend's `TodoStatus::from_provider`.
 */
export function parseTodoItems(raw: unknown): TodoItem[] {
  if (!Array.isArray(raw)) return []
  const items: TodoItem[] = []
  for (const entry of raw) {
    if (!entry || typeof entry !== 'object') continue
    const obj = entry as Record<string, unknown>
    const content = typeof obj.content === 'string' ? obj.content : ''
    if (!content) continue
    const rawStatus = typeof obj.status === 'string' ? obj.status : ''
    // `todo` events carry the backend's normalized tokens (pending /
    // in_progress / done), but accept `completed` as a `done` synonym too so a
    // provider/plugin that emits raw tokens still buckets correctly — mirrors
    // the backend's `TodoStatus::from_provider`.
    const status: TodoStatus =
      rawStatus === 'in_progress'
        ? 'in_progress'
        : rawStatus === 'done' || rawStatus === 'completed'
          ? 'done'
          : 'pending'
    const activeForm = typeof obj.activeForm === 'string' ? obj.activeForm : undefined
    items.push({ content, status, activeForm })
  }
  return items
}
