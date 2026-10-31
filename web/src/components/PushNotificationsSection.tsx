import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'

/** This browser's Web Push state (`/api/push/*`, `routes/notifications.rs`). */
type PushState = 'loading' | 'unsupported' | 'denied' | 'off' | 'on'

const supported = () =>
  typeof window !== 'undefined' &&
  'serviceWorker' in navigator &&
  'PushManager' in window &&
  'Notification' in window

/** The VAPID public key (base64url) as the bytes `subscribe` wants. */
function keyBytes(b64url: string): Uint8Array<ArrayBuffer> {
  const pad = '='.repeat((4 - (b64url.length % 4)) % 4)
  const raw = atob((b64url + pad).replace(/-/g, '+').replace(/_/g, '/'))
  const out = new Uint8Array(new ArrayBuffer(raw.length))
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i)
  return out
}

async function errorText(res: Response, fallback: string): Promise<string> {
  const body = (await res.json().catch(() => ({}))) as { error?: string }
  return body.error ?? `${fallback} (HTTP ${res.status})`
}

async function readState(): Promise<PushState> {
  if (!supported()) return 'unsupported'
  if (Notification.permission === 'denied') return 'denied'
  const reg = await navigator.serviceWorker.getRegistration()
  const sub = await reg?.pushManager.getSubscription()
  return sub && Notification.permission === 'granted' ? 'on' : 'off'
}
/**
 * Settings → Assistant → Notifications: lets this browser receive a push
 * when the Assistant gets news (a scheduled task, a session update or
 * question) while no Assistant panel is open. Admin-only, like the API.
 */
export default function PushNotificationsSection() {
  const [state, setState] = useState<PushState>(() =>
    !supported() ? 'unsupported' : Notification.permission === 'denied' ? 'denied' : 'loading',
  )
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [note, setNote] = useState('')

  const refresh = useCallback(async () => {
    setState(await readState().catch((): PushState => 'off'))
  }, [])

  useEffect(() => {
    let live = true
    void readState()
      .catch((): PushState => 'off')
      .then((s) => {
        if (live) setState(s)
      })
    return () => {
      live = false
    }
  }, [])

  const enable = async () => {
    setBusy(true)
    setError('')
    setNote('')
    try {
      const permission = await Notification.requestPermission()
      if (permission !== 'granted') {
        setState(permission === 'denied' ? 'denied' : 'off')
        return
      }
      const keyRes = await authedFetch('/api/push/vapid-key')
      if (!keyRes.ok) throw new Error(await errorText(keyRes, "Couldn't load the push key"))
      const { publicKey } = (await keyRes.json()) as { publicKey: string }
      await navigator.serviceWorker.register('/sw.js')
      const reg = await navigator.serviceWorker.ready
      const options = { userVisibleOnly: true, applicationServerKey: keyBytes(publicKey) }
      let sub: PushSubscription
      try {
        sub = await reg.pushManager.subscribe(options)
      } catch (e) {
        // Subscribed earlier under another server key: start over.
        const old = await reg.pushManager.getSubscription()
        if (!old) throw e
        await old.unsubscribe()
        sub = await reg.pushManager.subscribe(options)
      }
      const json = sub.toJSON()
      const res = await authedFetch('/api/push/subscribe', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          endpoint: json.endpoint,
          p256dh: json.keys?.p256dh ?? '',
          auth_key: json.keys?.auth ?? '',
        }),
      })
      if (!res.ok) {
        await sub.unsubscribe().catch(() => {})
        throw new Error(await errorText(res, "Couldn't register this browser"))
      }
      setState('on')
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      await refresh().catch(() => {})
    } finally {
      setBusy(false)
    }
  }

  const disable = async () => {
    setBusy(true)
    setError('')
    setNote('')
    try {
      const reg = await navigator.serviceWorker.getRegistration()
      const sub = await reg?.pushManager.getSubscription()
      if (sub) {
        const res = await authedFetch('/api/push/subscribe', {
          method: 'DELETE',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ endpoint: sub.endpoint }),
        })
        if (!res.ok && res.status !== 404) {
          throw new Error(await errorText(res, "Couldn't unregister this browser"))
        }
        await sub.unsubscribe()
      }
      setState('off')
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const sendTest = async () => {
    setBusy(true)
    setError('')
    setNote('')
    try {
      const res = await authedFetch('/api/push/test', { method: 'POST' })
      if (!res.ok) throw new Error(await errorText(res, 'Test failed'))
      const { delivered, failed } = (await res.json()) as { delivered: number; failed: number }
      setNote(
        failed > 0
          ? `Sent to ${delivered} browser(s); ${failed} failed.`
          : `Sent to ${delivered} browser(s).`,
      )
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const reason =
    state === 'unsupported'
      ? "This browser can't receive notifications here. It needs HTTPS — on iPhone or iPad, add Peckboard to the Home Screen first."
      : state === 'denied'
        ? 'Notifications are blocked for this site. Allow them in the browser’s site settings.'
        : null

  return (
    <section
      className="settings-section"
      data-testid="push-notifications-section"
      data-settings-anchor="push-notifications"
    >
      <h3>Notifications</h3>
      <p className="form-hint">
        Get a notification on this device when the Assistant has news — a scheduled task, a session
        update or question — while no Assistant panel is open. At most one every 30 seconds.
      </p>
      <label className="settings-row settings-row-toggle">
        <input
          type="checkbox"
          checked={state === 'on'}
          disabled={busy || state === 'loading' || reason !== null}
          onChange={(e) => void (e.target.checked ? enable() : disable())}
          data-testid="push-notifications-toggle"
        />
        <span className="settings-label">Notify this browser</span>
      </label>
      {reason && (
        <span className="form-actions-reason" data-testid="push-notifications-reason">
          {reason}
        </span>
      )}
      {state === 'on' && (
        <div className="mirror-test-row">
          <button
            type="button"
            className="btn-secondary btn-sm"
            disabled={busy}
            onClick={() => void sendTest()}
            data-testid="push-notifications-test"
          >
            Send test
          </button>
          {note && (
            <span className="form-hint" data-testid="push-notifications-note">
              {note}
            </span>
          )}
        </div>
      )}
      {error && (
        <p className="form-error" role="alert" data-testid="push-notifications-error">
          {error}
        </p>
      )}
    </section>
  )
}
