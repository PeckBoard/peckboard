import type { RepeatingTask } from '../../types/api'

/** Whose tasks to show: an assistant's id, and whether it is the default one
 *  (which also owns every task that names no assistant). */
export interface TaskOwner {
  id: string
  isDefault: boolean
}

/** One assistant's own scheduled tasks, soonest first; paused repeating ones
 *  at the end (so they can be resumed). Spent one-offs are gone. Without an
 *  `owner` (the assistants haven't loaded yet), every assistant's tasks. */
export function upcomingAssistantTasks(
  tasks: RepeatingTask[],
  owner?: TaskOwner | null,
): RepeatingTask[] {
  const mine = tasks.filter(
    (t) =>
      t.target === 'assistant' &&
      (t.enabled ? t.next_run_at !== null : t.schedule_kind !== 'once') &&
      (!owner || (t.assistant_id ? t.assistant_id === owner.id : owner.isDefault)),
  )
  const at = (t: RepeatingTask) =>
    t.enabled && t.next_run_at ? Date.parse(t.next_run_at) : Number.MAX_SAFE_INTEGER
  return mine.sort((a, b) => at(a) - at(b) || a.name.localeCompare(b.name))
}
