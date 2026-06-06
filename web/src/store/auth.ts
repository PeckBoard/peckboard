import { create } from 'zustand'

const TOKEN_KEY = 'peckboard_token'

function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY)
}

function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token)
}

function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY)
}

/** Fetch wrapper that attaches JWT and handles 401 by clearing auth state. */
export async function authedFetch(
  input: RequestInfo | URL,
  init?: RequestInit,
): Promise<Response> {
  const token = getToken()
  const headers = new Headers(init?.headers)
  if (token) {
    headers.set('Authorization', `Bearer ${token}`)
  }
  const res = await fetch(input, { ...init, headers })
  if (res.status === 401) {
    clearToken()
    useAuthStore.getState().clearAuth()
  }
  return res
}

interface AuthState {
  initialized: boolean
  authenticated: boolean
  needsRegistration: boolean
  user: { id: string; username: string; role: string } | null
  clearAuth: () => void
  checkAuth: () => Promise<void>
  login: (username: string, password: string) => Promise<void>
  logout: () => Promise<void>
  register: (username: string, password: string, email?: string) => Promise<void>
}

export const useAuthStore = create<AuthState>((set) => ({
  initialized: false,
  authenticated: false,
  needsRegistration: false,
  user: null,

  clearAuth: () => set({ authenticated: false, user: null }),

  checkAuth: async () => {
    try {
      // First check if any users exist
      const statusRes = await fetch('/api/auth/status')
      if (statusRes.ok) {
        const status = await statusRes.json()
        if (!status.has_users) {
          set({ needsRegistration: true, initialized: true, authenticated: false, user: null })
          return
        }
      }

      // If we have a token, try to validate it
      const token = getToken()
      if (token) {
        const meRes = await authedFetch('/api/auth/me')
        if (meRes.ok) {
          const user = await meRes.json()
          set({ authenticated: true, user, needsRegistration: false, initialized: true })
          return
        }
      }

      set({ authenticated: false, user: null, needsRegistration: false, initialized: true })
    } catch {
      set({ authenticated: false, user: null, initialized: true })
    }
  },

  login: async (username, password) => {
    const res = await fetch('/api/auth/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, password }),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Login failed' }))
      throw new Error(err.error || 'Login failed')
    }
    const data = await res.json()
    setToken(data.token)
    set({ authenticated: true, user: data.user, needsRegistration: false })
  },

  logout: async () => {
    try {
      await authedFetch('/api/auth/logout', { method: 'POST' })
    } catch {
      // ignore network errors on logout
    }
    clearToken()
    set({ authenticated: false, user: null })
  },

  register: async (username, password, email?) => {
    const body: Record<string, string> = { username, password }
    if (email) body.email = email
    const res = await fetch('/api/auth/register', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Registration failed' }))
      throw new Error(err.error || 'Registration failed')
    }
    const data = await res.json()
    setToken(data.token)
    set({ authenticated: true, user: data.user, needsRegistration: false })
  },
}))
