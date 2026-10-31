import { useMemo } from 'react'
import { useAssistantActivity, type HeldRelay } from '../../store/assistantActivity'
import { useSessionsStore } from '../../store/sessions'
import { useVoiceStore } from '../../store/voice'
import { MenuButton, type MenuItem } from '../Dropdown'
import { describeTool, toolInFlight } from './assistantToolWords'
import { openInApp } from './openInApp'
import { SNOOZE_CHOICES, snoozeQuestion } from './snooze'

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`
}
function heldItem(h: HeldRelay): MenuItem {
  const base = { label: h.summary || h.kind, hint: h.source_name }
  if (h.kind !== 'question' || !h.source_session_id || !h.question_id) return base
  const sessionId = h.source_session_id
  const questionId = h.question_id
  return {
    ...base,
    testId: `assistant-queue-item-${h.id}`,
    submenu: SNOOZE_CHOICES.map((c) => ({
      label: c.label,
      testId: `assistant-snooze-${c.minutes}`,
      onSelect: () => void snoozeQuestion(sessionId, questionId, c.minutes),
    })),
  }
}

/**
 * The strip under the status: the tool the Assistant is running right now
 * in plain words, the sessions working on its behalf, and the updates held
 * back so it stays on one topic (tap to hear the next one).
 */
export default function AssistantActivity() {
  const events = useVoiceStore((s) => s.events)
  const sessions = useSessionsStore((s) => s.sessions)
  const queue = useAssistantActivity((s) => s.queue)
  const linked = useAssistantActivity((s) => s.linked)
  const busy = useAssistantActivity((s) => s.busy)
  const error = useAssistantActivity((s) => s.error)

  const tool = useMemo(() => toolInFlight(events), [events])
  const toolText = useMemo(
    () =>
      tool ? describeTool(tool, (id) => sessions.find((s) => s.id === id)?.name ?? null) : null,
    [tool, sessions],
  )

  const linkedItems = useMemo<MenuItem[]>(
    () =>
      linked.map((s) => ({
        label: s.name,
        hint: s.state === 'working' ? 'working' : 'needs an answer',
        testId: `assistant-linked-${s.id}`,
        onSelect: () => openInApp('session', s.id),
      })),
    [linked],
  )
  const queueItems = useMemo(() => queue.map(heldItem), [queue])

  if (!toolText && linked.length === 0 && queue.length === 0 && !error) return null
  return (
    <div className="assistant-activity" data-testid="assistant-activity">
      {toolText && (
        <span
          className="assistant-chip assistant-tool-chip"
          data-testid="assistant-tool-activity"
          role="status"
          aria-live="polite"
        >
          <span className="assistant-chip-spinner" aria-hidden />
          {toolText}
        </span>
      )}
      {linked.length > 0 && (
        <MenuButton
          items={linkedItems}
          ariaLabel="Sessions working for you"
          triggerClassName="assistant-chip assistant-chip-button"
          testId="assistant-linked-badge"
          align="left"
        >
          <span className="assistant-chip-count">{linked.length}</span>
          {linked.length === 1 ? 'session working for you' : 'sessions working for you'}
        </MenuButton>
      )}
      {queue.length > 0 && (
        <span className="assistant-chip-group">
          <button
            type="button"
            className="assistant-chip assistant-chip-button assistant-queue-badge"
            data-testid="assistant-queue-badge"
            title="Hear the next one now"
            disabled={busy}
            onClick={() => void useAssistantActivity.getState().releaseNext()}
          >
            {plural(queue.length, 'update waiting', 'updates waiting')}
          </button>
          <MenuButton
            items={queueItems}
            ariaLabel="What is waiting"
            triggerClassName="assistant-chip assistant-chip-button assistant-chip-more"
            testId="assistant-queue-menu"
            align="left"
          >
            <span aria-hidden>▾</span>
          </MenuButton>
        </span>
      )}
      {error && (
        <span className="assistant-chip assistant-chip-error" role="alert">
          {error}
          <button
            type="button"
            className="assistant-chip-dismiss"
            aria-label="Dismiss"
            onClick={() => useAssistantActivity.getState().clearError()}
          >
            ×
          </button>
        </span>
      )}
    </div>
  )
}
