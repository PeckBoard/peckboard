import { useEffect, useState, type KeyboardEvent } from 'react'
import FieldError from './FieldError'
import { authedFetch } from '../store/auth'
import { useLinkPreviewStore, type AutoEmbeds } from '../store/linkPreviews'
import '../styles/link-preview.css'

const AUTO_OPTIONS: { value: AutoEmbeds; label: string }[] = [
  { value: 'off', label: 'Off' },
  { value: 'trusted', label: 'Trusted providers' },
  { value: 'all', label: 'All' },
]

interface AdminSettings {
  trusted_internal_hosts: string[]
  screenshot_fallback: boolean
}

const MAX_HOSTS = 100

/** Mirrors `TrustedHost::parse` (service::link_preview::ssrf): a host name,
 *  an IP, or an IP/CIDR range. */
function trustedHostError(raw: string): string | null {
  const s = raw.trim().replace(/\.$/, '').toLowerCase()
  if (!s) return 'Enter a host name, IP, or CIDR range.'
  const ipv4 = /^(25[0-5]|2[0-4]\d|1?\d?\d)(\.(25[0-5]|2[0-4]\d|1?\d?\d)){3}$/
  const ipv6 = /^\[?[0-9a-f:.]*:[0-9a-f:.]*\]?$/
  const [ip, bits, extra] = s.split('/')
  if (bits !== undefined) {
    const max = ipv4.test(ip) ? 32 : ipv6.test(ip) ? 128 : -1
    if (max < 0 || extra !== undefined || !/^\d{1,3}$/.test(bits) || Number(bits) > max) {
      return `“${raw.trim()}” is not a valid CIDR range (e.g. 10.0.0.0/8).`
    }
    return null
  }
  if (ipv4.test(s) || ipv6.test(s)) return null
  const labelOk = (l: string) =>
    l.length > 0 && l.length <= 63 && /^[a-z0-9_-]+$/.test(l) && !/^-|-$/.test(l)
  if (s.length <= 253 && s.split('.').every(labelOk)) return null
  return `“${raw.trim()}” is not a host name, IP, or CIDR range.`
}

