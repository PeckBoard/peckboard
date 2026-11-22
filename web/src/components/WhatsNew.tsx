import { useEffect, useState } from 'react'
import { authedFetch, useAuthStore } from '../store/auth'
import WHATS_NEW from '../whatsNew.json'
import '../styles/whatsNew.css'

interface WhatsNewEntry {
  version: string
  date: string
  highlights: string[]
}

const ENTRIES: WhatsNewEntry[] = WHATS_NEW

/** `-1 | 0 | 1` comparing dotted numeric versions (`0.1.9` < `0.1.10`). */
function compareVersions(a: string, b: string): number {
  const pa = a.split('.').map((n) => parseInt(n, 10) || 0)
  const pb = b.split('.').map((n) => parseInt(n, 10) || 0)
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0)
    if (d !== 0) return d < 0 ? -1 : 1
  }
  return 0
}
function storageKey(userId: string): string {
  return `peckboard.whatsNew.lastSeen.${userId}`
}

/**
 * The "What's new in X" card after an upgrade: bottom corner, non-blocking,
 * dismissible. Lists the highlights (`whatsNew.json`) of every version newer
 * than the one this user last saw, up to the running one (`GET /api/config`
 * → `version`). Last-seen is per user, in localStorage. The very first load
 * (nothing recorded, e.g. a fresh install) records the version silently, so
 * a new user never sees it.
 */
export default function WhatsNew() {
  const userId = useAuthStore((s) => s.user?.id ?? null)
  const [shown, setShown] = useState<{
    userId: string
    version: string
    entries: WhatsNewEntry[]
  } | null>(null)

  useEffect(() => {
    if (!userId) return
    let cancelled = false
    authedFetch('/api/config')
      .then((res) => (res.ok ? res.json() : null))
      .then((cfg: { version?: string } | null) => {
        const version = cfg?.version
        if (cancelled || !version) return
        const key = storageKey(userId)
        let lastSeen: string | null
        try {
          lastSeen = localStorage.getItem(key)
        } catch {
          return
        }
        if (!lastSeen) {
          // Nothing recorded (fresh install, new browser): start from here.
          localStorage.setItem(key, version)
          return
        }
        const since = lastSeen
        if (compareVersions(version, since) <= 0) return
        const entries = ENTRIES.filter(
          (e) => compareVersions(e.version, since) > 0 && compareVersions(e.version, version) <= 0,
        ).sort((a, b) => compareVersions(b.version, a.version))
        if (entries.length === 0) {
          localStorage.setItem(key, version)
          return
        }
        setShown({ userId, version, entries })
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [userId])

  if (!shown || shown.userId !== userId) return null

  const dismiss = () => {
    try {
      localStorage.setItem(storageKey(shown.userId), shown.version)
    } catch {
      // Storage unavailable: the card just comes back next load.
    }
    setShown(null)
  }

  const multi = shown.entries.length > 1
  return (
    <aside className="whats-new" aria-labelledby="whats-new-title" data-testid="whats-new">
      <div className="whats-new-head">
        <h2 id="whats-new-title" className="whats-new-title">
          What&rsquo;s new in {shown.version}
        </h2>
        <button
          type="button"
          className="whats-new-close"
          aria-label="Dismiss"
          onClick={dismiss}
          data-testid="whats-new-dismiss"
        >
          ×
        </button>
      </div>
      <div className="whats-new-body">
        {shown.entries.map((e) => (
          <section key={e.version} data-testid={`whats-new-version-${e.version}`}>
            {multi && <h3 className="whats-new-version">{e.version}</h3>}
            <ul className="whats-new-list">
              {e.highlights.map((h) => (
                <li key={h}>{h}</li>
              ))}
            </ul>
          </section>
        ))}
      </div>
      <div className="whats-new-actions">
        <button type="button" className="btn-primary btn-sm" onClick={dismiss}>
          Got it
        </button>
      </div>
    </aside>
  )
}
