// Inline-SVG multi-series line/area chart for the usage trend widgets. No
// charting dependency by design (see the usage dashboard's "no chart lib"
// note); this is the whole renderer. Values are pre-formatted by the caller —
// the chart only does geometry, never pricing.

/** One plotted series. `points` must be ascending in `x` (the backend already
 *  returns trend points ordered by bucket_ts). */
export interface ChartSeries {
  id: string
  label: string
  /** A CSS color token string, e.g. `var(--accent)`. Never a hex literal, so
   *  series auto-theme for dark mode. */
  color: string
  points: { x: number; y: number }[]
}

interface LineChartProps {
  series: ChartSeries[]
  /** Pixel height of the plot; width is fluid (100%). */
  height?: number
  /** Fill the area under the line. Only applied when there is a single series,
   *  since overlapping fills muddy a multi-series chart. */
  area?: boolean
  formatValue: (v: number) => string
  formatX: (x: number) => string
  testid?: string
}

// viewBox geometry. Width is arbitrary (the SVG scales to 100% width via
// preserveAspectRatio="none"); the gutters leave room for axis labels.
const VB_W = 640
const PAD_L = 8
const PAD_R = 12
const PAD_T = 10
const PAD_B = 18
const Y_TICKS = 3

export default function LineChart({
  series,
  height = 180,
  area = false,
  formatValue,
  formatX,
  testid,
}: LineChartProps) {
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

  const yTicks = Array.from({ length: Y_TICKS + 1 }, (_, i) => yMin + ((yMax - yMin) * i) / Y_TICKS)
  const fillArea = area && withPoints.length === 1

  return (
    <svg
      role="img"
      aria-label={`Trend chart of ${withPoints.map((s) => s.label).join(', ')} from ${formatX(
        xMin,
      )} to ${formatX(xMax)}, peak ${formatValue(yMax)}`}
      className="usage-chart"
      viewBox={`0 0 ${VB_W} ${height}`}
      width="100%"
      height={height}
      preserveAspectRatio="none"
      data-testid={testid}
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
        const pts = s.points.map((p) => `${xAt(p.x).toFixed(1)},${yAt(p.y).toFixed(1)}`).join(' ')
        const single = s.points.length === 1
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
            {single ? (
              <circle cx={xAt(s.points[0].x)} cy={yAt(s.points[0].y)} r="3" fill={s.color} />
            ) : (
              <polyline
                points={pts}
                fill="none"
                stroke={s.color}
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                vectorEffect="non-scaling-stroke"
              />
            )}
          </g>
        )
      })}
      {/* y-axis min/max labels (left-anchored inside the plot) */}
      <text className="usage-chart-axis" x={PAD_L} y={yAt(yMax) + 9}>
        {formatValue(yMax)}
      </text>
      <text className="usage-chart-axis" x={PAD_L} y={yAt(yMin) - 3}>
        {formatValue(yMin)}
      </text>
      {/* x-axis start/end labels */}
      <text className="usage-chart-axis" x={PAD_L} y={height - 5} textAnchor="start">
        {formatX(xMin)}
      </text>
      <text className="usage-chart-axis" x={VB_W - PAD_R} y={height - 5} textAnchor="end">
        {formatX(xMax)}
      </text>
    </svg>
  )
}
