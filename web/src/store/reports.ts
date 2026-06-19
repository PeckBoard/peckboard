import { create } from 'zustand'
import { authedFetch } from './auth'

export interface ReportEntry {
  folder: string
  file: string
  title: string
  date: string
  sessionId?: string
  projectName?: string
}

interface ReportsState {
  reports: ReportEntry[]
  loading: boolean
  error: string
  fetchReports: () => Promise<void>
}

export const useReportsStore = create<ReportsState>((set) => ({
  reports: [],
  loading: true,
  error: '',

  fetchReports: async () => {
    set({ loading: true, error: '' })
    try {
      const res = await authedFetch('/api/reports')
      if (!res.ok) {
        const data = await res.json().catch(() => ({ error: 'Failed to fetch reports' }))
        throw new Error(data.error || 'Failed to fetch reports')
      }
      const data = await res.json()
      const list: ReportEntry[] = Array.isArray(data) ? data : (data.reports ?? [])
      set({ reports: list, loading: false })
    } catch (err) {
      set({
        loading: false,
        error: err instanceof Error ? err.message : 'Failed to fetch reports',
      })
    }
  },
}))
