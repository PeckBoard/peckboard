import { useState } from 'react'
import type { LinkPreviewData } from '../store/linkPreviews'
import { FORGE_TYPES } from '../lib/linkPreviewDetails'

/** Card bodies for a preview's typed `details` (`service::link_preview`):
 *  forge (GitHub-shaped repo / PR / issue / run / commit), package, the
 *  generic `badge` + `facts` row, plus the fediverse post, calendar event
 *  and 3D-asset cards. */

type Details = Record<string, unknown>

const str = (v: unknown): string | null =>
  typeof v === 'string' && v !== '' ? v : typeof v === 'number' ? String(v) : null

const strs = (v: unknown): string[] =>
  Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string' && x !== '') : []
function compact(n: number): string {
  if (n < 1000) return String(n)
  return new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(
    n,
  )
}

function Badge({ label, tone }: { label: string; tone: string }) {
  return (
    <span className={`link-badge link-badge-${tone}`} data-testid="link-card-state">
      {label}
    </span>
  )
}

/** Forge state badge + the facts worth a glance (author, number, size). */
function ForgeLine({ d }: { d: Details }) {
  const type = d.type
  let badge: { label: string; tone: string } | null = null
  if (type === 'pull') {
    badge = d.merged
      ? { label: 'Merged', tone: 'merged' }
      : d.draft
        ? { label: 'Draft', tone: 'muted' }
        : d.state === 'open'
          ? { label: 'Open', tone: 'ok' }
          : { label: 'Closed', tone: 'danger' }
  } else if (type === 'issue') {
    badge =
      d.state === 'open'
        ? { label: 'Open', tone: 'ok' }
        : d.state_reason === 'not_planned'
          ? { label: 'Not planned', tone: 'muted' }
          : { label: 'Closed', tone: 'merged' }
  } else if (type === 'run') {
    const c = str(d.conclusion) ?? str(d.status)
    if (c)
      badge = {
        label: c.replace(/_/g, ' '),
        tone: c === 'success' ? 'ok' : c === 'failure' ? 'danger' : 'muted',
      }
  }
  const bits: string[] = []
  const num = str(d.number) ?? str(d.run_number)
  if (num) bits.push(`#${num}`)
  const author = str(d.author) ?? str(d.login)
  if (author) bits.push(author)
  if (type === 'commit' && str(d.sha)) bits.push(String(d.sha))
  if (type === 'repo') {
    if (str(d.language)) bits.push(String(d.language))
    if (str(d.stars)) bits.push(`★ ${d.stars}`)
    if (str(d.forks)) bits.push(`${d.forks} forks`)
  }
  if (typeof d.additions === 'number' && typeof d.deletions === 'number') {
    bits.push(`+${d.additions} −${d.deletions}`)
  }
  if (type === 'run' && str(d.head_branch)) bits.push(String(d.head_branch))
  const checks = str(d.checks)
  if (checks) bits.push(`checks: ${checks}`)
  if (!badge && bits.length === 0) return null
  return (
    <span className="link-card-meta" data-testid="link-card-github">
      {badge && <Badge {...badge} />}
      {bits.map((b) => (
        <span key={b}>{b}</span>
      ))}
    </span>
  )
}

function PackageLine({ d }: { d: Details }) {
  const version = str(d.version)
  const license = str(d.license)
  const downloads = typeof d.downloads === 'number' ? d.downloads : null
  const period = str(d.downloads_period)
  return (
    <span className="link-card-meta" data-testid="link-card-package">
      {str(d.registry) && <span className="link-card-registry">{String(d.registry)}</span>}
      {version && <span className="link-badge link-badge-ok">v{version.replace(/^v/, '')}</span>}
      {license && <span>{license}</span>}
      {downloads !== null && (
        <span title="Downloads">
          {'↓'} {compact(downloads)}
          {period ? (period === 'pulls' ? ' pulls' : `/${period}`) : ''}
        </span>
      )}
    </span>
  )
}

function FactsLine({ d }: { d: Details }) {
  const badge = d.badge as { label?: unknown; tone?: unknown } | null | undefined
  const label = badge ? str(badge.label) : null
  const facts = strs(d.facts)
  if (!label && facts.length === 0) return null
  return (
    <span className="link-card-meta" data-testid="link-card-facts" data-type={String(d.type)}>
      {label && <Badge label={label} tone={str(badge?.tone) ?? 'muted'} />}
      {facts.map((f) => (
        <span key={f}>{f}</span>
      ))}
    </span>
  )
}

