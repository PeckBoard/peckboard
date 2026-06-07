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
      authedFetch('/api/sessions')
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

      // Sessions
      const sessions = Array.isArray(sessionsData)
        ? sessionsData
        : (sessionsData?.sessions ?? sessionsData ?? [])
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
 */
export function filterMentions(items: MentionItem[], query: string, limit = 10): MentionItem[] {
  const q = query.toLowerCase()
  return items
    .filter((m) => m.label.toLowerCase().includes(q) || m.detail.toLowerCase().includes(q))
    .slice(0, limit)
}
