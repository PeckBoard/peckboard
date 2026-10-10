import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import { DashCount, DashEmpty, DashError, DashLoading } from './DashParts'
import { relativeTime } from './useDashboardData'
import { pluginUiFetch, PluginUiError } from './pluginUi'
import type { InfoWidgetProps } from './types'
import '../../../styles/dashboard-info.css'
import '../../../styles/dashboard-plugins.css'

const PLUGIN = 'github-bridge'
/** The plugin caches GitHub for ≤ 60s, so polling faster buys nothing. */
const POLL_MS = 60_000

/** One linked PR (`GET /api/plugin-ui/github-bridge/prs`). */
interface Pr {
  card_id: string | null
  card_title: string | null
  project_id: string | null
  repo: string
  number: number
  title: string
  url: string
  state: 'open' | 'closed' | 'merged'
  draft: boolean
  author: string
  updated_at: string
  checks: {
    state: 'success' | 'failure' | 'pending' | 'none'
    passed: number
    failed: number
    pending: number
  }
  review: 'approved' | 'changes_requested' | 'review_required' | null
}

interface PrsResponse {
  configured: boolean
  prs: Pr[]
  error: string | null
}

const REVIEW: Record<string, { label: string; tone: string }> = {
  approved: { label: 'Approved', tone: 'ok' },
  changes_requested: { label: 'Changes', tone: 'danger' },
  review_required: { label: 'Review', tone: 'neutral' },
}

function prTone(p: Pr): 'open' | 'draft' | 'merged' | 'closed' {
  if (p.state === 'merged') return 'merged'
  if (p.state === 'closed') return 'closed'
  return p.draft ? 'draft' : 'open'
}

/** GitHub-style PR glyph, coloured by state. */
function PrIcon({ tone }: { tone: ReturnType<typeof prTone> }) {
  const label = tone[0].toUpperCase() + tone.slice(1)
  return (
    <svg
      className={`prs-icon prs-icon-${tone}`}
      width="14"
      height="14"
      viewBox="0 0 16 16"
      role="img"
      aria-label={label}
    >
      <title>{label}</title>
      <g fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round">
        <circle cx="4.5" cy="3.5" r="1.5" />
        <circle cx="4.5" cy="12.5" r="1.5" />
        {tone === 'merged' ? (
          <>
            <circle cx="11.5" cy="8" r="1.5" />
            <path d="M4.5 5v6M4.5 5c0 2 2 3 5.5 3" />
          </>
        ) : tone === 'closed' ? (
          <>
            <path d="M4.5 5v6M10 3l3 3M13 3l-3 3" />
            <circle cx="11.5" cy="12.5" r="1.5" />
          </>
        ) : (
          <>
            <circle cx="11.5" cy="12.5" r="1.5" />
            <path
              d={
                tone === 'draft'
                  ? 'M4.5 5v6M11.5 6.5v1M11.5 9.5v1'
                  : 'M4.5 5v6M11.5 11V6.5c0-1.1-.9-2-2-2H7.5'
              }
            />
          </>
        )}
      </g>
    </svg>
  )
}

function ChecksPill({ c }: { c: Pr['checks'] }) {
  if (c.state === 'none') return null
  const tone = c.state === 'success' ? 'ok' : c.state === 'failure' ? 'danger' : 'run'
  const text =
    c.state === 'success'
      ? `✓ ${c.passed}`
      : c.state === 'failure'
        ? `✗ ${c.failed}`
        : `● ${c.pending}`
  return (
    <span
      className={`dash-pill dash-pill-${tone} prs-checks`}
      title={`${c.passed} passed · ${c.failed} failed · ${c.pending} pending`}
      data-testid="dash-pr-checks"
      data-state={c.state}
    >
      {text}
    </span>
  )
}

function usePrs() {
  const [data, setData] = useState<PrsResponse | null>(null)
  const [error, setError] = useState<{ message: string; notInstalled: boolean } | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const seq = useRef(0)

  const load = useCallback(async (force = false) => {
    const mine = ++seq.current
    if (force) setRefreshing(true)
    try {
      const d = await pluginUiFetch<PrsResponse>(
        PLUGIN,
        force ? '/refresh' : '/prs',
        force ? { method: 'POST' } : undefined,
      )
      if (mine !== seq.current) return
      setData(d)
      setError(null)
    } catch (e) {
      if (mine !== seq.current) return
      setError({
        message: e instanceof Error ? e.message : 'Failed to load',
        notInstalled: e instanceof PluginUiError && e.notInstalled,
      })
    } finally {
      if (force) setRefreshing(false)
    }
  }, [])

  useEffect(() => {
    const first = window.setTimeout(() => void load(), 0)
    const t = window.setInterval(() => {
      if (document.visibilityState === 'visible') void load()
    }, POLL_MS)
    return () => {
      window.clearTimeout(first)
      window.clearInterval(t)
    }
  }, [load])

  return { data, error, refreshing, reload: load }
}

