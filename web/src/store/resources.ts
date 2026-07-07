import { create } from 'zustand'
import { authedFetch } from './auth'

export interface WorkflowStepInfo {
  step: string
  instructions: string
}

export interface WorkflowInfo {
  id: string
  name: string
  description: string
  priority: number
  steps: (string | WorkflowStepInfo)[]
}
export interface ModelInfo {
  id: string
  display_name: string
  /** Capability tier within the provider (higher = more capable). Single-
   *  tier providers report 0. Only comparable within one provider+account. */
  tier?: number
}

/** One selectable reasoning-effort level, as served per-provider by
 *  `/api/models`. `id` is passed to the provider's `--effort` flag; `label`
 *  is shown in the effort picker. */
export interface EffortLevel {
  id: string
  label: string
}

export interface ProviderInfo {
  id: string
  display_name: string
  models: ModelInfo[]
  /** Effort levels this provider exposes. Empty ⇒ the provider has no
   *  effort control, so only the "Default" option is offered. */
  effort_levels?: EffortLevel[]
}

/** The always-present "Default" effort option (no override — the provider
 *  decides). Value is `''` so it round-trips as "no effort" everywhere. */
export const DEFAULT_EFFORT_OPTION = { value: '', label: 'Default' }

/**
 * Effort dropdown options for a given model id. Derives the provider from the
 * `provider:model` prefix (bare ids default to `claude`, matching the backend),
 * then returns "Default" followed by that provider's effort levels. This is how
 * the effort picker "loads effort levels from the provider" once a model is
 * chosen — Claude/Grok expose the full ladder, Cursor/Ollama/Mock only Default.
 */
export function effortOptionsForModel(
  modelId: string | null | undefined,
  providers: ProviderInfo[],
): { value: string; label: string }[] {
  const providerId = modelId && modelId.includes(':') ? modelId.split(':')[0] : 'claude'
  const provider = providers.find((p) => p.id === providerId)
  const levels = provider?.effort_levels ?? []
  return [DEFAULT_EFFORT_OPTION, ...levels.map((l) => ({ value: l.id, label: l.label }))]
}

interface ResourcesState {
  workflows: WorkflowInfo[]
  models: ModelInfo[]
  providers: ProviderInfo[]
  fetchWorkflows: () => Promise<void>
  fetchModels: () => Promise<void>
}

export const useResourcesStore = create<ResourcesState>((set) => ({
  workflows: [],
  models: [],
  providers: [],

  fetchWorkflows: async () => {
    try {
      const res = await authedFetch('/api/workflows')
      if (!res.ok) return
      const data = await res.json()
      if (data?.workflows) set({ workflows: data.workflows })
    } catch {
      /* ignore fetch errors — caller renders empty list */
    }
  },

  fetchModels: async () => {
    try {
      const res = await authedFetch('/api/models')
      if (!res.ok) return
      const data = await res.json()
      const patch: Partial<ResourcesState> = {}
      if (data?.models) patch.models = data.models
      if (data?.providers) patch.providers = data.providers
      if (Object.keys(patch).length > 0) set(patch)
    } catch {
      /* ignore */
    }
  },
}))
