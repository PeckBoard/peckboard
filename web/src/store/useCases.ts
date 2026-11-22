import { useEffect } from 'react'
import { create } from 'zustand'
import { authedFetch } from './auth'

/** What a user said they'll use PeckBoard for (setup wizard, first step).
 *  Ids match the server's `USE_CASES` in `src/routes/me.rs`. */
export type UseCase = 'coding' | 'writing' | 'ops' | 'printing' | 'other'

/** Every use case, in display order, with its chip label. */
export const USE_CASE_OPTIONS: { id: UseCase; label: string }[] = [
  { id: 'coding', label: 'Coding' },
  { id: 'writing', label: 'Writing' },
  { id: 'ops', label: 'Servers & ops' },
  { id: 'printing', label: '3D printing' },
  { id: 'other', label: 'Other' },
]

interface UseCasesState {
  useCases: UseCase[]
  /** True once `GET /api/me/use-cases` has answered (even with `[]`). */
  loaded: boolean
  load: () => Promise<void>
  /** Replace the signed-in user's answer. Optimistic; throws on failure. */
  save: (useCases: UseCase[]) => Promise<void>
}

let inflight: Promise<void> | null = null
/** Bumped by every save, so a load that started earlier can't clobber it. */
let saveCount = 0
/** Per-user use cases, stored server-side (`/api/me/use-cases`) so they
 *  follow the user across browsers. Read with [`useUseCases`]. */
export const useUseCasesStore = create<UseCasesState>((set) => ({
  useCases: [],
  loaded: false,
  load: () => {
    // Several components mount the hook at once; share one request.
    inflight ??= (async () => {
      const saves = saveCount
      try {
        const res = await authedFetch('/api/me/use-cases')
        if (!res.ok) return
        const body = (await res.json()) as { use_cases?: string[] }
        // A toggle made while this was in flight is newer: keep it.
        if (saveCount !== saves) return
        const known = new Set(USE_CASE_OPTIONS.map((o) => o.id as string))
        set({
          useCases: (body.use_cases ?? []).filter((u): u is UseCase => known.has(u)),
          loaded: true,
        })
      } catch {
        /* offline: keep what we have */
      } finally {
        inflight = null
      }
    })()
    return inflight
  },
  save: async (useCases) => {
    saveCount++
    set({ useCases, loaded: true })
    const res = await authedFetch('/api/me/use-cases', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ use_cases: useCases }),
    })
    if (!res.ok) {
      const data = (await res.json().catch(() => null)) as { error?: string } | null
      throw new Error(data?.error || `HTTP ${res.status}`)
    }
  },
}))

/** The signed-in user's use cases (loaded on first use). Use it to put the
 *  content that matches them first — e.g. starter prompts, the Welcome
 *  page's capability gallery. Empty = never answered: keep default order. */
export function useUseCases(): UseCase[] {
  const useCases = useUseCasesStore((s) => s.useCases)
  const loaded = useUseCasesStore((s) => s.loaded)
  useEffect(() => {
    if (!loaded) void useUseCasesStore.getState().load()
  }, [loaded])
  return useCases
}

/** Stable sort: items tagged with more of `useCases` come first; ties and
 *  an empty `useCases` keep the original order. */
export function orderByUseCases<T>(
  items: T[],
  tagsOf: (item: T) => readonly UseCase[],
  useCases: readonly UseCase[],
): T[] {
  if (useCases.length === 0) return items
  const score = (item: T) => tagsOf(item).filter((t) => useCases.includes(t)).length
  return items
    .map((item, i) => ({ item, i, s: score(item) }))
    .sort((a, b) => b.s - a.s || a.i - b.i)
    .map((x) => x.item)
}
