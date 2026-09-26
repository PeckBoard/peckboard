import { useEffect, useState } from 'react'
import { useUiStore } from '../store/ui'
import { EMPTY_BACKGROUND_TASKS, useBackgroundStore } from '../store/background'

/**
 * State for one session's Background Tasks panel: the task list (fetched on
 * mount and after every reconnect, then kept live by the `background_task`
 * WS event), whether the panel is open, and which task's output is shown.
 * Also honours focus requests from the chat's completion notices.
 */
export function useBackgroundPanel(sessionId: string) {
  const tasks = useBackgroundStore((s) => s.tasksBySession[sessionId] ?? EMPTY_BACKGROUND_TASKS)
  const fetchTasks = useBackgroundStore((s) => s.fetchTasks)
  const focusRequest = useBackgroundStore((s) => s.focusRequest)
  const connected = useUiStore((s) => s.connected)
  const [open, setOpen] = useState(false)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  // Seeded with whatever request is already pending, so remounting a pane
  // doesn't replay a stale click.
  const [handledNonce, setHandledNonce] = useState<number | null>(
    () => useBackgroundStore.getState().focusRequest?.nonce ?? null,
  )

  // Refetch on (re)connect too: WS frames missed while disconnected have no
  // replay log, so the REST list is the source of truth after a gap.
  useEffect(() => {
    void fetchTasks(sessionId)
  }, [sessionId, connected, fetchTasks])

  if (focusRequest && focusRequest.sessionId === sessionId && focusRequest.nonce !== handledNonce) {
    // Adjust state during render (React's pattern for reacting to a changed
    // input) rather than in an effect.
    setHandledNonce(focusRequest.nonce)
    setOpen(true)
    setSelectedId(focusRequest.taskId)
  }

  const running = tasks.filter((t) => t.status === 'running').length
  return { tasks, running, open, setOpen, selectedId, setSelectedId }
}
