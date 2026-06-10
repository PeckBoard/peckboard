import { useEffect, useState, type ReactNode } from 'react'
import { useUsageStore } from '../store/usage'
import type { UsageTotals } from '../types/api'
import CostBreakdownSection from './usage/CostBreakdownSection'
import TrendsSection from './usage/TrendsSection'
import SessionsPanelBody from './usage/SessionsPanel'
import { CardsPanelBody, ExpertsPanelBody, ProjectsPanelBody } from './usage/EntityRollups'

/** Compact token formatter: 1_234_567 -> "1.23M". Keeps the stat cards and
 *  panel counts readable without a charting lib. */
function fmtTokens(n: number): string {
  if (!Number.isFinite(n)) return '0'
  const abs = Math.abs(n)
  if (abs >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`
  if (abs >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`
  if (abs >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return `${Math.round(n)}`
}

/** USD formatter that keeps small estimates legible (sub-cent costs still show
 *  a non-zero figure). Mirrors the backend's `est_cost`, which is already
 *  computed — the client never re-prices here. */
function fmtUsd(n: number): string {
  if (!Number.isFinite(n)) return '$0.00'
  if (n > 0 && n < 0.01) return '<$0.01'
  return `$${n.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
}

/** The header's overall-totals cards. Each is a labelled figure summed across
 *  every session. */
function totalsCards(totals: UsageTotals): { label: string; value: string }[] {
  return [
    { label: 'Est. Cost', value: fmtUsd(totals.est_cost) },
    { label: 'Total Tokens', value: fmtTokens(totals.total_tokens) },
    { label: 'Input', value: fmtTokens(totals.input_tokens) },
    { label: 'Output', value: fmtTokens(totals.output_tokens) },
    { label: 'Cache Read', value: fmtTokens(totals.cache_read_tokens) },
    { label: 'Context', value: fmtTokens(totals.context_tokens) },
  ]
}

interface PanelProps {
  title: string
  /** Number of rows the panel's data currently holds — shown as a badge and
   *  used to pick the empty vs. populated state. */
  count: number
  testid: string
  children?: ReactNode
}

/** A single dashboard panel shell. Real internals (tables, charts) are filled
 *  in by the dependent frontend panel cards; this renders the frame, the
 *  count badge, and an empty placeholder so the layout reads correctly before
 *  those land or while the backend has no data. */
function UsagePanel({ title, count, testid, children }: PanelProps) {
  return (
    <section className="usage-panel" data-testid={testid}>
      <header className="usage-panel-header">
        <h3 className="usage-panel-title">{title}</h3>
        <span className="usage-panel-count">{count}</span>
      </header>
      <div className="usage-panel-body">
        {count === 0 ? (
          <div className="usage-panel-empty">No data yet</div>
        ) : (
          (children ?? (
            <div className="usage-panel-placeholder">
              {count} {count === 1 ? 'item' : 'items'}
            </div>
          ))
        )}
      </div>
    </section>
  )
}

export default function UsageDashboard() {
  const dashboard = useUsageStore((s) => s.dashboard)
  const loaded = useUsageStore((s) => s.loaded)
  const loading = useUsageStore((s) => s.loading)
  const error = useUsageStore((s) => s.error)
  const fetchUsage = useUsageStore((s) => s.fetchUsage)

  // Drill-down selection: clicking a project in the Projects panel filters the
  // Cards panel to that project. Transient view state, so it lives here rather
  // than in the store (see the usage dashboard panel conventions).
  const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null)

  useEffect(() => {
    fetchUsage()
  }, [fetchUsage])

  if (loading && !loaded) {
    return (
      <div className="usage-page" data-testid="usage-view">
        <div className="chat-loading">
          <div className="loading-spinner" />
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="usage-page" data-testid="usage-view">
        <p className="form-error">{error}</p>
      </div>
    )
  }

  const { totals, sessions, projects, cards, experts, operations } = dashboard

  return (
    <div className="usage-page" data-testid="usage-view">
      <div className="usage-header">
        <h2 className="usage-title">Usage</h2>
      </div>

      <div className="usage-stat-grid" data-testid="usage-totals">
        {totalsCards(totals).map((c) => (
          <div className="usage-stat-card" key={c.label}>
            <div className="usage-stat-label">{c.label}</div>
            <div className="usage-stat-value">{c.value}</div>
          </div>
        ))}
      </div>

      <div className="usage-grid">
        <UsagePanel title="Sessions" count={sessions.length} testid="usage-panel-sessions">
          <SessionsPanelBody sessions={sessions} />
        </UsagePanel>
        <UsagePanel title="Projects" count={projects.length} testid="usage-panel-projects">
          <ProjectsPanelBody
            projects={projects}
            selectedId={selectedProjectId}
            onSelect={setSelectedProjectId}
          />
        </UsagePanel>
        <UsagePanel title="Cards" count={cards.length} testid="usage-panel-cards">
          <CardsPanelBody
            cards={cards}
            projects={projects}
            selectedProjectId={selectedProjectId}
            onClearFilter={() => setSelectedProjectId(null)}
          />
        </UsagePanel>
        <UsagePanel title="Experts" count={experts.length} testid="usage-panel-experts">
          <ExpertsPanelBody experts={experts} />
        </UsagePanel>
      </div>

      <CostBreakdownSection operations={operations} />
      <TrendsSection />
    </div>
  )
}
