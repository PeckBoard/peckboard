import {
  RANGE_PRESETS,
  useUsageStore,
  type RangePreset,
  type ResolvedRange,
} from '../../store/usage'

/** `YYYY-MM-DD` for a local calendar day — the value shape `<input type="date">`
 *  reads and writes. Built from the local parts, not `toISOString()`, which
 *  would shift the day for anyone east or west of UTC. */
function toLocalDay(d: Date): string {
  const mm = `${d.getMonth() + 1}`.padStart(2, '0')
  const dd = `${d.getDate()}`.padStart(2, '0')
  return `${d.getFullYear()}-${mm}-${dd}`
}

function mmdd(d: Date): string {
  return `${`${d.getMonth() + 1}`.padStart(2, '0')}/${`${d.getDate()}`.padStart(2, '0')}`
}

/** The window in words. The end date always carries its year and the start
 *  carries one too when the range crosses a year boundary, so a months-old or
 *  DST-straddling window can't be misread as "recent". `to` is exclusive, so
 *  the last covered instant is a millisecond earlier — that's the day the
 *  caption must name. */
function describeRange(r: ResolvedRange): string {
  const end = new Date(r.to - 1)
  if (r.from == null) return `all recorded usage through ${mmdd(end)}/${end.getFullYear()}`
  const start = new Date(r.from)
  const startLabel =
    start.getFullYear() === end.getFullYear()
      ? mmdd(start)
      : `${mmdd(start)}/${start.getFullYear()}`
  return `${startLabel} – ${mmdd(end)}/${end.getFullYear()}`
}

/** IANA zone name for the caption, e.g. `Europe/London`. The whole dashboard
 *  formats in local time, so this is stated once here instead of on every
 *  axis label and row. */
function localZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'local time'
  } catch {
    return 'local time'
  }
}

function fmtStamp(ts: number): string {
  return new Date(ts).toLocaleTimeString(undefined, {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  })
}

/** The dashboard's date-range picker, refresh control, and as-of caption.
 *
 *  Every panel below is scoped to the range this sets, and the caption names
 *  the window the figures on screen were actually fetched for (the store's
 *  `resolved`, not the live preset) — so an in-flight refetch can never leave
 *  a caption describing a window the numbers don't come from. */
export default function RangeBar() {
  const range = useUsageStore((s) => s.range)
  const resolved = useUsageStore((s) => s.resolved)
  const lastUpdated = useUsageStore((s) => s.lastUpdated)
  const loading = useUsageStore((s) => s.loading)
  const loaded = useUsageStore((s) => s.loaded)
  const setRange = useUsageStore((s) => s.setRange)
  const fetchUsage = useUsageStore((s) => s.fetchUsage)

  // A refetch over data already on screen: the panels keep their figures and
  // only this bar indicates work, rather than blanking the page to a spinner.
  const refreshing = loading && loaded

  const today = toLocalDay(new Date())

  const pick = (preset: RangePreset) => {
    if (preset !== 'custom') {
      setRange({ preset })
      return
    }
    // Seed Custom from the last week so the two date inputs are never empty
    // (an empty bound would silently mean "unbounded").
    const weekAgo = new Date()
    weekAgo.setDate(weekAgo.getDate() - 7)
    setRange({
      preset: 'custom',
      fromDay: range.fromDay ?? toLocalDay(weekAgo),
      toDay: range.toDay ?? today,
    })
  }

  return (
    <div className="usage-rangebar" data-testid="usage-rangebar">
      <div className="usage-rangebar-controls">
        <div className="usage-seg" role="group" aria-label="Date range">
          {RANGE_PRESETS.map((p) => (
            <button
              key={p.value}
              type="button"
              className={p.value === range.preset ? 'usage-seg-btn active' : 'usage-seg-btn'}
              aria-pressed={p.value === range.preset}
              title={p.title}
              onClick={() => pick(p.value)}
              data-testid={`usage-range-${p.value}`}
            >
              {p.label}
            </button>
          ))}
        </div>

        {range.preset === 'custom' && (
          <div className="usage-range-custom">
            <label className="usage-range-date">
              <span>From</span>
              <input
                type="date"
                value={range.fromDay ?? ''}
                max={range.toDay ?? today}
                onChange={(e) => setRange({ ...range, preset: 'custom', fromDay: e.target.value })}
                data-testid="usage-range-from"
              />
            </label>
            <label className="usage-range-date">
              <span>To</span>
              <input
                type="date"
                value={range.toDay ?? ''}
                min={range.fromDay}
                max={today}
                onChange={(e) => setRange({ ...range, preset: 'custom', toDay: e.target.value })}
                data-testid="usage-range-to"
              />
            </label>
          </div>
        )}

        <button
          type="button"
          className={refreshing ? 'usage-refresh-btn refreshing' : 'usage-refresh-btn'}
          onClick={() => void fetchUsage()}
          disabled={loading}
          aria-busy={refreshing}
          aria-label="Refresh usage data"
          data-testid="usage-refresh"
        >
          <span className="usage-refresh-icon" aria-hidden="true">
            ⟳
          </span>
          Refresh
        </button>
      </div>

      <p className="usage-range-caption" data-testid="usage-range-caption" aria-live="polite">
        Showing {describeRange(resolved)} · times in {localZone()}
        {lastUpdated > 0 && (
          <>
            {' · '}
            <span data-testid="usage-updated">updated {fmtStamp(lastUpdated)}</span>
          </>
        )}
        {refreshing && <span className="usage-range-refreshing"> · refreshing…</span>}
      </p>
    </div>
  )
}
