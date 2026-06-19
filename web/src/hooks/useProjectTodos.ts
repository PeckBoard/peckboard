import { useEffect, useMemo, useState } from 'react'
import { useWsStore } from '../store/ws'
import { authedFetch } from '../store/auth'
import { latestTodoSnapshot, parseTodoItems, type TodoItem } from '../types/todo'
import type { Card } from '../types/api'

/**
 * Aggregate the latest todo snapshot for every card's worker session, keyed by
 * card id. Cards with no todos are omitted so the project view stays
 * uncluttered.
 *
 * Two sources feed each card's snapshot, newest wins:
 *  - Live `todo` events streaming over the WS into the ws store (authoritative
 *    once any arrive, since TodoWrite is replace-all).
 *  - A load-time GET /api/sessions/:id/todos fetch, so a freshly opened project
 *    shows todos a worker reported before this client subscribed (the WS only
 *    replays from the last seen seq, which a cold load doesn't have).
 *
 * A card's session is its active `worker_session_id`, falling back to
 * `last_worker_session_id` so a snapshot survives the worker completing a chunk
 * (the orchestrator clears `worker_session_id` between dispatches).
 */
export function useProjectTodos(cards: Card[]): Record<string, TodoItem[]> {
  const eventsBySession = useWsStore((s) => s.eventsBySession)
  // sessionId -> snapshot fetched at load time.
  const [fetched, setFetched] = useState<Record<string, TodoItem[]>>({})

  const sessionByCard = useMemo(() => {
    const map: Record<string, string> = {}
    for (const c of cards) {
      const sid = c.worker_session_id ?? c.last_worker_session_id
      if (sid) map[c.id] = sid
    }
    return map
  }, [cards])

  // Re-fetch whenever the set of relevant sessions changes. The sorted,
  // comma-joined key keeps this stable across renders that don't add/remove a
  // worker session.
  const sessionKey = useMemo(
    () => [...new Set(Object.values(sessionByCard))].sort().join(','),
    [sessionByCard],
  )

  useEffect(() => {
    const ids = sessionKey ? sessionKey.split(',') : []
    if (ids.length === 0) return
    let cancelled = false
    for (const sid of ids) {
      authedFetch(`/api/sessions/${sid}/todos`)
        .then((res) => (res.ok ? res.json() : null))
        .then((data) => {
          if (cancelled) return
          setFetched((prev) => ({ ...prev, [sid]: parseTodoItems(data?.todos) }))
        })
        .catch(() => {})
    }
    return () => {
      cancelled = true
    }
  }, [sessionKey])

  return useMemo(() => {
    const result: Record<string, TodoItem[]> = {}
    for (const [cardId, sid] of Object.entries(sessionByCard)) {
      const live = latestTodoSnapshot(eventsBySession[sid] ?? [])
      const todos = live ?? fetched[sid] ?? []
      if (todos.length > 0) result[cardId] = todos
    }
    return result
  }, [sessionByCard, eventsBySession, fetched])
}
