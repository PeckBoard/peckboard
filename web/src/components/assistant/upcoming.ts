import type { RepeatingTask } from '../../types/api'

/** The Assistant's own scheduled tasks, soonest first; paused repeating ones
 *  at the end (so they can be resumed). Spent one-offs are gone. */
export function upcomingAssistantTasks(tasks: RepeatingTask[]): RepeatingTask[] {
  const mine = tasks.filter(
    (t) =>
      t.target === 'assistant' && (t.enabled ? t.next_run_at !== null : t.schedule_kind !== 'once'),
  )
  const at = (t: RepeatingTask) =>
    t.enabled && t.next_run_at ? Date.parse(t.next_run_at) : Number.MAX_SAFE_INTEGER
  return mine.sort((a, b) => at(a) - at(b) || a.name.localeCompare(b.name))
}
