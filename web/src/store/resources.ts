import { create } from 'zustand'
import { authedFetch } from './auth'

export interface WorkflowInfo {
  id: string
  name: string
  steps: string[]
}

export interface ModelInfo {
  id: string
  display_name: string
}

export interface ProviderInfo {
  id: string
  display_name: string
  models: ModelInfo[]
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
