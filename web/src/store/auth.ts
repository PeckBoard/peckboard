import { create } from 'zustand'

const TOKEN_KEY = 'peckboard_token'

function getToken(): string | null {
  // Check localStorage first (remember-me), then sessionStorage
  return localStorage.getItem(TOKEN_KEY) ?? sessionStorage.getItem(TOKEN_KEY)
}

/** Which storage currently holds the token, so we can refresh in-place. */
function getRememberMe(): boolean {
  return localStorage.getItem(TOKEN_KEY) !== null
}

function setToken(token: string, rememberMe: boolean): void {
  if (rememberMe) {
    localStorage.setItem(TOKEN_KEY, token)
    sessionStorage.removeItem(TOKEN_KEY)
  } else {
    sessionStorage.setItem(TOKEN_KEY, token)
    localStorage.removeItem(TOKEN_KEY)
  }
}

function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY)
  sessionStorage.removeItem(TOKEN_KEY)
}

/** Fetch wrapper that attaches JWT and handles 401 by clearing auth state. */
export async function authedFetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
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
  user: { id: string; username: string; role: string } | null
  clearAuth: () => void
  checkAuth: () => Promise<void>
  login: (username: string, password: string, rememberMe?: boolean) => Promise<void>
  logout: () => Promise<void>
  changePassword: (currentPassword: string, newPassword: string) => Promise<void>
}

export const useAuthStore = create<AuthState>((set) => ({
  initialized: false,
  authenticated: false,
  user: null,

  clearAuth: () => set({ authenticated: false, user: null }),

  checkAuth: async () => {
    try {
      // Self-service registration is gone; the server auto-creates the
      // sole admin on first start. We only need to probe the token.
      const token = getToken()
      if (token) {
        const meRes = await authedFetch('/api/auth/me')
        if (meRes.ok) {
          const user = await meRes.json()
          set({ authenticated: true, user, initialized: true })
          return
        }
      }

      set({ authenticated: false, user: null, initialized: true })
    } catch {
      set({ authenticated: false, user: null, initialized: true })
    }
  },

  login: async (username, password, rememberMe = true) => {
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
    setToken(data.token, rememberMe)
    set({ authenticated: true, user: data.user })
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

  changePassword: async (currentPassword, newPassword) => {
    const res = await authedFetch('/api/auth/change-password', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        current_password: currentPassword,
        new_password: newPassword,
      }),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to change password' }))
      throw new Error(err.error || 'Failed to change password')
    }
    // Server revokes all sessions and mints a fresh token. Persist it
    // into the same storage tier the caller used originally so a logged-
    // in tab stays logged in across the change.
    const data = await res.json()
    setToken(data.token, getRememberMe())
    set({ authenticated: true, user: data.user })
  },
}))
