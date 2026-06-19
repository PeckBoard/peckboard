import type { CardReport, PendingQuestion } from '../../store/projects'
import type { Card, Event } from '../../types/api'

// How long a thought bubble lingers after its event before fading out,
// unless a newer event replaces it first.
export const THOUGHT_BUBBLE_MS = 5000

/** Collapse whitespace and clamp a string to `n` chars for the tiny bubble. */
export function truncate(s: string, n = 60): string {
  const t = s.replace(/\s+/g, ' ').trim()
  return t.length > n ? `${t.slice(0, n - 1)}…` : t
}

/** Pull a short, human-meaningful hint out of a tool's input payload. */
export function toolHint(input?: Record<string, unknown>): string {
  if (!input) return ''
  const filePath = (input.file_path as string) ?? (input.path as string)
  if (filePath) return filePath.split('/').pop() || filePath
  const command = input.command as string
  if (command) return truncate(command, 36)
  const pattern = input.pattern as string
  if (pattern) return truncate(pattern, 36)
  const description = input.description as string
  if (description) return truncate(description, 36)
  return ''
}

/**
 * Reduce a raw session event to a one-line summary for a card's thought
 * bubble. Returns an empty string for events that carry no useful signal —
 * those are ignored and leave the current bubble untouched.
 */
export function summarizeEvent(event: Event): string {
  const d = event.data ?? {}
  switch (event.kind) {
    case 'user': {
      const text = (d.text as string) ?? ''
      return text.trim() ? truncate(text) : 'New message'
    }
    case 'agent-start':
      return 'Started working'
    case 'agent-text': {
      const text = (d.text as string) ?? ''
      return text.trim() ? truncate(text) : ''
    }
    case 'agent-tool-start': {
      const name = (d.name as string) ?? (d.tool_name as string) ?? 'tool'
      const hint = toolHint(d.input as Record<string, unknown> | undefined)
      return hint ? `${name}: ${hint}` : `Running ${name}`
    }
    case 'agent-tool-end': {
      const name = (d.name as string) ?? (d.tool_name as string) ?? 'tool'
      return d.error ? `${name} failed` : `${name} done`
    }
    case 'agent-end':
      return (d.status as string) === 'crashed' ? 'Crashed' : 'Done'
    case 'step-change':
      return `Step: ${truncate((d.step as string) ?? (d.label as string) ?? 'next', 40)}`
    case 'question':
      return 'Needs your input'
    case 'interrupt':
      return 'Interrupted'
    case 'system': {
      const text = (d.text as string) ?? (d.message as string) ?? ''
      return text.trim() ? truncate(text) : ''
    }
    default:
      return ''
  }
}

export interface ThoughtBubble {
  text: string
  key: number
}

// Stable references for empty selector results so subscribers don't
// re-render on every store update with an empty array/list.
export const EMPTY_REPORTS: CardReport[] = []
export const EMPTY_QUESTIONS: PendingQuestion[] = []

export const STEPS = [
  { key: 'backlog', label: 'Backlog' },
  { key: 'in_progress', label: 'In Progress' },
  { key: 'review', label: 'Review' },
  { key: 'done', label: 'Done' },
  { key: 'wont_do', label: "Won't Do" },
] as const

export const EFFORT_OPTIONS = [
  { value: '', label: 'Default' },
  { value: 'low', label: 'Low' },
  { value: 'medium', label: 'Medium' },
  { value: 'high', label: 'High' },
  { value: 'xhigh', label: 'Extra high' },
  { value: 'max', label: 'Max' },
]

export interface PriorityInfo {
  label: string
  value: number
  description: string
}

/**
 * Pick the priority a card should adopt when dropped at `insertIdx` inside
 * `rowCards` (cards in the destination step, in the order they appear in
 * the board). Mirrors the backend's ASC-by-priority sort: the card adopts
 * the priority of its new leading neighbor (or trailing, at the start of
 * the row), so it lands in that priority bucket. Same-priority neighbours
 * mean the drop is a same-bucket move and returns the dragged card's
 * current priority — the caller treats that as a no-op write.
 */
export function priorityAtInsertIdx(
  rowCards: Card[],
  draggedId: string,
  insertIdx: number,
  fallback: number,
): number {
  const others = rowCards.filter((c) => c.id !== draggedId)
  // Insert index was computed against the visible row (which still
  // contains the dragged card). Translate it into an index in `others` by
  // subtracting 1 if the dragged card sat before the insertion point.
  const draggedIdx = rowCards.findIndex((c) => c.id === draggedId)
  const adjusted = draggedIdx >= 0 && draggedIdx < insertIdx ? insertIdx - 1 : insertIdx
  if (others.length === 0) return fallback
  if (adjusted <= 0) return others[0].priority
  if (adjusted >= others.length) return others[others.length - 1].priority
  return others[adjusted - 1].priority
}

export function priorityBadge(priority: number, priorities: PriorityInfo[]) {
  const p = priorities.find((pr) => pr.value === priority)
  const label = p?.label ?? `P${priority}`
  const className =
    priority <= 0
      ? 'priority-critical'
      : priority <= 1
        ? 'priority-high'
        : priority <= 2
          ? 'priority-medium'
          : 'priority-low'
  return <span className={`priority-badge ${className}`}>{label}</span>
}
