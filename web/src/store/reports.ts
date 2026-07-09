import { create } from 'zustand'
import { authedFetch } from './auth'

export interface ReportEntry {
  folder: string
  file: string
  title: string
  date: string
  session_id?: string
  project_name?: string
  session_name?: string
  session_created_at?: string
}

export type ReportSortOrder = 'newest' | 'oldest'

export interface ReportFilter {
  /** Case-insensitive substring matched against title, file, project,
   *  session id and the date folder. Empty = no text filter. */
  query: string
  /** Exact session_id to keep; empty = all sessions. */
  sessionId: string
  /** Exact project_name to keep; empty = all projects. */
  projectName: string
  order: ReportSortOrder
}

/** Parse a report's `date` to epoch millis; -Infinity when unparseable so
 *  it sorts to the end regardless of direction. */
function reportTime(r: ReportEntry): number {
  const t = new Date(r.date).getTime()
  return Number.isNaN(t) ? -Infinity : t
}

/**
 * Pure filter + sort over the loaded report list. Kept out of the
 * component so it can be reasoned about (and tested) in isolation.
 * Sorts by frontmatter `date`; unparseable dates sink to the bottom for
 * either order.
 */
export function filterAndSortReports(
  reports: ReportEntry[],
  { query, sessionId, projectName, order }: ReportFilter,
): ReportEntry[] {
  const q = query.trim().toLowerCase()
  const filtered = reports.filter((r) => {
    if (sessionId && r.session_id !== sessionId) return false
    if (projectName && r.project_name !== projectName) return false
    if (!q) return true
    const hay = [r.title, r.file, r.project_name, r.session_name, r.session_id, r.folder]
      .filter(Boolean)
      .join(' ')
      .toLowerCase()
    return hay.includes(q)
  })
  const dir = order === 'oldest' ? 1 : -1
  return filtered.sort((a, b) => {
    const ta = reportTime(a)
    const tb = reportTime(b)
    if (ta === tb) return 0
    // Unparseable (-Infinity) always sinks, independent of direction.
    if (ta === -Infinity) return 1
    if (tb === -Infinity) return -1
    return (ta - tb) * dir
  })
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
