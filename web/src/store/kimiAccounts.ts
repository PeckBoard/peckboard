import { create } from 'zustand'
import type { KimiAccount, KimiAccountInput, KimiLoginStart } from '../types/api'
import { authedFetch } from './auth'
import { useResourcesStore } from './resources'

interface KimiAccountsState {
  accounts: KimiAccount[]
  loaded: boolean
  loading: boolean
  error: string | null
  fetchAccounts: () => Promise<void>
  createAccount: (input: KimiAccountInput) => Promise<KimiAccount>
  updateAccount: (id: string, input: KimiAccountInput) => Promise<void>
  deleteAccount: (id: string) => Promise<void>
  /** Begin a device login for an account; returns the sign-in URL to open. */
  startLogin: (id: string) => Promise<KimiLoginStart>
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

export const useKimiAccountsStore = create<KimiAccountsState>((set, get) => ({
  accounts: [],
  loaded: false,
  loading: false,
  error: null,

  fetchAccounts: async () => {
    set({ loading: true })
    try {
      const res = await authedFetch('/api/kimi-accounts')
      if (!res.ok) {
        set({ error: await errorFrom(res, 'Failed to load accounts'), loading: false })
        return
      }
      const accounts = (await res.json()) as KimiAccount[]
      set({ accounts, loaded: true, loading: false, error: null })
    } catch {
      set({ error: 'Failed to load accounts', loading: false })
    }
  },

  createAccount: async (input) => {
    const res = await authedFetch('/api/kimi-accounts', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to add account'))
    const account = (await res.json()) as KimiAccount
    await get().fetchAccounts()
    refreshModels()
    return account
  },

  updateAccount: async (id, input) => {
    const res = await authedFetch(`/api/kimi-accounts/${id}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to update account'))
    await get().fetchAccounts()
    refreshModels()
  },

  deleteAccount: async (id) => {
    const res = await authedFetch(`/api/kimi-accounts/${id}`, { method: 'DELETE' })
    if (!res.ok && res.status !== 204) {
      throw new Error(await errorFrom(res, 'Failed to delete account'))
    }
    await get().fetchAccounts()
    refreshModels()
  },

  startLogin: async (id) => {
    const res = await authedFetch(`/api/kimi-accounts/${id}/login/start`, { method: 'POST' })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to start Kimi login'))
    return (await res.json()) as KimiLoginStart
  },
}))