function AdminLinkPreviewSettings() {
  const [settings, setSettings] = useState<AdminSettings | null>(null)
  const [draft, setDraft] = useState('')
  const [draftError, setDraftError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/settings/link-preview')
      .then((res) => (res.ok ? (res.json() as Promise<AdminSettings>) : Promise.reject()))
      .then((data) => {
        if (!cancelled) setSettings(data)
      })
      .catch(() => {
        if (cancelled) return
        setError('Could not load link preview settings.')
        setSettings({ trusted_internal_hosts: [], screenshot_fallback: false })
      })
    return () => {
      cancelled = true
    }
  }, [])

  if (!settings) return <p className="settings-loading">Loading link preview settings…</p>

  const update = (next: AdminSettings) => {
    setSaved(false)
    setSettings(next)
  }
  const addHost = () => {
    const host = draft.trim()
    if (!host) return
    const problem =
      trustedHostError(host) ??
      (settings.trusted_internal_hosts.some((h) => h.toLowerCase() === host.toLowerCase())
        ? `“${host}” is already in the list.`
        : settings.trusted_internal_hosts.length >= MAX_HOSTS
          ? `At most ${MAX_HOSTS} trusted hosts.`
          : null)
    setDraftError(problem)
    if (problem) return
    update({ ...settings, trusted_internal_hosts: [...settings.trusted_internal_hosts, host] })
    setDraft('')
  }
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter' || e.key === ',') {
      e.preventDefault()
      addHost()
    }
  }
  const save = async () => {
    setSaving(true)
    setError(null)
    try {
      const res = await authedFetch('/api/settings/link-preview', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(settings),
      })
      if (!res.ok) {
        const body = (await res.json().catch(() => null)) as { error?: string } | null
        throw new Error(body?.error ?? `HTTP ${res.status}`)
      }
      setSaved(true)
    } catch (e) {
      setError(`Could not save: ${(e as Error).message}`)
    } finally {
      setSaving(false)
    }
  }

  return (
    <>
      <div className="link-preview-hosts">
        <label htmlFor="link-preview-host-input">
          <span className="settings-label">Trusted internal hosts</span>
          <span className="form-hint">
            Link previews never fetch private or loopback addresses — except these host names, IPs,
            or CIDR ranges (e.g. <code>nas.lan</code>, <code>10.0.0.0/8</code>).
          </span>
        </label>
        {settings.trusted_internal_hosts.length > 0 && (
          <ul className="tls-san-list" data-testid="link-preview-hosts">
            {settings.trusted_internal_hosts.map((h) => (
              <li key={h} className="tls-san link-preview-host">
                {h}
                <button
                  type="button"
                  className="link-preview-host-remove"
                  aria-label={`Remove ${h}`}
                  onClick={() =>
                    update({
                      ...settings,
                      trusted_internal_hosts: settings.trusted_internal_hosts.filter(
                        (x) => x !== h,
                      ),
                    })
                  }
                >
                  {'×'}
                </button>
              </li>
            ))}
          </ul>
        )}
        <div className="link-preview-host-add">
          <input
            id="link-preview-host-input"
            type="text"
            className="form-input"
            placeholder="nas.lan or 192.168.1.0/24"
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value)
              setDraftError(null)
            }}
            onKeyDown={onKeyDown}
            data-testid="link-preview-host-input"
          />
          <button
            type="button"
            className="btn-secondary"
            onClick={addHost}
            disabled={!draft.trim()}
            data-testid="link-preview-host-add"
          >
            Add
          </button>
        </div>
        <FieldError message={draftError ?? undefined} testId="link-preview-host-error" />
      </div>

      <label className="settings-row link-preview-screenshot">
        <span>
          <span className="settings-label">Screenshot fallback</span>
          <span className="form-hint">
            For pages without a preview image, render one. This runs a headless browser on the
            server against links posted in chat — only enable it if you trust what gets linked.
          </span>
        </span>
        <input
          type="checkbox"
          checked={settings.screenshot_fallback}
          onChange={(e) => update({ ...settings, screenshot_fallback: e.target.checked })}
          data-testid="link-preview-screenshot"
        />
      </label>

      {error && (
        <p className="form-error" role="alert" data-testid="link-preview-settings-error">
          {error}
        </p>
      )}
      <div className="settings-row">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => void save()}
          disabled={saving}
          data-testid="link-preview-settings-save"
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
        {saved && <span className="form-hint">Saved.</span>}
      </div>
    </>
  )
}

/**
 * Settings → Chat & Models → Link Previews. "Auto-load embeds" is a
 * per-browser preference (localStorage); admins also get the server-wide
 * trusted internal hosts and the screenshot fallback switch.
 */
export default function LinkPreviewSettingsSection({ isAdmin }: { isAdmin: boolean }) {
  const autoEmbeds = useLinkPreviewStore((s) => s.autoEmbeds)
  const setAutoEmbeds = useLinkPreviewStore((s) => s.setAutoEmbeds)
  return (
    <section
      className="settings-section"
      data-testid="link-preview-section"
      data-settings-anchor="link-previews"
    >
      <h3>Link Previews</h3>
      <p className="form-hint">
        Links in chat show a preview card. Embeds (videos, posts) load only when you click them
        unless you auto-load them here — loading one contacts that site. Applies to this browser.
      </p>
      <span className="settings-label link-preview-auto-label" id="link-preview-auto-label">
        Auto-load embeds
      </span>
      <div className="theme-toggle" role="group" aria-labelledby="link-preview-auto-label">
        {AUTO_OPTIONS.map((o) => (
          <button
            key={o.value}
            type="button"
            className={`theme-btn ${autoEmbeds === o.value ? 'active' : ''}`}
            aria-pressed={autoEmbeds === o.value}
            onClick={() => setAutoEmbeds(o.value)}
            data-testid={`link-preview-auto-${o.value}`}
          >
            {o.label}
          </button>
        ))}
      </div>
      {isAdmin && <AdminLinkPreviewSettings />}
    </section>
  )
}
