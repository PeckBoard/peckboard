import { useState, type ReactNode } from 'react'
import WidgetFrame from '../WidgetFrame'
import type { InfoWidgetProps } from './types'
import { useDashboardData } from './useDashboardData'
import { DashEmpty, DashError, DashLoading } from './DashParts'

interface Day {
  date: string
  pass: number
  changes_requested: number
  crashes: number
}

interface Quality {
  days: Day[]
  totals: { pass: number; changes_requested: number; crashes: number; retries: number }
  by_project: {
    project_id: string
    project_name: string
    pass: number
    changes_requested: number
  }[]
}

const RANGES = [7, 30, 90] as const

/** Outcome series — semantic, not categorical: each is a good/bad state.
 *  Stack order bottom → top. */
const SERIES: { key: 'pass' | 'changes_requested' | 'crashes'; label: string; color: string }[] = [
  { key: 'pass', label: 'Passed', color: 'var(--success)' },
  { key: 'changes_requested', label: 'Changes', color: 'var(--warning)' },
  { key: 'crashes', label: 'Crashes', color: 'var(--danger)' },
]

function Kpi({
  label,
  value,
  tone,
  hint,
}: {
  label: string
  value: ReactNode
  tone?: 'danger' | 'warn'
  hint?: string
}) {
  return (
    <div className={`dash-kpi${tone ? ` dash-kpi-${tone}` : ''}`} title={hint ?? label}>
      <span className="dash-kpi-value">{value}</span>
      <span className="dash-kpi-label">{label}</span>
    </div>
  )
}

function shortDate(iso: string): string {
  const d = new Date(`${iso}T00:00:00Z`)
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' })
}

/** Stacked daily bars. viewBox-scaled so it fills the widget's width;
 *  hover a column for its breakdown. */
function DailyBars({ days }: { days: Day[] }) {
  const [hover, setHover] = useState<number | null>(null)
  const W = 600
  const H = 120
  const PAD_TOP = 6
  const max = Math.max(1, ...days.map((d) => d.pass + d.changes_requested + d.crashes))
  const slot = W / Math.max(1, days.length)
  const barW = Math.max(1, slot * 0.72)
  const y = (v: number) => ((H - PAD_TOP) * v) / max
  const hd = hover !== null ? days[hover] : null

  return (
    <div className="dash-chart" onPointerLeave={() => setHover(null)}>
      <svg
        className="dash-chart-svg"
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`Daily review outcomes over ${days.length} days`}
      >
        <line x1="0" x2={W} y1={H - 0.5} y2={H - 0.5} className="dash-chart-axis" />
        {days.map((d, i) => {
          let acc = 0
          const x = i * slot + (slot - barW) / 2
          return (
            <g key={d.date} onPointerEnter={() => setHover(i)}>
              <rect x={i * slot} y={0} width={slot} height={H} fill="transparent" />
              {hover === i && (
                <rect x={i * slot} y={0} width={slot} height={H} className="dash-chart-hover" />
              )}
              {SERIES.map((s) => {
                const v = d[s.key]
                if (v <= 0) return null
                const h = y(v)
                acc += h
                return (
                  <rect
                    key={s.key}
                    x={x}
                    y={H - acc}
                    width={barW}
                    height={Math.max(0, h - 1)}
                    fill={s.color}
                  />
                )
              })}
            </g>
          )
        })}
      </svg>
      <div className="dash-chart-xaxis">
        <span>{days.length ? shortDate(days[0].date) : ''}</span>
        <span>{days.length ? shortDate(days[days.length - 1].date) : ''}</span>
      </div>
      {hd && hover !== null && (
        <div
          className="dash-tooltip"
          role="status"
          style={{
            left: `${((hover + 0.5) / days.length) * 100}%`,
            transform: `translateX(${hover / days.length > 0.6 ? '-100%' : hover / days.length < 0.25 ? '0' : '-50%'})`,
          }}
        >
          <div className="dash-tooltip-title">{shortDate(hd.date)}</div>
          {SERIES.map((s) => (
            <div key={s.key} className="dash-tooltip-row">
              <span className="project-widget-swatch" style={{ background: s.color }} />
              <span>{s.label}</span>
              <span className="dash-tooltip-val">{hd[s.key]}</span>
            </div>
          ))}
        </div>
      )}
      <ul className="project-widget-legend dash-chart-legend">
        {SERIES.map((s) => (
          <li key={s.key}>
            <span className="project-widget-swatch" style={{ background: s.color }} />
            <span>{s.label}</span>
          </li>
        ))}
      </ul>
    </div>
  )
}

/** Review Quality: pass rate and failure KPIs plus a daily outcome chart. */
export default function ReviewQualityWidget({
  widget,
  ctx,
  menuItems,
  scopeProjectId,
  scopeName,
}: InfoWidgetProps) {
  const [range, setRange] = useState<(typeof RANGES)[number]>(30)
  const { data, error, reload } = useDashboardData<Quality>(
    `/api/dashboard/review-quality?days=${range}`,
    scopeProjectId,
    ['card-update'],
  )

  const t = data?.totals
  const reviewed = t ? t.pass + t.changes_requested : 0
  const rate = t && reviewed > 0 ? Math.round((t.pass / reviewed) * 100) : null

  let body: ReactNode
  if (!data || !t) {
    body = error ? <DashError message={error} onRetry={() => void reload()} /> : <DashLoading />
  } else if (reviewed === 0 && t.crashes === 0 && t.retries === 0) {
    body = (
      <DashEmpty testId="dash-review_quality-empty">No reviews in the last {range} days</DashEmpty>
    )
  } else {
    body = (
      <div className="dash-scroll dash-quality" data-testid="dash-review_quality-list">
        <div className="dash-kpis">
          <Kpi
            label="Pass rate"
            hint={`${t.pass} of ${reviewed} reviews passed`}
            value={rate === null ? '—' : `${rate}%`}
            tone={rate !== null && rate < 50 ? 'warn' : undefined}
          />
          <Kpi label="Changes" hint="Reviews that requested changes" value={t.changes_requested} />
          <Kpi label="Crashes" value={t.crashes} tone={t.crashes > 0 ? 'danger' : undefined} />
          <Kpi label="Retries" value={t.retries} />
        </div>
        <DailyBars days={data.days} />
      </div>
    )
  }

  return (
    <WidgetFrame
      kind="review_quality"
      widgetId={widget.id}
      title={scopeName ? `Review Quality · ${scopeName}` : 'Review Quality'}
      statusSlot={
        <select
          className="dash-select"
          aria-label="Time range"
          value={range}
          onChange={(e) => setRange(Number(e.target.value) as (typeof RANGES)[number])}
          data-testid="dash-review_quality-range"
        >
          {RANGES.map((r) => (
            <option key={r} value={r}>
              {r}d
            </option>
          ))}
        </select>
      }
      menuItems={menuItems}
      ctx={ctx}
    >
      {body}
    </WidgetFrame>
  )
}
