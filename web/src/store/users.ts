import { create } from 'zustand'
import { authedFetch } from './auth'

export interface UserRecord {
  id: string
  username: string
  email: string | null
  role: string
  created_at: string
}

interface UsersState {
  users: UserRecord[]
  loading: boolean
  error: string
  fetchUsers: () => Promise<void>
  createUser: (data: {
    username: string
    password: string
    role: string
    email?: string
  }) => Promise<void>
  deleteUser: (id: string) => Promise<void>
  clearError: () => void
}

export const useUsersStore = create<UsersState>((set, get) => ({
  users: [],
  loading: true,
  error: '',

  fetchUsers: async () => {
    set({ loading: true, error: '' })
    try {
      const res = await authedFetch('/api/users')
      if (!res.ok) {
        const data = await res.json().catch(() => ({ error: 'Failed to fetch users' }))
        throw new Error(data.error || 'Failed to fetch users')
      }
      const users: UserRecord[] = await res.json()
      set({ users, loading: false })
    } catch (err) {
      set({
        loading: false,
        error: err instanceof Error ? err.message : 'Failed to fetch users',
      })
    }
  },

  createUser: async (data) => {
    const body: Record<string, string> = {
      username: data.username,
      password: data.password,
      role: data.role,
    }
    if (data.email) body.email = data.email
    const res = await authedFetch('/api/users', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to create user' }))
      throw new Error(err.error || 'Failed to create user')
    }
    await get().fetchUsers()
  },

  deleteUser: async (id: string) => {
    const res = await authedFetch(`/api/users/${id}`, { method: 'DELETE' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to delete user' }))
      throw new Error(err.error || 'Failed to delete user')
    }
    await get().fetchUsers()
  },

  clearError: () => set({ error: '' }),
}))
