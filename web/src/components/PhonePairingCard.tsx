import { useEffect, useState } from 'react'
import { authedFetch, useAuthStore } from '../store/auth'
import { docsUrl } from '../utils/helpTerms'
import '../styles/phonePairing.css'

/** `GET /api/pairing/qr` — see `src/routes/pairing.rs`. */
interface PairingQr {
  url: string | null
  qr_svg: string | null
  reachable: boolean
  reason: 'no_https' | 'localhost' | null
}

const REASON_TEXT: Record<'no_https' | 'localhost', string> = {
  no_https:
    'This PeckBoard isn’t using a secure connection (HTTPS) yet, so your phone can’t open it safely.',
  localhost:
    'This page is open on “localhost”, which only this computer can reach, and PeckBoard knows no other address for it yet.',
}

/** In-app navigation to a Settings page, the way other deep links do it. */
function openSettings(page: 'tls' | 'remote-access') {
  const path = `/settings/${page}`
  if (window.location.pathname !== path) window.history.pushState(null, '', path)
  window.dispatchEvent(new PopStateEvent('popstate'))
}

/**
 * "Use PeckBoard on your phone": a QR code of the address a phone's browser
 * can open this instance at, plus where to get the PeckBoard app. When the
 * instance is only reachable on localhost or without HTTPS, it shows the
 * Settings → Secure connection (HTTPS) guidance instead of a QR code.
 *
 * Pairing the PeckBoard *app* is a separate, admin-only flow (one-time link
 * through the relay, Settings → Remote Access); this card links there.
 */
export default function PhonePairingCard({ showTitle = true }: { showTitle?: boolean }) {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const [data, setData] = useState<PairingQr | null>(null)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    let cancelled = false
    const origin = encodeURIComponent(window.location.origin)
    authedFetch(`/api/pairing/qr?origin=${origin}`)
      .then((res) => (res.ok ? (res.json() as Promise<PairingQr>) : Promise.reject()))
      .then((d) => {
        if (!cancelled) setData(d)
      })
      .catch(() => {
        if (!cancelled) setFailed(true)
      })
    return () => {
      cancelled = true
    }
  }, [])

  return (
    <div className="phone-pairing-card" data-testid="phone-pairing-card">
      {showTitle && <h3 className="phone-pairing-title">Use PeckBoard on your phone</h3>}
      {failed ? (
        <p className="form-hint" data-testid="phone-pairing-error">
          Couldn&rsquo;t work out this PeckBoard&rsquo;s address. Try again later.
        </p>
      ) : !data ? (
        <p className="form-hint">Loading…</p>
      ) : data.reachable && data.url ? (
        <div className="phone-pairing-qr-row">
          {data.qr_svg && (
            <div
              className="mfa-qr phone-pairing-qr"
              data-testid="phone-pairing-qr"
              dangerouslySetInnerHTML={{ __html: data.qr_svg }}
            />
          )}
          <div className="phone-pairing-qr-text">
            <p className="form-hint" style={{ marginTop: 0 }}>
              Scan this with your phone&rsquo;s camera to open PeckBoard there. Your phone needs to
              be able to reach this computer (for example, the same Wi-Fi).
            </p>
            <code className="phone-pairing-url" data-testid="phone-pairing-url">
              {data.url}
            </code>
          </div>
        </div>
      ) : (
        <div data-testid="phone-pairing-https-guidance">
          <p className="form-hint" style={{ marginTop: 0 }}>
            {REASON_TEXT[data.reason ?? 'no_https']}{' '}
            {isAdmin
              ? 'Turn it on in Settings → Secure connection (HTTPS), then open PeckBoard by its network address.'
              : 'Ask your PeckBoard admin to turn on Settings → Secure connection (HTTPS).'}
          </p>
          {isAdmin && (
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={() => openSettings('tls')}
              data-testid="phone-pairing-open-tls"
            >
              Open Secure connection (HTTPS)
            </button>
          )}
        </div>
      )}
      <p className="form-hint phone-pairing-app">
        Prefer an app? Get the{' '}
        <a
          href={docsUrl('/downloads.html')}
          target="_blank"
          rel="noreferrer"
          data-testid="phone-pairing-downloads"
        >
          PeckBoard app
        </a>
        {isAdmin ? (
          <>
            {' '}
            and pair it under{' '}
            <button
              type="button"
              className="btn-link"
              onClick={() => openSettings('remote-access')}
              data-testid="phone-pairing-open-remote"
            >
              Settings → Remote Access
            </button>{' '}
            — that also works away from home.
          </>
        ) : (
          '.'
        )}
      </p>
    </div>
  )
}
