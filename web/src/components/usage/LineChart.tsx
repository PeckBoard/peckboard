// Inline-SVG multi-series line/area chart for the usage trend widgets. No
// charting dependency by design (see the usage dashboard's "no chart lib"
// note); this is the whole renderer. Values are pre-formatted by the caller —
// the chart only does geometry, never pricing.

import { useState, type KeyboardEvent, type PointerEvent } from 'react'

/** One plotted series. `points` must be ascending in `x` (the backend already
 *  returns trend points ordered by bucket_ts). */
export interface ChartSeries {
  id: string
  label: string
  /** A CSS color token string, e.g. `var(--chart-1)`. Never a hex literal, so
   *  series auto-theme for dark mode. */
  color: string
  /** `stroke-dasharray` for the line — the colour-independent channel that lets
   *  a reader trace a series without relying on hue (and the secondary encoding
   *  the chart palette's CVD budget assumes). Omit for a solid line. */
  dash?: string
  points: { x: number; y: number }[]
}

interface LineChartProps {
  series: ChartSeries[]
  /** Pixel height of the plot band. Axis labels live outside it, in HTML. */
  height?: number
  /** Fill the area under the line. Only applied when there is a single series,
   *  since overlapping fills muddy a multi-series chart. */
  area?: boolean
  formatValue: (v: number) => string
  formatX: (x: number) => string
  /** Unit of the value axis ("tokens", "USD"). Printed in the caption and in
   *  every readout row, so a chart of 1.2M tokens and one of $1.20 can never
   *  be confused at a glance. */
  unit: string
  /** What one x step is ("day", "hour", "turn") — completes the caption. */
  xUnit?: string
  testid?: string
}

// viewBox geometry. Width is arbitrary (the SVG scales to 100% width via
// preserveAspectRatio="none"). Nothing inside the SVG is text or a circle, so
// the non-uniform scale only ever stretches paths — whose stroke widths are
// pinned by vectorEffect. Ticks, axis labels, point markers and the readout
// are HTML laid over the same box in percentage coordinates, which is exact
// because a non-uniform fit makes viewBox fraction == box fraction.
const VB_W = 640
const PAD_L = 4
const PAD_R = 4
const PAD_T = 8
const PAD_B = 8
const Y_TICKS = 3

/** Sentence-case a unit for the caption; an all-caps unit (USD) keeps its
 *  casing. */
function captionUnit(unit: string): string {
  return unit === unit.toUpperCase() ? unit : unit.charAt(0).toUpperCase() + unit.slice(1)
}

