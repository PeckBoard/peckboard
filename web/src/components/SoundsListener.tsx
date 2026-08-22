import { useEffect } from 'react'
import { useWsStore } from '../store/ws'
import { useClaudeAccountsStore } from '../store/claudeAccounts'
import { useGrokAccountsStore } from '../store/grokAccounts'
import { useKimiAccountsStore } from '../store/kimiAccounts'
import type { Event as SessionEvent, WarnLevel, PlanUsageMap } from '../types/api'
import { isLiveEventTs, playSound, unlockSounds, type SoundKind } from '../util/sounds'

const LIMIT_RE = /rate.?limit|usage.?limit|over.?quota|quota.?exceed|budget/i

function exceededIds(accounts: Array<{ id: string; usage?: { level?: WarnLevel } }>): Set<string> {
  const out = new Set<string>()
  for (const a of accounts) {
    if (a.usage?.level === 'exceeded') out.add(a.id)
  }
  return out
}

function cappedPlanKeys(plan: PlanUsageMap): Set<string> {
  const out = new Set<string>()
  for (const [id, entry] of Object.entries(plan)) {
    const buckets = entry.usage
    if (!buckets) continue
    for (const b of Object.values(buckets)) {
      if (b && b.utilization >= 1) out.add(`${id}:${b.resets_at ?? 'cap'}`)
    }
  }
  return out
}

function onEvent(event: SessionEvent) {
  if (!isLiveEventTs(event.ts)) return
  const kind = event.kind
  if (kind === 'question') playSound('question')
  else if (kind === 'agent-end') {
    const status = (event.data.status as string) ?? ''
    if (status === 'complete') playSound('runComplete')
    const err = `${event.data.error ?? ''} ${event.data.stderr ?? ''}`
    if (LIMIT_RE.test(err)) playSound('accountLimit')
  } else if (kind === 'agent-start') playSound('runStart')
  else if (kind === 'agent-tool-end') playSound('toolUsed')
}

/**
 * App-wide sound dispatcher. Lives outside any one view so a question on
 * a background session still chimes. History after WS resume is skipped
 * via `isLiveEventTs`.
 */
export default function SoundsListener() {
  useEffect(() => {
    const unlock = () => unlockSounds()
    window.addEventListener('pointerdown', unlock, { once: true })
    window.addEventListener('keydown', unlock, { once: true })
    return () => {
      window.removeEventListener('pointerdown', unlock)
      window.removeEventListener('keydown', unlock)
    }
  }, [])

  useEffect(() => {
    const ws = useWsStore.getState()
    ws.addEventListener(onEvent)
    return () => ws.removeEventListener(onEvent)
  }, [])

  useEffect(() => {
    const onQueue = (e: globalThis.Event) => {
      const detail = (e as CustomEvent).detail as { data?: { action?: string } } | undefined
      if (detail?.data?.action === 'drained') playSound('queueProcessed')
    }
    const onQuestion: Array<[string, SoundKind]> = [
      ['peckboard:worker-question', 'question'],
      ['peckboard:askpass-request', 'question'],
      ['peckboard:env-unlock-request', 'question'],
    ]
    window.addEventListener('peckboard:queue', onQueue as EventListener)
    const wrappers = onQuestion.map(([name, kind]) => {
      const fn = () => playSound(kind)
      window.addEventListener(name, fn)
      return [name, fn] as const
    })
    const onProject = (e: globalThis.Event) => {
      const project = (e as CustomEvent).detail?.data?.project as
        | { pause_reason?: string | null }
        | undefined
      const reason = project?.pause_reason ?? ''
      if (reason.startsWith('budget:')) playSound('accountLimit')
    }
    window.addEventListener('peckboard:project-update', onProject as EventListener)
    return () => {
      window.removeEventListener('peckboard:queue', onQueue as EventListener)
      window.removeEventListener('peckboard:project-update', onProject as EventListener)
      for (const [name, fn] of wrappers) window.removeEventListener(name, fn)
    }
  }, [])

  useEffect(() => {
    let claudeSeen: Set<string> | null = null
    let grokSeen: Set<string> | null = null
    let kimiSeen: Set<string> | null = null
    let planSeen: Set<string> | null = null

    const watch = (prev: Set<string> | null, next: Set<string>): Set<string> => {
      if (prev === null) return next
      for (const id of next) {
        if (!prev.has(id)) {
          playSound('accountLimit')
          break
        }
      }
      return next
    }

    const unsubC = useClaudeAccountsStore.subscribe((s) => {
      claudeSeen = watch(claudeSeen, exceededIds(s.accounts))
      planSeen = watch(planSeen, cappedPlanKeys(s.planUsage))
    })
    const unsubG = useGrokAccountsStore.subscribe((s) => {
      grokSeen = watch(grokSeen, exceededIds(s.accounts))
    })
    const unsubK = useKimiAccountsStore.subscribe((s) => {
      kimiSeen = watch(kimiSeen, exceededIds(s.accounts))
    })
    return () => {
      unsubC()
      unsubG()
      unsubK()
    }
  }, [])

  return null
}
