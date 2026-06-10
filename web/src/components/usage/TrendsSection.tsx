import { useEffect, useMemo, useState } from 'react'
import { fetchTrendSeries, useUsageStore, type TrendEntity } from '../../store/usage'
import type { TrendSeries, UsageDashboard } from '../../types/api'
import { fmtTokens, fmtUsd } from '../../util/format'
import LineChart, { type ChartSeries } from './LineChart'

type Metric = 'tokens' | 'cost'
type Bucket = 'hour' | 'day'

/** Series-color rotation. Semantic tokens only, so charts auto-theme; the
 *  first (accent) is used for the single `overall` series. */
const PALETTE = [
  'var(--accent)',
  'var(--success)',
  'var(--warning)',
  'var(--danger)',
  'var(--accent-muted)',
  'var(--text3)',
]

/** Cap on lines drawn at once — beyond a handful, an overlaid line chart is
 *  unreadable. We keep the highest-volume series and note the remainder. */
const MAX_SERIES = 6

const ENTITY_OPTIONS: { value: TrendEntity; label: string }[] = [
  { value: 'overall', label: 'Overall' },
  { value: 'session', label: 'By Session' },
  { value: 'project', label: 'By Project' },
  { value: 'card', label: 'By Card' },
  { value: 'expert', label: 'By Expert' },
  { value: 'operation', label: 'By Operation' },
]

const OPERATION_LABELS: Record<string, string> = {
  file_update: 'File Updates',
  ask_expert: 'Expert Consults',
  qa: 'Questions & Answers',
}

/** Build an id→name resolver from the entity rows the dashboard already holds,
 *  so trend series read as "Onboarding card" rather than a raw UUID. */
function makeNameResolver(dashboard: UsageDashboard) {
  const byId = new Map<string, string>()
  for (const row of [
    ...dashboard.sessions,
    ...dashboard.projects,
    ...dashboard.cards,
    ...dashboard.experts,
  ]) {
    if (row.name) byId.set(row.id, row.name)
  }
  return (entity: TrendEntity, id: string): string => {
    if (entity === 'overall' || id === 'overall') return 'Overall'
    if (entity === 'operation') return OPERATION_LABELS[id] ?? id
    return byId.get(id) ?? `${id.slice(0, 8)}…`
  }
}

function shortLabel(ts: number, bucket: Bucket): string {
  const d = new Date(ts)
  const mm = `${d.getMonth() + 1}`.padStart(2, '0')
  const dd = `${d.getDate()}`.padStart(2, '0')
  if (bucket === 'day') return `${mm}/${dd}`
  const hh = `${d.getHours()}`.padStart(2, '0')
  return `${mm}/${dd} ${hh}:00`
}

function seriesValue(s: TrendSeries, metric: Metric): number {
  return s.points.reduce((sum, p) => sum + (metric === 'tokens' ? p.tokens : p.est_cost), 0)
}

function TrendWidget({
  metric,
  title,
  nameFor,
}: {
  metric: Metric
  title: string
  nameFor: (entity: TrendEntity, id: string) => string
}) {
  const [bucket, setBucket] = useState<Bucket>('day')
  const [entity, setEntity] = useState<TrendEntity>('overall')
  // The loaded series tagged with the query they belong to. `loading` is then
  // derived (loaded key !== current query) instead of being set synchronously
  // in the effect, which would trigger cascading renders.
  const [loaded, setLoaded] = useState<{ key: string; series: TrendSeries[] }>({
    key: '',
    series: [],
  })

  const queryKey = `${metric}|${entity}|${bucket}`

  useEffect(() => {
    let cancelled = false
    const key = `${metric}|${entity}|${bucket}`
    // fetchTrendSeries degrades to [] on error and never rejects.
    fetchTrendSeries({ metric, entity, bucket }).then((data) => {
      if (!cancelled) setLoaded({ key, series: data })
    })
    return () => {
      cancelled = true
    }
  }, [metric, entity, bucket])

  const loading = loaded.key !== queryKey

  const format = metric === 'tokens' ? fmtTokens : fmtUsd

  const ranked = useMemo(() => {
    const series = loaded.key === queryKey ? loaded.series : []
    return [...series].sort((a, b) => seriesValue(b, metric) - seriesValue(a, metric))
  }, [loaded, queryKey, metric])
  const shown = ranked.slice(0, MAX_SERIES)
  const hidden = ranked.length - shown.length

  const chartSeries: ChartSeries[] = shown.map((s, i) => ({
    id: s.entity_id,
    label: nameFor(entity, s.entity_id),
    color: PALETTE[i % PALETTE.length],
    points: s.points.map((p) => ({
      x: p.bucket_ts,
      y: metric === 'tokens' ? p.tokens : p.est_cost,
    })),
  }))

  const testid = `usage-trend-${metric}`

  return (
    <section className="usage-panel usage-trend" data-testid={testid}>
      <header className="usage-panel-header usage-trend-header">
        <h4 className="usage-panel-title">{title}</h4>
        <div className="usage-trend-controls">
          <div className="usage-seg" role="group" aria-label="Bucket width">
            {(['hour', 'day'] as Bucket[]).map((b) => (
              <button
                key={b}
                type="button"
                className={b === bucket ? 'usage-seg-btn active' : 'usage-seg-btn'}
                aria-pressed={b === bucket}
                onClick={() => setBucket(b)}
                data-testid={`${testid}-bucket-${b}`}
              >
                {b === 'hour' ? 'Hourly' : 'Daily'}
              </button>
            ))}
          </div>
          <select
            className="usage-trend-select"
            value={entity}
            onChange={(e) => setEntity(e.target.value as TrendEntity)}
            aria-label="Group trend by"
            data-testid={`${testid}-entity`}
          >
            {ENTITY_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
        </div>
      </header>
      <div className="usage-trend-body">
        {loading ? (
          <div className="usage-panel-empty">Loading…</div>
        ) : (
          <>
            <LineChart
              series={chartSeries}
              area={chartSeries.length === 1}
              formatValue={format}
              formatX={(x) => shortLabel(x, bucket)}
              testid={`${testid}-chart`}
            />
            {chartSeries.length > 0 && (
              <ul className="usage-legend" data-testid={`${testid}-legend`}>
                {chartSeries.map((s) => (
                  <li className="usage-legend-item" key={s.id}>
                    <span className="usage-legend-swatch" style={{ background: s.color }} />
                    <span className="usage-legend-label" title={s.label}>
                      {s.label}
                    </span>
                  </li>
                ))}
                {hidden > 0 && <li className="usage-legend-more">+{hidden} more</li>}
              </ul>
            )}
          </>
        )}
      </div>
    </section>
  )
}

/** The cost-and-trends card's second half: one reusable trend widget per
 *  metric (tokens, cost), each with its own bucket + entity selectors driving
 *  a live `/api/usage/trends` query. */
export default function TrendsSection() {
  // Each widget re-queries `/api/usage/trends` from its own bucket/entity
  // controls, so the section owns no series state — it only resolves entity
  // ids to names from the dashboard the store already loaded.
  const dashboard = useUsageStore((s) => s.dashboard)
  const nameFor = useMemo(() => makeNameResolver(dashboard), [dashboard])

  return (
    <section className="usage-section" data-testid="usage-trends">
      <h3 className="usage-section-title">Trends</h3>
      <div className="usage-subgrid usage-trend-grid">
        <TrendWidget metric="tokens" title="Tokens Over Time" nameFor={nameFor} />
        <TrendWidget metric="cost" title="Cost Over Time" nameFor={nameFor} />
      </div>
    </section>
  )
}