/** PRs linked to cards (github-bridge `gh_link_pr`): state, CI checks,
 *  review, and the card each belongs to. Scoped widgets filter on the
 *  card's project. */
export default function PrsWidget({
  widget,
  ctx,
  menuItems,
  scopeProjectId,
  scopeName,
  onOpenProject,
}: InfoWidgetProps) {
  const { data, error, refreshing, reload } = usePrs()
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 60_000)
    return () => window.clearInterval(t)
  }, [])

  const prs = (data?.prs ?? []).filter((p) => !scopeProjectId || p.project_id === scopeProjectId)
  const failing = prs.filter((p) => p.state === 'open' && p.checks.state === 'failure').length
  const notInstalled = !!error?.notInstalled

  let body: ReactNode
  if (notInstalled) {
    body = (
      <DashEmpty testId="dash-prs-empty">
        <span data-reason="not-installed">GitHub Bridge plugin not installed</span>
      </DashEmpty>
    )
  } else if (!data) {
    body = error ? (
      <DashError message={error.message} onRetry={() => void reload()} />
    ) : (
      <DashLoading />
    )
  } else if (!data.configured) {
    body = (
      <DashEmpty testId="dash-prs-empty">
        <span data-reason="not-configured">
          Connect GitHub in the GitHub Bridge plugin settings
        </span>
      </DashEmpty>
    )
  } else {
    const banner = data.error || error?.message
    body = (
      <div className="dash-scroll">
        {banner && (
          <div className="dash-inline-error dash-banner" role="alert" title={banner}>
            {banner}
          </div>
        )}
        {prs.length === 0 ? (
          <DashEmpty testId="dash-prs-empty">No linked pull requests</DashEmpty>
        ) : (
          <ul className="dash-items" data-testid="dash-prs-list">
            {prs.map((p) => {
              const tone = prTone(p)
              const review = p.review ? REVIEW[p.review] : null
              const key = `${p.repo}#${p.number}`
              return (
                <li
                  key={key}
                  className={`dash-item prs-item${tone === 'closed' ? ' dash-item-muted' : ''}`}
                  data-testid="dash-pr-row"
                  data-pr={key}
                  data-state={tone}
                >
                  <div className="dash-item-main prs-main">
                    <PrIcon tone={tone} />
                    <span className="dash-item-text">
                      <span className="prs-title-line">
                        <span className="dash-mono prs-ref">{key}</span>
                        <a
                          className="dash-item-title prs-title"
                          href={p.url}
                          target="_blank"
                          rel="noopener noreferrer"
                          title={p.title}
                        >
                          {p.title}
                        </a>
                      </span>
                      <span className="dash-item-sub">
                        {p.author}
                        {' · '}
                        <span title={new Date(p.updated_at).toLocaleString()}>
                          {relativeTime(p.updated_at, now)}
                        </span>
                        {p.card_title && (
                          <>
                            {' · '}
                            {p.project_id ? (
                              <button
                                type="button"
                                className="dash-link prs-card"
                                title={`Open the board for ${p.card_title}`}
                                onClick={() => onOpenProject(p.project_id!)}
                              >
                                {p.card_title}
                              </button>
                            ) : (
                              <span className="prs-card">{p.card_title}</span>
                            )}
                          </>
                        )}
                      </span>
                    </span>
                    <ChecksPill c={p.checks} />
                    {review && (
                      <span
                        className={`dash-pill dash-pill-${review.tone}`}
                        data-testid="dash-pr-review"
                      >
                        {review.label}
                      </span>
                    )}
                  </div>
                </li>
              )
            })}
          </ul>
        )}
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="prs"
      widgetId={widget.id}
      title={scopeName ? `PRs & CI · ${scopeName}` : 'PRs & CI'}
      statusSlot={
        !notInstalled && data?.configured ? (
          <>
            {prs.length > 0 && (
              <DashCount
                n={failing}
                tone="danger"
                label={`${failing} open PRs with failing checks`}
              />
            )}
            <button
              type="button"
              className="prs-refresh"
              disabled={refreshing}
              aria-busy={refreshing || undefined}
              aria-label="Refresh pull requests"
              title="Refetch from GitHub"
              data-testid="dash-prs-refresh"
              onClick={() => void reload(true)}
            >
              <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
                <path
                  d="M12.5 6.5A4.75 4.75 0 0 0 3.6 6M3.5 9.5a4.75 4.75 0 0 0 8.9.5M3.2 3.2v2.9h2.9M12.8 12.8V9.9H9.9"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.4"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
              </svg>
            </button>
          </>
        ) : undefined
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {body}
    </WidgetFrame>
  )
}
