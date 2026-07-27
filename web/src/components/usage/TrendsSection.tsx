import { useEffect, useMemo, useState } from 'react'
import {
  fetchTrendSeries,
  useUsageStore,
  type ResolvedRange,
  type TrendEntity,
} from '../../store/usage'
import type { TrendSeries, UsageDashboard } from '../../types/api'
import { fmtTokens, fmtUsd } from '../../util/format'
import LineChart, { type ChartSeries } from './LineChart'

type Metric = 'tokens' | 'cost'
type Bucket = 'hour' | 'day'

/** Series styles in fixed slot order. Colour comes from the dedicated chart
 *  ramp in `index.css` — never the semantic tokens, because a series is an
 *  identity ("which project") and not a good/bad state — and every slot also
 *  carries a dash pattern, so a reader who cannot separate the hues can still
 *  trace a line and match it to its legend entry. The ORDER is the
 *  colour-blind-safety mechanism (it is what the palette was validated on):
 *  never reorder it, and never cycle past the last slot. */
const SERIES_STYLES: { color: string; dash?: string; pattern: string }[] = [
  { color: 'var(--chart-1)', pattern: 'solid' },
  { color: 'var(--chart-2)', dash: '6 4', pattern: 'dashed' },
  { color: 'var(--chart-3)', dash: '1 4', pattern: 'dotted' },
  { color: 'var(--chart-4)', dash: '12 4', pattern: 'long dash' },
  { color: 'var(--chart-5)', dash: '10 3 2 3', pattern: 'dash-dot' },
  { color: 'var(--chart-6)', dash: '6 3 1 3 1 3', pattern: 'dash-dot-dot' },
]

/** Cap on lines drawn at once — beyond a handful, an overlaid line chart is
 *  unreadable, and there is no seventh style to hand out. We keep the
 *  highest-volume series and note the remainder. */
const MAX_SERIES = SERIES_STYLES.length

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
  file_read: 'Cache Reads by File',
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

/** Axis tick label. The timezone is stated once in the dashboard caption
 *  rather than on every tick; the year appears only when the selected range
 *  makes `MM/DD` ambiguous (`showYear`), which keeps the axis readable. */
function shortLabel(ts: number, bucket: Bucket, showYear: boolean): string {
  const d = new Date(ts)
  const mm = `${d.getMonth() + 1}`.padStart(2, '0')
  const dd = `${d.getDate()}`.padStart(2, '0')
  const yy = showYear ? `/${`${d.getFullYear()}`.slice(2)}` : ''
  if (bucket === 'day') return `${mm}/${dd}${yy}`
  const hh = `${d.getHours()}`.padStart(2, '0')
  return `${mm}/${dd}${yy} ${hh}:00`
}

function seriesValue(s: TrendSeries, metric: Metric): number {
  return s.points.reduce((sum, p) => sum + (metric === 'tokens' ? p.tokens : p.est_cost), 0)
}

function TrendWidget({
  metric,
  title,
  nameFor,
  range,
  showYear,
}: {
  metric: Metric
  title: string
  nameFor: (entity: TrendEntity, id: string) => string
  /** The dashboard's resolved date range — the same window every other panel
   *  is scoped to, so the charts and the totals describe one period. */
  range: ResolvedRange
  showYear: boolean
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

  const { from, to } = range
  const queryKey = `${metric}|${entity}|${bucket}|${from ?? ''}|${to}`

  useEffect(() => {
    let cancelled = false
    const key = `${metric}|${entity}|${bucket}|${from ?? ''}|${to}`
    // fetchTrendSeries degrades to [] on error and never rejects.
    fetchTrendSeries({ metric, entity, bucket, from, to }).then((data) => {
      if (!cancelled) setLoaded({ key, series: data })
    })
    return () => {
      cancelled = true
    }
  }, [metric, entity, bucket, from, to])

  const loading = loaded.key !== queryKey

  const format = metric === 'tokens' ? fmtTokens : fmtUsd

  const ranked = useMemo(() => {
    const series = loaded.key === queryKey ? loaded.series : []
    return [...series].sort((a, b) => seriesValue(b, metric) - seriesValue(a, metric))
  }, [loaded, queryKey, metric])
  const shown = ranked.slice(0, MAX_SERIES)
  const hidden = ranked.length - shown.length

  // `shown` is capped at SERIES_STYLES.length, so slot `i` always exists — no
  // modulo, because a repeated hue would make two series indistinguishable.
  const chartSeries: (ChartSeries & { pattern: string })[] = shown.map((s, i) => ({
    id: s.entity_id,
    label: nameFor(entity, s.entity_id),
    color: SERIES_STYLES[i].color,
    dash: SERIES_STYLES[i].dash,
    pattern: SERIES_STYLES[i].pattern,
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
              formatX={(x) => shortLabel(x, bucket, showYear)}
              unit={metric === 'tokens' ? 'tokens' : 'USD'}
              xUnit={bucket}
              testid={`${testid}-chart`}
            />
            {chartSeries.length > 0 && (
              <ul className="usage-legend" data-testid={`${testid}-legend`}>
                {chartSeries.map((s) => (
                  <li className="usage-legend-item" key={s.id}>
                    {/* The swatch draws the series' line, not a colour chip:
                        it repeats the dash pattern so the legend still maps to
                        the chart when the hues are indistinguishable. */}
                    <svg
                      className="usage-legend-swatch"
                      viewBox="0 0 28 10"
                      width="28"
                      height="10"
                      aria-hidden="true"
                      data-pattern={s.pattern}
                    >
                      <line
                        x1="1"
                        y1="5"
                        x2="27"
                        y2="5"
                        stroke={s.color}
                        strokeWidth="2"
                        strokeLinecap="round"
                        strokeDasharray={s.dash}
                      />
                    </svg>
                    <span className="usage-legend-label" title={`${s.label} — ${s.pattern} line`}>
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
  // controls plus the dashboard's shared date range, so the section owns no
  // series state — it only resolves entity ids to names from the dashboard
  // the store already loaded.
  const dashboard = useUsageStore((s) => s.dashboard)
  const range = useUsageStore((s) => s.resolved)
  const nameFor = useMemo(() => makeNameResolver(dashboard), [dashboard])

  // `MM/DD` alone is ambiguous once the window leaves the current year or
  // straddles a year boundary — then, and only then, the ticks carry a year.
  const endYear = new Date(range.to - 1).getFullYear()
  const showYear =
    endYear !== new Date().getFullYear() ||
    (range.from != null && new Date(range.from).getFullYear() !== endYear)

  return (
    <section className="usage-section" data-testid="usage-trends">
      <h3 className="usage-section-title">Trends</h3>
      <div className="usage-subgrid usage-trend-grid">
        <TrendWidget
          metric="tokens"
          title="Tokens Over Time"
          nameFor={nameFor}
          range={range}
          showYear={showYear}
        />
        <TrendWidget
          metric="cost"
          title="Est. Cost Over Time (USD)"
          nameFor={nameFor}
          range={range}
          showYear={showYear}
        />
      </div>
    </section>
  )
}
