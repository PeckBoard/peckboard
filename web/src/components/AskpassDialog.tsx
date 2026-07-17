import { useEffect, useState } from 'react'
import Modal from './Modal'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'

/**
 * Global sudo-password dialog. Mounted once near the app root; it listens
 * for the `peckboard:askpass-request` window event (fanned out from the WS
 * store) and shows a masked password prompt. The password is POSTed to
 * `/api/sessions/{id}/askpass-answer` and cleared from component state
 * immediately — it never lands anywhere else on the client.
 *
 * `peckboard:askpass-resolved` (any tab answered, timeout, or cancel)
 * dismisses an open dialog for the same request.
 */
interface PendingAsk {
  requestId: string
  sessionId: string
  prompt: string
}

export default function AskpassDialog() {
  const [ask, setAsk] = useState<PendingAsk | null>(null)
  const [password, setPassword] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const sessions = useSessionsStore((s) => s.sessions)

  useEffect(() => {
    const onRequest = (e: Event) => {
      const detail = (e as CustomEvent).detail as {
        request_id?: string
        session_id?: string
        prompt?: string
      }
      if (!detail?.request_id || !detail?.session_id) return
      // One dialog at a time — a second concurrent sudo prompt is rare and
      // waits behind the first (the helper long-poll holds it open).
      setAsk((cur) =>
        cur
          ? cur
          : {
              requestId: detail.request_id!,
              sessionId: detail.session_id!,
              prompt: detail.prompt || 'Password:',
            },
      )
    }
    const onResolved = (e: Event) => {
      const detail = (e as CustomEvent).detail as { request_id?: string }
      setAsk((cur) => {
        if (cur && detail?.request_id === cur.requestId) {
          setPassword('')
          setError(null)
          setBusy(false)
          return null
        }
        return cur
      })
    }
    window.addEventListener('peckboard:askpass-request', onRequest)
    window.addEventListener('peckboard:askpass-resolved', onResolved)
    return () => {
      window.removeEventListener('peckboard:askpass-request', onRequest)
      window.removeEventListener('peckboard:askpass-resolved', onResolved)
    }
  }, [])

  if (!ask) return null

  const sessionName = sessions.find((s) => s.id === ask.sessionId)?.name ?? 'a session'

  const respond = async (body: Record<string, unknown>) => {
    setBusy(true)
    setError(null)
    try {
      const res = await authedFetch(`/api/sessions/${ask.sessionId}/askpass-answer`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ request_id: ask.requestId, ...body }),
      })
      // Whether we win the race or another tab already answered (410), the
      // dialog's job is done. Clear the password no matter what.
      setPassword('')
      if (!res.ok && res.status !== 410) {
        const d = await res.json().catch(() => null)
        setError(d?.error ?? `Failed (${res.status}).`)
        setBusy(false)
        return
      }
      setAsk(null)
      setBusy(false)
    } catch {
      setPassword('')
      setError('Failed — server unreachable.')
      setBusy(false)
    }
  }

  const submit = () => {
    if (busy) return
    void respond({ password })
  }
  const cancel = () => {
    if (busy) return
    void respond({ cancel: true })
  }

  return (
    <Modal onClose={cancel} maxWidth={440} data-testid="askpass-dialog">
      <h3>Password needed</h3>
      <p className="form-hint">
        <strong>{sessionName}</strong> is running a command that needs your sudo password on the
        Peckboard host.
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault()
          submit()
        }}
      >
        <div className="form-field">
          <label className="form-label">{ask.prompt}</label>
          <input
            className="form-input"
            type="password"
            value={password}
            autoFocus
            autoComplete="off"
            data-testid="askpass-input"
            onChange={(e) => setPassword(e.target.value)}
          />
        </div>
        <p className="form-hint">
          Sent once to sudo on the host and not stored. It never enters the session transcript.
        </p>
        {error && <p className="form-error">{error}</p>}
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={cancel} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || password.length === 0}
            data-testid="askpass-submit"
          >
            {busy ? 'Sending…' : 'Send password'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