/** The meta row under a card's title for its `details`. */
export function DetailsLine({ d }: { d: Details }) {
  if (FORGE_TYPES.has(String(d.type))) return <ForgeLine d={d} />
  if (d.type === 'package') return <PackageLine d={d} />
  return <FactsLine d={d} />
}

// ── Fediverse post ────────────────────────────────────────────────────

function relativeDate(iso: string): string {
  const t = Date.parse(iso)
  if (!Number.isFinite(t)) return ''
  return new Date(t).toLocaleDateString(undefined, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  })
}

/** A post from any fediverse instance: author row, text, media thumbs. */
export function PostCard({ p }: { p: LinkPreviewData }) {
  const d = p.details ?? {}
  const href = p.final_url || p.url
  const [revealed, setRevealed] = useState(false)
  const warning = str(d.warning)
  const thumbs = strs(d.thumbs)
  const counts = (d.counts ?? {}) as Record<string, unknown>
  const handle = str(d.handle)
  const created = str(d.created_at)
  const hidden = !!warning && !revealed
  return (
    <div className="link-card link-card-post" data-testid="link-post" data-software={p.provider}>
      <a className="link-post-head" href={href} target="_blank" rel="noreferrer noopener">
        {str(d.avatar) && (
          <img className="link-post-avatar" src={String(d.avatar)} alt="" loading="lazy" />
        )}
        <span className="link-post-who">
          <span className="link-post-author" data-testid="link-post-author">
            {str(d.author) ?? handle ?? p.site_name}
          </span>
          {handle && <span className="link-post-handle">{handle}</span>}
        </span>
        <span className="link-post-site">{p.site_name}</span>
      </a>
      {warning && (
        <button type="button" className="link-post-cw" onClick={() => setRevealed((v) => !v)}>
          <span className="link-badge link-badge-muted">CW</span> {warning}{' '}
          <span className="link-post-cw-toggle">{revealed ? 'Hide' : 'Show'}</span>
        </button>
      )}
      {!hidden && (
        <>
          {p.title && str(d.community) && <span className="link-card-title">{p.title}</span>}
          {str(d.text) && (
            <p className="link-post-text" data-testid="link-post-text">
              {String(d.text)}
            </p>
          )}
          {thumbs.length > 0 && (
            <a
              className={`link-post-media link-post-media-${Math.min(thumbs.length, 4)}`}
              href={href}
              target="_blank"
              rel="noreferrer noopener"
            >
              {thumbs.map((t) => (
                <img key={t} src={t} alt="" loading="lazy" />
              ))}
            </a>
          )}
        </>
      )}
      <span className="link-card-meta link-post-meta">
        {str(d.community) && <span>!{String(d.community)}</span>}
        {typeof counts.replies === 'number' && <span>{counts.replies} replies</span>}
        {typeof counts.boosts === 'number' && <span>{counts.boosts} boosts</span>}
        {typeof counts.likes === 'number' && <span>{counts.likes} likes</span>}
        {typeof counts.score === 'number' && <span>{counts.score} points</span>}
        {created && <span>{relativeDate(created)}</span>}
      </span>
    </div>
  )
}

// ── Calendar event ────────────────────────────────────────────────────

const DAY_MS = 24 * 3600 * 1000

function parseWhen(v: string, allDay: boolean): Date | null {
  // A bare date is a calendar day, not UTC midnight.
  const t = allDay ? Date.parse(`${v}T00:00:00`) : Date.parse(v)
  return Number.isFinite(t) ? new Date(t) : null
}

function eventTime(d: Details): string {
  const allDay = d.all_day === true
  const start = str(d.start) ? parseWhen(String(d.start), allDay) : null
  if (!start) return ''
  const end = str(d.end) ? parseWhen(String(d.end), allDay) : null
  const dateOpts: Intl.DateTimeFormatOptions = {
    weekday: 'short',
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  }
  if (allDay) {
    const first = start.toLocaleDateString(undefined, dateOpts)
    // DTEND of an all-day event is exclusive.
    const last = end ? new Date(end.getTime() - DAY_MS) : null
    return last && last.getTime() > start.getTime()
      ? `${first} – ${last.toLocaleDateString(undefined, dateOpts)}`
      : first
  }
  const timeOpts: Intl.DateTimeFormatOptions = { hour: 'numeric', minute: '2-digit' }
  const head = `${start.toLocaleDateString(undefined, dateOpts)}, ${start.toLocaleTimeString(undefined, timeOpts)}`
  if (!end) return head
  const sameDay = end.toDateString() === start.toDateString()
  return sameDay
    ? `${head} – ${end.toLocaleTimeString(undefined, timeOpts)}`
    : `${head} – ${end.toLocaleDateString(undefined, dateOpts)}, ${end.toLocaleTimeString(undefined, timeOpts)}`
}

