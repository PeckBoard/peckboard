import { create } from 'zustand'
import { authedFetch } from './auth'

export interface FileStatus {
  path: string
  status: string
}

export interface Commit {
  hash: string
  short_hash: string
  author: string
  date: string
  message: string
}

export interface CommitDetail {
  hash: string
  author: string
  date: string
  message: string
  diff: string
}

export interface DiscoveredRepo {
  name: string
  path: string
  folder_id: string
  folder_name: string
}

interface GitState {
  discoveredRepos: DiscoveredRepo[]
  loadingRepos: boolean
  fileStatuses: FileStatus[]
  commits: Commit[]
  fetchedPath: string | null
  loadingStatus: boolean
  statusError: string
  fetchRepos: () => Promise<void>
  fetchStatus: (path: string) => Promise<void>
  resetStatus: () => void
}

export const useGitStore = create<GitState>((set) => ({
  discoveredRepos: [],
  loadingRepos: false,
  fileStatuses: [],
  commits: [],
  fetchedPath: null,
  loadingStatus: false,
  statusError: '',

  fetchRepos: async () => {
    set({ loadingRepos: true })
    try {
      const res = await authedFetch('/api/git/repos')
      if (res.ok) {
        const repos: DiscoveredRepo[] = await res.json()
        set({ discoveredRepos: repos })
      }
    } catch {
      /* ignore scan errors */
    } finally {
      set({ loadingRepos: false })
    }
  },

  fetchStatus: async (path: string) => {
    set({ loadingStatus: true, statusError: '', fetchedPath: null })
    try {
      const [statusRes, commitsRes] = await Promise.all([
        authedFetch(`/api/git/status?path=${encodeURIComponent(path)}`),
        authedFetch(`/api/git/commits?path=${encodeURIComponent(path)}&limit=20`),
      ])
      if (!statusRes.ok) {
        const data = await statusRes.json().catch(() => ({ error: 'Failed to fetch git status' }))
        throw new Error(data.error || 'Failed to fetch git status')
      }
      if (!commitsRes.ok) {
        const data = await commitsRes.json().catch(() => ({ error: 'Failed to fetch commits' }))
        throw new Error(data.error || 'Failed to fetch commits')
      }
      const statusData: FileStatus[] = await statusRes.json()
      const commitsData: Commit[] = await commitsRes.json()
      set({
        fileStatuses: statusData,
        commits: commitsData,
        fetchedPath: path,
      })
    } catch (err) {
      set({
        statusError: err instanceof Error ? err.message : 'Failed to fetch git data',
      })
    } finally {
      set({ loadingStatus: false })
    }
  },

  resetStatus: () =>
    set({
      fileStatuses: [],
      commits: [],
      fetchedPath: null,
      statusError: '',
    }),
}))
