import { create } from 'zustand'
import { authedFetch } from './auth'

/** `GET /api/voice/settings` — the Assistant's admin settings
 *  (`service::assistant_settings`). */
export interface AssistantSettings {
  auto_show_on_topic: boolean
  memory_enabled: boolean
  retention_hours: number
  max_entries: number
  auto_clear: 'after_topic' | 'off'
  clear_delay_minutes: number
  idle_topic_end_minutes: number
}

/** Inclusive ranges the server enforces (mirrors `assistant_settings.rs`). */
export const ASSISTANT_SETTING_RANGES = {
  retention_hours: [1, 168],
  max_entries: [5, 200],
  clear_delay_minutes: [0, 60],
  idle_topic_end_minutes: [5, 240],
} as const satisfies Partial<Record<keyof AssistantSettings, readonly [number, number]>>

interface AssistantSettingsState {
  settings: AssistantSettings | null
  error: string | null
  load: () => Promise<void>
  /** PUT only the changed keys: the server lays them over what it has, so
   *  two Settings sections never overwrite each other's fields. */
  save: (patch: Partial<AssistantSettings>) => Promise<boolean>
}

async function errorOf(res: Response, fallback: string): Promise<string> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  return body?.error ?? `${fallback} (${res.status})`
}

/** One shared copy of the Assistant settings for every Settings section. */
export const useAssistantSettings = create<AssistantSettingsState>((set, get) => ({
  settings: null,
  error: null,

  load: async () => {
    try {
      const res = await authedFetch('/api/voice/settings')
      if (!res.ok) throw new Error(await errorOf(res, "Couldn't load"))
      set({ settings: (await res.json()) as AssistantSettings, error: null })
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) })
    }
  },

  save: async (patch) => {
    const prev = get().settings
    if (prev) set({ settings: { ...prev, ...patch }, error: null })
    try {
      const res = await authedFetch('/api/voice/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patch),
      })
      if (!res.ok) throw new Error(await errorOf(res, "Couldn't save"))
      set({ settings: (await res.json()) as AssistantSettings, error: null })
      return true
    } catch (e) {
      set({ settings: prev, error: e instanceof Error ? e.message : String(e) })
      return false
    }
  },
}))