function icsEscape(s: string): string {
  return s.replace(/\\/g, '\\\\').replace(/;/g, '\\;').replace(/,/g, '\\,').replace(/\n/g, '\\n')
}

/** `2026-11-01` / `2026-11-01T09:00:00Z` / floating → an iCalendar value. */
function icsWhen(v: string, allDay: boolean): string {
  const digits = v.replace(/[-:]/g, '')
  return allDay ? `;VALUE=DATE:${digits.slice(0, 8)}` : `:${digits.replace(/\.\d+/, '')}`
}

function downloadEvent(p: LinkPreviewData) {
  const d = p.details ?? {}
  const allDay = d.all_day === true
  const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+/, '')
  const lines = [
    'BEGIN:VCALENDAR',
    'VERSION:2.0',
    'PRODID:-//Peckboard//Link preview//EN',
    'BEGIN:VEVENT',
    `UID:${stamp}-${Math.random().toString(36).slice(2)}@peckboard`,
    `DTSTAMP:${stamp}`,
    `DTSTART${icsWhen(String(d.start), allDay)}`,
  ]
  if (str(d.end)) lines.push(`DTEND${icsWhen(String(d.end), allDay)}`)
  lines.push(`SUMMARY:${icsEscape(p.title ?? 'Event')}`)
  if (str(d.location)) lines.push(`LOCATION:${icsEscape(String(d.location))}`)
  if (p.description) lines.push(`DESCRIPTION:${icsEscape(p.description)}`)
  lines.push(`URL:${p.final_url || p.url}`, 'END:VEVENT', 'END:VCALENDAR')
  const blob = new Blob([lines.join('\r\n') + '\r\n'], { type: 'text/calendar' })
  const href = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = href
  a.download = `${(p.title ?? 'event').replace(/[^\w.-]+/g, '_').slice(0, 60)}.ics`
  document.body.appendChild(a)
  a.click()
  a.remove()
  setTimeout(() => URL.revokeObjectURL(href), 1000)
}

/** An `.ics` link: title, time in the viewer's zone, location, and an
 *  "Add to calendar" download. */
export function EventCard({ p }: { p: LinkPreviewData }) {
  const d = p.details ?? {}
  const href = p.final_url || p.url
  const when = eventTime(d)
  const more = typeof d.events === 'number' && d.events > 1 ? d.events - 1 : 0
  return (
    <div className="link-card link-card-event" data-testid="link-event">
      <div className="link-event-body">
        <div className="link-event-date" aria-hidden="true">
          {str(d.start) && (
            <>
              <span className="link-event-month">
                {parseWhen(String(d.start), d.all_day === true)?.toLocaleDateString(undefined, {
                  month: 'short',
                })}
              </span>
              <span className="link-event-day">
                {parseWhen(String(d.start), d.all_day === true)?.getDate()}
              </span>
            </>
          )}
        </div>
        <div className="link-card-body">
          <a
            className="link-card-title"
            href={href}
            target="_blank"
            rel="noreferrer noopener"
            data-testid="link-event-title"
          >
            {p.title ?? 'Event'}
          </a>
          {when && (
            <span className="link-event-when" data-testid="link-event-when">
              {when}
            </span>
          )}
          {str(d.location) && <span className="link-card-desc">{String(d.location)}</span>}
          {more > 0 && (
            <span className="link-card-note">
              +{more} more event{more === 1 ? '' : 's'}
            </span>
          )}
        </div>
      </div>
      <div className="link-card-actions">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => downloadEvent(p)}
          data-testid="link-event-add"
        >
          Add to calendar
        </button>
      </div>
    </div>
  )
}

/** Download action under a 3D-asset card when the page links a model file. */
export function AssetActions({ d }: { d: Details }) {
  const src = str(d.download_src)
  const raw = str(d.download_url)
  if (!src || !raw) return null
  const name = raw.split(/[?#]/)[0].split('/').pop() || 'model'
  return (
    <div className="link-card-actions" data-testid="link-asset-actions">
      <a className="btn-secondary" href={src} download={name}>
        Download {name}
      </a>
    </div>
  )
}
