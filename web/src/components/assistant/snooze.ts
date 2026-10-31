import { useAssistantActivity } from '../../store/assistantActivity'
import { useRepeatingTasksStore } from '../../store/repeatingTasks'

/** "Ask me later" choices for a relayed question, in minutes. */
export const SNOOZE_CHOICES: { label: string; minutes: number }[] = [
  { label: 'Ask me in 10 minutes', minutes: 10 },
  { label: 'Ask me in 1 hour', minutes: 60 },
]

/** Snooze a relayed question (a one-off Assistant task raises it again),
 *  then refresh the Upcoming list it lands in. → an error message or `null`. */
export async function snoozeQuestion(
  sessionId: string,
  questionId: string,
  minutes: number,
): Promise<string | null> {
  const err = await useAssistantActivity.getState().snooze(sessionId, questionId, minutes)
  if (!err)
    void useRepeatingTasksStore
      .getState()
      .fetchTasks()
      .catch(() => undefined)
  return err
}
