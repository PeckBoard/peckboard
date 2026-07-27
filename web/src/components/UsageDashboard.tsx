import { useEffect, useMemo, useState, type ReactNode } from 'react'
import { USAGE_PANEL_LABELS, useUsageStore, type UsagePanelKey } from '../store/usage'
import type { SessionUsage, UsageTotals } from '../types/api'
import { billedTokens } from '../util/cost'
import { fmtTokens, fmtUsd } from '../util/format'
import CostFootnote from './usage/CostFootnote'
import CostBreakdownSection from './usage/CostBreakdownSection'
import ProjectDetail from './usage/ProjectDetail'
import SessionDetail from './usage/SessionDetail'
import TrendsSection from './usage/TrendsSection'
import SessionsPanelBody from './usage/SessionsPanel'
import { CardsPanelBody, ExpertsPanelBody, ProjectsPanelBody } from './usage/EntityRollups'
import RangeBar from './usage/RangeBar'

/** The header's overall-totals cards. Each is a labelled figure summed across
 *  every session. When the sessions request failed the figures are unknown, so
 *  every card reads “—” — rendering $0.00 would be a lie the user reads as
 *  “I spent nothing”.
 *
 *  Two of these figures were previously misleading and are deliberately
 *  shaped now: the tokens card is the BILLED sum, which is the same figure
 *  the per-session rows report, so the header reconciles with the panels; and
 *  the context card is the single largest session rather than a sum, because
 *  context occupancy is a snapshot and adding snapshots across sessions
 *  produced a number that meant nothing sitting next to real spend. */
function totalsCards(
  totals: UsageTotals,
  sessions: SessionUsage[],
  unavailable: boolean,
): { label: string; value: string; hint: string; testid: string }[] {
  const usd = (n: number) => (unavailable ? '—' : fmtUsd(n))
  const tok = (n: number) => (unavailable ? '—' : fmtTokens(n))
  const largestContext = sessions.reduce((m, s) => Math.max(m, s.total_context_tokens), 0)
  return [
    {
      label: 'Est. cost (USD)',
      value: usd(totals.est_cost),
      hint: 'Estimate in USD, priced from the per-model rate table served by /api/usage/costs',
      testid: 'usage-stat-cost',
    },
    {
      label: 'Billed Tokens',
      value: tok(billedTokens(totals)),
      hint: 'Input + output + cache read + cache write, summed across every session',
      testid: 'usage-stat-billed-tokens',
    },
    {
      label: 'Input',
      value: tok(totals.input_tokens),
      hint: 'Input tokens across every session',
      testid: 'usage-stat-input',
    },
    {
      label: 'Output',
      value: tok(totals.output_tokens),
      hint: 'Output tokens across every session',
      testid: 'usage-stat-output',
    },
    {
      label: 'Cache Read',
      value: tok(totals.cache_read_tokens),
      hint: 'Cache-read tokens across every session',
      testid: 'usage-stat-cache-read',
    },
    {
      label: 'Largest Context',
      value: tok(largestContext),
      hint: 'Peak context occupancy of the fullest single session — a snapshot, not spend',
      testid: 'usage-stat-largest-context',
    },
  ]
}

interface PanelProps {
  title: string
  /** Number of rows the panel's data currently holds — shown as a badge and
   *  used to pick the empty vs. populated state. */
  count: number
  testid: string
  /** The panel's backing request failed. "No data yet" would then be a claim
   *  we can't make, so the panel shows an error + Retry instead. */
  failed?: boolean
  onRetry?: () => void
  children?: ReactNode
}

/** A single dashboard panel shell: frame, count badge, and either an error
 *  (request failed) or an empty placeholder (request succeeded, no rows). */
