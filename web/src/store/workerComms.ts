import { create } from 'zustand'
import { authedFetch } from './auth'
import type { Event } from '../types/api'

export interface WorkerInfo {
  session_id: string
  name: string
  card_title: string | null
  step: string | null
}

export interface CommMessage {
  id: string
  ts: number
  from_session: string
  from_name: string
  to_session: string | null
  to_name: string | null
  type: 'message' | 'finding' | 'auto-notify' | 'notification'
  text: string
}

interface WorkerCommsState {
  workersByProject: Record<string, WorkerInfo[]>
  messagesByProject: Record<string, CommMessage[]>
  loadingByProject: Record<string, boolean>
  /** True when the project-wide fetch itself failed (not an individual
   *  worker's event scan) — previous data is kept, but the view must not
   *  read a stale/absent list as "no communications". */
  errorByProject: Record<string, boolean>
  /** Count of workers whose event-history scan failed this fetch, so a
   *  partial failure (some workers loaded, some didn't) is visible instead
   *  of silently dropping their messages. */
  unreachableByProject: Record<string, number>
  fetchComms: (projectId: string) => Promise<void>
}

interface CardLite {
  title: string
  step: string
  worker_session_id?: string | null
  last_worker_session_id?: string | null
}

export const useWorkerCommsStore = create<WorkerCommsState>((set) => ({
  workersByProject: {},
  messagesByProject: {},
  loadingByProject: {},
  errorByProject: {},
  unreachableByProject: {},

  fetchComms: async (projectId: string) => {
    set((s) => ({
      loadingByProject: { ...s.loadingByProject, [projectId]: true },
    }))
    try {
      let sessRes: Response
      try {
        sessRes = await authedFetch(`/api/projects/${projectId}`)
      } catch {
        set((s) => ({ errorByProject: { ...s.errorByProject, [projectId]: true } }))
        return
      }
      if (!sessRes.ok) {
        set((s) => ({ errorByProject: { ...s.errorByProject, [projectId]: true } }))
        return
      }
      const projData = await sessRes.json()
      const cards: CardLite[] = projData?.cards ?? []

      const workerMap: Record<string, WorkerInfo> = {}
      for (const c of cards) {
        for (const sid of [c.worker_session_id, c.last_worker_session_id]) {
          if (sid && !workerMap[sid]) {
            workerMap[sid] = {
              session_id: sid,
              name: `worker: ${c.title}`,
              card_title: c.title,
              step: c.step,
            }
          }
        }
      }

      const comms: CommMessage[] = []
      let unreachable = 0
      for (const w of Object.values(workerMap)) {
        try {
          // `?after_seq=0` opts into the unbounded WS-catchup mode of
          // the events route. The default fetch was capped at the
          // chat view's page size after pagination landed, which
          // silently dropped older worker-to-worker comms from this
          // aggregator. We need every event ≥ seq 1 because we filter
          // for `source: worker-*` user events scattered throughout
          // the timeline.
          const evRes = await authedFetch(`/api/sessions/${w.session_id}/events?after_seq=0`)
          if (!evRes.ok) {
            unreachable++
            continue
          }
          const events: Event[] = await evRes.json()
          for (const e of events) {
            if (e.kind !== 'user') continue
            const source = (e.data.source as string) ?? ''
            if (!source.startsWith('worker-')) continue

            const text = (e.data.text as string) ?? ''
            let fromName = 'Unknown'
            let fromSession = ''
            const toSession: string | null = w.session_id
            const toName: string | null = w.card_title ?? w.name

            const fromMatch = text.match(
              /\[(?:Worker message from|Shared finding from worker on|Auto\] Worker on) "([^"]+)"/,
            )
            if (fromMatch) fromName = fromMatch[1]

            const sessionMatch = text.match(/From session: ([a-f0-9-]+)/)
            if (sessionMatch) fromSession = sessionMatch[1]

            if (source === 'worker-auto-notify') {
              const autoMatch = text.match(/Worker on "([^"]+)" modified/)
              if (autoMatch) fromName = autoMatch[1]
              fromSession = ''
            }

            let type: CommMessage['type'] = 'message'
            if (source === 'worker-finding') type = 'finding'
            else if (source === 'worker-auto-notify') type = 'auto-notify'
            else if (source === 'worker-notification') type = 'notification'

            const displayText = text.length > 300 ? text.slice(0, 297) + '...' : text

            comms.push({
              id: e.id,
              ts: e.ts,
              from_session: fromSession,
              from_name: fromName,
              to_session: toSession,
              to_name: toName,
              type,
              text: displayText,
            })
          }
        } catch {
          unreachable++
        }
      }

      comms.sort((a, b) => a.ts - b.ts)

      set((s) => ({
        workersByProject: { ...s.workersByProject, [projectId]: Object.values(workerMap) },
        messagesByProject: { ...s.messagesByProject, [projectId]: comms },
        errorByProject: { ...s.errorByProject, [projectId]: false },
        unreachableByProject: { ...s.unreachableByProject, [projectId]: unreachable },
      }))
    } catch {
      set((s) => ({ errorByProject: { ...s.errorByProject, [projectId]: true } }))
    } finally {
      set((s) => ({
        loadingByProject: { ...s.loadingByProject, [projectId]: false },
      }))
    }
  },
}))
