import { create } from 'zustand'
import type { Project } from '../types/api'
import { authedFetch } from './auth'
import { useProjectsStore } from './projects'
import { useTabsStore } from './tabs'
/** Marker the server writes into the demo project's context
 *  (`DEMO_MARKER` in `src/routes/demo_project.rs`). */
const DEMO_MARKER = '[peckboard-demo-project]'

/** Is this the scripted demo project (not one the user made)? */
export function isDemoProject(p: Pick<Project, 'name' | 'context'>): boolean {
  return p.name.startsWith('Demo: ') && (p.context ?? '').includes(DEMO_MARKER)
}

/** The demo project (`/api/demo-project`): a scripted board that shows how
 *  Peckboard works without any AI account. The server owns what counts as
 *  the demo; this store just tracks it. */
interface DemoProjectState {
  demo: Project | null
  /** True once `fetchDemo` has answered, so the card doesn't offer
   *  "Start demo" while an existing demo is still loading. */
  loaded: boolean
  fetchDemo: () => Promise<void>
  /** Create the demo (or get the existing one) and open its board. */
  startDemo: () => Promise<Project>
  deleteDemo: () => Promise<void>
}

async function errorText(res: Response, fallback: string): Promise<string> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  return body?.error || fallback
}

export const useDemoProjectStore = create<DemoProjectState>((set, get) => ({
  demo: null,
  loaded: false,

  fetchDemo: async () => {
    try {
      const res = await authedFetch('/api/demo-project')
      if (!res.ok) return
      const demo = (await res.json()) as Project | null
      set({ demo, loaded: true })
    } catch {
      // Leave `loaded` false: a failed request can't claim there is no demo.
    }
  },

  startDemo: async () => {
    const res = await authedFetch('/api/demo-project', { method: 'POST' })
    if (!res.ok) throw new Error(await errorText(res, 'Couldn’t start the demo.'))
    const demo = (await res.json()) as Project
    set({ demo, loaded: true })
    useProjectsStore.setState((s) => ({
      projects: s.projects.some((p) => p.id === demo.id) ? s.projects : [...s.projects, demo],
    }))
    useProjectsStore.getState().setActiveProject(demo.id)
    useTabsStore.getState().openTab('project', demo.id)
    // Go to the board from wherever the card sits (Projects, Welcome): push
    // the route and let App's router follow, as in-app links do.
    const path = `/projects/${demo.id}`
    if (window.location.pathname !== path) window.history.pushState(null, '', path)
    window.dispatchEvent(new PopStateEvent('popstate'))
    return demo
  },

  deleteDemo: async () => {
    const id = get().demo?.id
    const res = await authedFetch('/api/demo-project', { method: 'DELETE' })
    if (!res.ok && res.status !== 404) {
      throw new Error(await errorText(res, 'Couldn’t delete the demo.'))
    }
    set({ demo: null, loaded: true })
    if (id) {
      useProjectsStore.setState((s) => ({
        projects: s.projects.filter((p) => p.id !== id),
        activeProjectId: s.activeProjectId === id ? null : s.activeProjectId,
        cards: s.activeProjectId === id ? [] : s.cards,
      }))
      useTabsStore.getState().removeTabsForItem('project', id)
    }
  },
}))