export default function LineChart({
  series,
  height = 180,
  area = false,
  formatValue,
  formatX,
  unit,
  xUnit,
  testid,
}: LineChartProps) {
  // Index into `xs` of the point the readout is pinned to (hover or arrows).
  const [active, setActive] = useState<number | null>(null)

  const withPoints = series.filter((s) => s.points.length > 0)
  if (withPoints.length === 0) {
    return (
      <div className="usage-panel-empty" data-testid={testid ? `${testid}-empty` : undefined}>
        No data in this range
      </div>
    )
  }

  const allX = withPoints.flatMap((s) => s.points.map((p) => p.x))
  const allY = withPoints.flatMap((s) => s.points.map((p) => p.y))
  const xMin = Math.min(...allX)
  const xMax = Math.max(...allX)
  // Anchor the value axis at zero — tokens and cost are non-negative, and a
  // zero baseline keeps relative magnitudes honest across re-renders.
  const yMin = 0
  const yMax = Math.max(...allY, 1)

  const plotW = VB_W - PAD_L - PAD_R
  const plotH = height - PAD_T - PAD_B
  const xAt = (x: number) =>
    xMax === xMin ? PAD_L + plotW / 2 : PAD_L + ((x - xMin) / (xMax - xMin)) * plotW
  const yAt = (y: number) => PAD_T + plotH - ((y - yMin) / (yMax - yMin)) * plotH
  // viewBox units -> percentage of the rendered box, for the HTML layers.
  const pctX = (x: number) => (xAt(x) / VB_W) * 100
  const pctY = (y: number) => (yAt(y) / height) * 100

  const yTicks = Array.from({ length: Y_TICKS + 1 }, (_, i) => yMin + ((yMax - yMin) * i) / Y_TICKS)
  const fillArea = area && withPoints.length === 1

  // Every distinct x, ascending: the traversal order for hover and arrow keys.
  const xs = Array.from(new Set(allX)).sort((a, b) => a - b)
  const activeIdx = active === null ? null : Math.min(active, xs.length - 1)
  const activeX = activeIdx === null ? null : xs[activeIdx]
  const rows =
    activeX === null
      ? []
      : withPoints.flatMap((s) => {
          const p = s.points.find((q) => q.x === activeX)
          return p ? [{ s, p }] : []
        })

  const summary = `Trend chart of ${withPoints.map((s) => s.label).join(', ')} in ${unit} from ${formatX(
    xMin,
  )} to ${formatX(xMax)}, peak ${formatValue(yMax)}`
  // Spoken by the live region on every readout move, so the values are
  // reachable without sight and without a mouse.
  const readout =
    activeX === null
      ? ''
      : `${formatX(activeX)}: ${rows
          .map(({ s, p }) => `${s.label} ${formatValue(p.y)} ${unit}`)
          .join(', ')}`

  /** Pin the readout to the point nearest the pointer. Measured off the
   *  rendered box (not viewBox units) so it stays correct at any width. */
  const pickAt = (e: PointerEvent<HTMLDivElement>) => {
    const r = e.currentTarget.getBoundingClientRect()
    if (r.width === 0) return
    const vbx = ((e.clientX - r.left) / r.width) * VB_W
    const target = xMax === xMin ? xMin : xMin + ((vbx - PAD_L) / plotW) * (xMax - xMin)
    let best = 0
    for (let i = 1; i < xs.length; i++) {
      if (Math.abs(xs[i] - target) < Math.abs(xs[best] - target)) best = i
    }
    setActive(best)
  }

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const last = xs.length - 1
    const cur = activeIdx ?? 0
    if (e.key === 'Escape') {
      setActive(null)
      return
    }
    let next: number
    if (e.key === 'ArrowRight') next = Math.min(cur + 1, last)
    else if (e.key === 'ArrowLeft') next = Math.max(cur - 1, 0)
    else if (e.key === 'Home') next = 0
    else if (e.key === 'End') next = last
    else return
    e.preventDefault()
    setActive(next)
  }

  const singles = withPoints.filter((s) => s.points.length === 1)

  return (
    <figure className="usage-chart-figure" data-testid={testid}>
      <div className="usage-chart-plot" style={{ height }}>
        <div className="usage-chart-yaxis" aria-hidden="true">
          {yTicks.map((ty) => (
            <span className="usage-chart-tick" key={ty} style={{ top: `${pctY(ty)}%` }}>
              {formatValue(ty)}
            </span>
          ))}
        </div>
        <div
          className="usage-chart-area"
          role="group"
          aria-label={`${summary}. Use arrow keys to read each point.`}
          tabIndex={0}
          onPointerMove={pickAt}
          onPointerLeave={() => setActive(null)}
          onFocus={() => setActive((a) => (a === null ? 0 : a))}
          onBlur={() => setActive(null)}
          onKeyDown={onKeyDown}
          data-testid={testid ? `${testid}-plot` : undefined}
        >
          <svg
            role="img"
            aria-label={summary}
            className="usage-chart"
            viewBox={`0 0 ${VB_W} ${height}`}
            width="100%"
            height={height}
            preserveAspectRatio="none"
          >
            {yTicks.map((ty) => (
              <line
                key={ty}
                className="usage-chart-grid"
                x1={PAD_L}
                x2={VB_W - PAD_R}
                y1={yAt(ty)}
                y2={yAt(ty)}
                vectorEffect="non-scaling-stroke"
              />
            ))}
            {withPoints.map((s) => {
              const pts = s.points
                .map((p) => `${xAt(p.x).toFixed(1)},${yAt(p.y).toFixed(1)}`)
                .join(' ')
              // A one-point series has no line to draw; its HTML marker below
              // is the mark.
              if (s.points.length === 1) return null
              return (
                <g key={s.id}>
                  {fillArea && (
                    <polygon
                      points={`${xAt(s.points[0].x).toFixed(1)},${yAt(yMin).toFixed(1)} ${pts} ${xAt(
                        s.points[s.points.length - 1].x,
                      ).toFixed(1)},${yAt(yMin).toFixed(1)}`}
                      fill={s.color}
                      opacity="0.12"
                    />
                  )}
                  <polyline
                    points={pts}
                    fill="none"
                    stroke={s.color}
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    vectorEffect="non-scaling-stroke"
                    strokeDasharray={s.dash}
                  />
                </g>
              )
            })}
          </svg>
          <div className="usage-chart-overlay" aria-hidden="true">
            {singles.map((s) => (
              <span
                key={`single-${s.id}`}
                className="usage-chart-dot"
                style={{
                  left: `${pctX(s.points[0].x)}%`,
                  top: `${pctY(s.points[0].y)}%`,
                  background: s.color,
                }}
              />
            ))}
            {activeX !== null && (
              <span className="usage-chart-crosshair" style={{ left: `${pctX(activeX)}%` }} />
            )}
            {rows.map(({ s, p }) => (
              <span
                key={`active-${s.id}`}
                className="usage-chart-dot active"
                style={{ left: `${pctX(p.x)}%`, top: `${pctY(p.y)}%`, background: s.color }}
              />
            ))}
          </div>
          {activeX !== null && (
            // Parked in the corner opposite the active point: always readable,
            // never over the crosshair, never clipped by the panel edge.
            <div
              className={
                pctX(activeX) < 50 ? 'usage-chart-tooltip right' : 'usage-chart-tooltip left'
              }
              aria-hidden="true"
              data-testid={testid ? `${testid}-tooltip` : undefined}
            >
              <div className="usage-chart-tooltip-x">{formatX(activeX)}</div>
              {rows.map(({ s, p }) => (
                <div className="usage-chart-tooltip-row" key={s.id}>
                  <span className="usage-chart-tooltip-swatch" style={{ background: s.color }} />
                  <span className="usage-chart-tooltip-label">{s.label}</span>
                  <span className="usage-chart-tooltip-value">
                    {formatValue(p.y)} <span className="usage-chart-tooltip-unit">{unit}</span>
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
      <div className="usage-chart-xaxis" aria-hidden="true">
        <span>{formatX(xMin)}</span>
        <span>{formatX(xMax)}</span>
      </div>
      <figcaption
        className="usage-chart-caption"
        data-testid={testid ? `${testid}-caption` : undefined}
      >
        {xUnit ? `${captionUnit(unit)} per ${xUnit}` : captionUnit(unit)}
      </figcaption>
      <div className="sr-only" role="status" aria-live="polite">
        {readout}
      </div>
    </figure>
  )
}
