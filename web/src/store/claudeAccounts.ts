import { create } from 'zustand'
import type {
  ClaudeAccount,
  ClaudeAccountInput,
  ClaudeLoginStart,
  PlanUsageMap,
} from '../types/api'
import { authedFetch } from './auth'
import { useResourcesStore } from './resources'

interface ClaudeAccountsState {
  accounts: ClaudeAccount[]
  loaded: boolean
  loading: boolean
  error: string | null
  /** Subscription plan usage per login (`default` = host login). */
  planUsage: PlanUsageMap
  planUsageRefreshing: boolean
  fetchAccounts: () => Promise<void>
  fetchPlanUsage: () => Promise<void>
  refreshPlanUsage: () => Promise<void>
  startLogin: () => Promise<ClaudeLoginStart>
  createAccount: (input: ClaudeAccountInput) => Promise<void>
  updateAccount: (id: string, input: ClaudeAccountInput) => Promise<void>
  deleteAccount: (id: string) => Promise<void>
}

/** Surface a `{ error }` JSON body (or a generic message) from a non-2xx. */
async function errorFrom(res: Response, fallback: string): Promise<string> {
  try {
    const body = await res.json()
    if (body && typeof body.error === 'string') return body.error
  } catch {
    /* non-JSON body */
  }
  return fallback
}

/** Any account mutation can change the model catalogue (each account adds a
 *  labelled variant per model), so re-pull `/api/models` after a write. */
function refreshModels() {
  void useResourcesStore.getState().fetchModels()
}

export const useClaudeAccountsStore = create<ClaudeAccountsState>((set, get) => ({
  accounts: [],
  loaded: false,
  loading: false,
  error: null,
  planUsage: {},
  planUsageRefreshing: false,

  fetchPlanUsage: async () => {
    try {
      const res = await authedFetch('/api/claude-accounts/plan-usage')
      if (!res.ok) return
      set({ planUsage: (await res.json()) as PlanUsageMap })
    } catch {
      /* cached data only — a failed poll is already surfaced per-entry */
    }
  },

  refreshPlanUsage: async () => {
    set({ planUsageRefreshing: true })
    try {
      const res = await authedFetch('/api/claude-accounts/plan-usage', { method: 'POST' })
      if (res.ok) set({ planUsage: (await res.json()) as PlanUsageMap })
    } catch {
      /* keep last snapshot */
    } finally {
      set({ planUsageRefreshing: false })
    }
  },

  fetchAccounts: async () => {
    set({ loading: true })
    try {
      const res = await authedFetch('/api/claude-accounts')
      if (!res.ok) {
        set({ error: await errorFrom(res, 'Failed to load accounts'), loading: false })
        return
      }
      const accounts = (await res.json()) as ClaudeAccount[]
      set({ accounts, loaded: true, loading: false, error: null })
    } catch {
      set({ error: 'Failed to load accounts', loading: false })
    }
  },

  startLogin: async () => {
    const res = await authedFetch('/api/claude-accounts/login/start', { method: 'POST' })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to start Claude login'))
    return (await res.json()) as ClaudeLoginStart
  },

  createAccount: async (input) => {
    const res = await authedFetch('/api/claude-accounts', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to add account'))
    await get().fetchAccounts()
    refreshModels()
  },

  updateAccount: async (id, input) => {
    const res = await authedFetch(`/api/claude-accounts/${id}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to update account'))
    await get().fetchAccounts()
    refreshModels()
  },

  deleteAccount: async (id) => {
    const res = await authedFetch(`/api/claude-accounts/${id}`, { method: 'DELETE' })
    if (!res.ok && res.status !== 204) {
      throw new Error(await errorFrom(res, 'Failed to delete account'))
    }
    await get().fetchAccounts()
    refreshModels()
  },
}))