function UsagePanel({ title, count, testid, failed, onRetry, children }: PanelProps) {
  return (
    <section className="usage-panel" data-testid={testid}>
      <header className="usage-panel-header">
        <h3 className="usage-panel-title">{title}</h3>
        <span className="usage-panel-count">{failed ? '—' : count}</span>
      </header>
      <div className="usage-panel-body">
        {failed ? (
          <div className="usage-panel-error" role="alert" data-testid={`${testid}-error`}>
            <span>Couldn’t load this panel.</span>
            <button type="button" onClick={onRetry}>
              Retry
            </button>
          </div>
        ) : count === 0 ? (
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

/** Which usage page is showing: the overview, one session (chat / worker /
 *  expert — they share the per-prompt detail page), or one project. */
type UsagePage =
  | { kind: 'overview' }
  | { kind: 'session'; id: string }
  | { kind: 'project'; id: string }

/** How old the figures must be before returning to the tab refetches them. */
const STALE_AFTER_MS = 60_000

export default function UsageDashboard() {
  const dashboard = useUsageStore((s) => s.dashboard)
  const loaded = useUsageStore((s) => s.loaded)
  const loading = useUsageStore((s) => s.loading)
  const error = useUsageStore((s) => s.error)
  const fetchUsage = useUsageStore((s) => s.fetchUsage)
  const failedPanels = useUsageStore((s) => s.failedPanels)

  const [page, setPage] = useState<UsagePage>({ kind: 'overview' })

  useEffect(() => {
    fetchUsage()
  }, [fetchUsage])

  // A dashboard left open all day would otherwise keep showing the morning's
  // numbers. Rather than polling on a timer (cost with nobody looking), the
  // data is refreshed when the tab becomes visible again and what's on screen
  // has gone stale.
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState !== 'visible') return
      const { lastUpdated, loading: busy } = useUsageStore.getState()
      if (busy || Date.now() - lastUpdated < STALE_AFTER_MS) return
      void fetchUsage()
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => document.removeEventListener('visibilitychange', onVisible)
  }, [fetchUsage])

  const { totals, sessions, projects, cards, experts, operations } = dashboard

  // Chats vs workers: both are sessions, split by the backend's role flags.
  // Experts come from their own rollup (they may overlap `sessions`, which
  // also carries `is_expert` — the chats list excludes them).
  const chats = useMemo(() => sessions.filter((s) => !s.is_worker && !s.is_expert), [sessions])
  const workers = useMemo(() => sessions.filter((s) => s.is_worker), [sessions])
  const failed = (k: UsagePanelKey) => failedPanels.includes(k)

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

  const backToOverview = () => setPage({ kind: 'overview' })
  const openSession = (id: string) => setPage({ kind: 'session', id })
  const openProject = (id: string) => setPage({ kind: 'project', id })

  if (page.kind === 'session') {
    return (
      <div className="usage-page" data-testid="usage-view">
        <SessionDetail id={page.id} onBack={backToOverview} />
      </div>
    )
  }

  if (page.kind === 'project') {
    return (
      <div className="usage-page" data-testid="usage-view">
        <ProjectDetail
          id={page.id}
          project={projects.find((p) => p.id === page.id) ?? null}
          sessions={sessions}
          cards={cards}
          onBack={backToOverview}
          onOpenSession={openSession}
        />
      </div>
    )
  }

  return (
    <div className="usage-page" data-testid="usage-view">
      <div className="usage-header">
        <h2 className="usage-title">Usage</h2>
        <RangeBar />
      </div>

      {failedPanels.length > 0 && (
        <div className="fetch-error-banner" role="alert" data-testid="usage-fetch-error">
          <span>
            Couldn’t load {failedPanels.map((k) => USAGE_PANEL_LABELS[k]).join(', ')} — the figures
            below are incomplete.
          </span>
          <button type="button" onClick={() => fetchUsage()}>
            Retry
          </button>
        </div>
      )}

      <div className="usage-stat-grid" data-testid="usage-totals">
        {totalsCards(totals, sessions, failed('sessions')).map((c) => (
          <div className="usage-stat-card" key={c.label} title={c.hint} data-testid={c.testid}>
            <div className="usage-stat-label">{c.label}</div>
            <div className="usage-stat-value" data-testid={`${c.testid}-value`}>
              {c.value}
            </div>
          </div>
        ))}
      </div>

      <div className="usage-grid">
        <UsagePanel
          title="Chats"
          count={chats.length}
          testid="usage-panel-sessions"
          failed={failed('sessions')}
          onRetry={fetchUsage}
        >
          <SessionsPanelBody sessions={chats} onOpen={openSession} />
        </UsagePanel>
        <UsagePanel
          title="Workers"
          count={workers.length}
          testid="usage-panel-workers"
          failed={failed('sessions')}
          onRetry={fetchUsage}
        >
          <SessionsPanelBody sessions={workers} onOpen={openSession} />
        </UsagePanel>
        <UsagePanel
          title="Projects"
          count={projects.length}
          testid="usage-panel-projects"
          failed={failed('projects')}
          onRetry={fetchUsage}
        >
          <ProjectsPanelBody projects={projects} onOpen={openProject} />
        </UsagePanel>
        <UsagePanel
          title="Cards"
          count={cards.length}
          testid="usage-panel-cards"
          failed={failed('cards')}
          onRetry={fetchUsage}
        >
          <CardsPanelBody cards={cards} />
        </UsagePanel>
        <UsagePanel
          title="Experts"
          count={experts.length}
          testid="usage-panel-experts"
          failed={failed('experts')}
          onRetry={fetchUsage}
        >
          <ExpertsPanelBody experts={experts} onOpen={openSession} />
        </UsagePanel>
      </div>

      <CostBreakdownSection
        operations={operations}
        failed={failed('operations')}
        onRetry={fetchUsage}
      />
      <TrendsSection />
      <CostFootnote />
    </div>
  )
}
