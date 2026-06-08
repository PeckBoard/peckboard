import type { CardReport, PendingQuestion } from '../../store/projects'
import type { Event } from '../../types/api'

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
