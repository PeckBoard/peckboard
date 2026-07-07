import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'

export interface MentionItem {
  type: 'report' | 'session' | 'card'
  label: string
  detail: string
  ref: string
}

/**
 * Shared hook that fetches all mentionable items (reports, sessions, cards)
 * for @ autocomplete. Used by InputBar and KanbanBoard question answers.
 */
export function useMentions(excludeSessionId?: string): MentionItem[] {
  const [items, setItems] = useState<MentionItem[]>([])

  useEffect(() => {
    Promise.all([
      authedFetch('/api/reports')
        .then((r) => (r.ok ? r.json() : null))
        .catch(() => null),
      // The session list is paginated, but autocomplete needs the
      // most-recently-active sessions — and exactly those — so a
      // first-page fetch matches the autocomplete intent. A user with
      // 10k stale sessions doesn't want every one in the @-menu.
      //
      // 500 is the server's `MAX_SESSION_PAGE_SIZE`; passing more would
      // be clamped silently. A user with more than 500 active sessions
      // would still find recent ones via @-search prefix matching but
      // miss the long tail — accept that tradeoff over walking the
      // cursor on every keystroke.
      authedFetch('/api/sessions?limit=500')
        .then((r) => (r.ok ? r.json() : null))
        .catch(() => null),
      authedFetch('/api/projects')
        .then((r) => (r.ok ? r.json() : null))
        .catch(() => null),
    ]).then(async ([reportsData, sessionsData, projectsData]) => {
      const result: MentionItem[] = []

      // Reports
      const reports = Array.isArray(reportsData) ? reportsData : (reportsData?.reports ?? [])
      for (const r of reports) {
        result.push({
          type: 'report',
          label: r.title || r.file,
          detail: `${r.folder}/${r.file}`,
          ref: `[report:${r.folder}/${r.file}]`,
        })
      }

      // Sessions: the endpoint returns `{items, next_cursor}` after
      // pagination landed; the older Array.isArray branch is the
      // compatibility path for the experts endpoint format-mirroring
      // and the legacy server response shape we still see in stale
      // browser tabs mid-deploy.
      const sessions = Array.isArray(sessionsData)
        ? sessionsData
        : (sessionsData?.items ?? sessionsData?.sessions ?? [])
      for (const s of sessions) {
        if (s.id === excludeSessionId) continue
        result.push({
          type: 'session',
          label: s.name || 'Untitled',
          detail: s.id,
          ref: `[session:${s.id}]`,
        })
      }

      // Cards from all projects
      const projects = Array.isArray(projectsData)
        ? projectsData
        : (projectsData?.projects ?? projectsData ?? [])
      for (const p of projects) {
        try {
          const cardsRes = await authedFetch(`/api/projects/${p.id}/cards`)
          if (!cardsRes.ok) continue
          const cardsData = await cardsRes.json()
          const cards = Array.isArray(cardsData) ? cardsData : (cardsData?.cards ?? cardsData ?? [])
          for (const c of cards) {
            const sid = c.worker_session_id || c.last_worker_session_id
            if (sid) {
              result.push({
                type: 'card',
                label: c.title,
                detail: `${p.name} — ${c.step}`,
                ref: `[session:${sid}]`,
              })
            }
          }
        } catch {
          /* ignore */
        }
      }

      setItems(result)
    })
  }, [excludeSessionId])

  return items
}

/**
 * Filter mentions by a search string.
 *
 * The dropdown is scrollable (`max-height` + `overflow-y: auto`), so this cap
 * only bounds the DOM nodes rendered per keystroke — it is not meant to hide
 * results. 10 was too aggressive: with reports listed first, a bare `@` never
 * reached sessions/cards once there were 10+ reports. 50 surfaces the full set
 * in realistic use while still guarding the pathological 500-session case.
 */
export function filterMentions(items: MentionItem[], query: string, limit = 50): MentionItem[] {
  const q = query.toLowerCase()
  return items
    .filter((m) => m.label.toLowerCase().includes(q) || m.detail.toLowerCase().includes(q))
    .slice(0, limit)
}
