import { create } from 'zustand'
import type { SshKey } from '../types/api'
import { authedFetch } from './auth'

/** Body for `POST /api/ssh-keys` — a pasted private key plus, if the key is
 *  passphrase-protected, the passphrase needed to open it. Both are sent
 *  once and sealed server-side; neither ever comes back. */
export interface SshKeyImportInput {
  name: string
  private_key: string
  passphrase?: string
}

/** Body for `POST /api/ssh-keys/generate`. */
export interface SshKeyGenerateInput {
  name: string
  key_type: string
}

interface SshKeysState {
  keys: SshKey[]
  loaded: boolean
  loading: boolean
  error: string | null
  fetchKeys: () => Promise<void>
  importKey: (input: SshKeyImportInput) => Promise<void>
  generateKey: (input: SshKeyGenerateInput) => Promise<void>
  renameKey: (id: string, name: string) => Promise<void>
  deleteKey: (id: string) => Promise<void>
  /** The public half, fetched on demand for the copy action. */
  fetchPublicKey: (id: string) => Promise<string>
  /** Surface a mutation failure in the section's inline error slot. */
  setError: (message: string | null) => void
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

/**
 * SSH key vault store. Mirrors `claudeAccounts.ts`: plain fetch/mutate
 * actions that refetch the list after a write. Keys don't broadcast over
 * the WebSocket (neither do env vars or accounts), so there's no
 * subscription to keep in sync.
 */
export const useSshKeysStore = create<SshKeysState>((set, get) => ({
  keys: [],
  loaded: false,
  loading: false,
  error: null,

  setError: (message) => set({ error: message }),

  fetchKeys: async () => {
    set({ loading: true })
    try {
      const res = await authedFetch('/api/ssh-keys')
      if (!res.ok) {
        set({ error: await errorFrom(res, 'Failed to load SSH keys'), loading: false })
        return
      }
      const body = (await res.json()) as { keys: SshKey[] }
      set({ keys: body.keys, loaded: true, loading: false, error: null })
    } catch {
      set({ error: 'Failed to load SSH keys', loading: false })
    }
  },

  importKey: async (input) => {
    const res = await authedFetch('/api/ssh-keys', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to import key'))
    await get().fetchKeys()
  },

  generateKey: async (input) => {
    const res = await authedFetch('/api/ssh-keys/generate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to generate key'))
    await get().fetchKeys()
  },

  renameKey: async (id, name) => {
    const res = await authedFetch(`/api/ssh-keys/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name }),
    })
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to rename key'))
    await get().fetchKeys()
  },

  deleteKey: async (id) => {
    const res = await authedFetch(`/api/ssh-keys/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    })
    if (!res.ok && res.status !== 204) {
      throw new Error(await errorFrom(res, 'Failed to delete key'))
    }
    await get().fetchKeys()
  },

  fetchPublicKey: async (id) => {
    const res = await authedFetch(`/api/ssh-keys/${encodeURIComponent(id)}/public`)
    if (!res.ok) throw new Error(await errorFrom(res, 'Failed to read public key'))
    const body = (await res.json()) as { public_key: string }
    return body.public_key
  },
}))
