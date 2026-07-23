import { useEffect, useState } from 'react'
import Modal from './Modal'
import { authedFetch, useAuthStore } from '../store/auth'
import { useSessionsStore } from '../store/sessions'

/**
 * Global dialog prompting the owner of encrypted environment variables for
 * their password when a starting session needs them. Mirrors AskpassDialog:
 * the WS store fans `env-unlock-request` / `env-unlock-resolved` frames out
 * as `peckboard:*` window events; only the tab whose logged-in user owns the
 * vars renders anything — every other tab stays silent. The password is
 * POSTed to `/api/env-vars/unlock-answer` and cleared from component state
 * on every exit path (submit, wrong password, cancel, resolve).
 */
interface PendingUnlock {
  requestId: string
  sessionId: string
  userId: string
  username: string
  varNames: string[]
}

export default function EnvUnlockDialog() {
  const [pending, setPending] = useState<PendingUnlock | null>(null)
  const [password, setPassword] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const user = useAuthStore((s) => s.user)
  const sessions = useSessionsStore((s) => s.sessions)

  useEffect(() => {
    const onRequest = (e: Event) => {
      const frame = (e as CustomEvent).detail as Record<string, unknown> | null
      // The payload rides in the frame's `data` field (top-level fallback in
      // case the frame shape ever flattens).
      const d = (frame?.data ?? frame ?? {}) as {
        request_id?: string
        session_id?: string
        user_id?: string
        username?: string
        var_names?: string[]
      }
      if (!d.request_id || !d.user_id) return
      // One dialog at a time — a second concurrent request waits behind the
      // first (the server holds it open until answer or timeout).
      setPending((cur) =>
        cur
          ? cur
          : {
              requestId: d.request_id!,
              sessionId: d.session_id ?? '',
              userId: d.user_id!,
              username: d.username ?? '',
              varNames: Array.isArray(d.var_names) ? d.var_names : [],
            },
      )
    }
    const onResolved = (e: Event) => {
      const frame = (e as CustomEvent).detail as Record<string, unknown> | null
      const d = (frame?.data ?? frame ?? {}) as { request_id?: string }
      setPending((cur) => {
        if (cur && d.request_id === cur.requestId) {
          setPassword('')
          setError(null)
          setBusy(false)
          return null
        }
        return cur
      })
    }
    window.addEventListener('peckboard:env-unlock-request', onRequest)
    window.addEventListener('peckboard:env-unlock-resolved', onResolved)
    return () => {
      window.removeEventListener('peckboard:env-unlock-request', onRequest)
      window.removeEventListener('peckboard:env-unlock-resolved', onResolved)
    }
  }, [])

  // Only the owner's tab shows the prompt.
  if (!pending || !user || user.id !== pending.userId) return null

  const sessionName = sessions.find((s) => s.id === pending.sessionId)?.name ?? 'A session'

  const respond = async (body: Record<string, unknown>) => {
    setBusy(true)
    setError(null)
    try {
      const res = await authedFetch('/api/env-vars/unlock-answer', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ request_id: pending.requestId, ...body }),
      })
      // Never keep a password in state past the request.
      setPassword('')
      if (res.status === 403) {
        // Wrong password leaves the request pending server-side — retry here.
        setError('Wrong password — try again')
        setBusy(false)
        return
      }
      if (!res.ok && res.status !== 410) {
        const d = (await res.json().catch(() => null)) as { error?: string } | null
        setError(d?.error ?? `Failed (${res.status}).`)
        setBusy(false)
        return
      }
      // Success or 410 (request already gone): the dialog's job is done.
      setPending(null)
      setError(null)
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
    <Modal onClose={cancel} maxWidth={440} data-testid="env-unlock-dialog">
      <h3>Unlock environment variables</h3>
      <p className="form-hint">
        <strong>{sessionName}</strong> needs environment variables ({pending.varNames.join(', ')})
        that are encrypted with <strong>{pending.username}</strong>&rsquo;s password.
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault()
          submit()
        }}
      >
        <div className="form-field">
          <label className="form-label">Password</label>
          <input
            className="form-input"
            type="password"
            value={password}
            autoFocus
            autoComplete="off"
            data-testid="env-unlock-input"
            onChange={(e) => setPassword(e.target.value)}
          />
        </div>
        <p className="form-hint">
          Used once to decrypt your variables on the server and not stored. It never enters the
          session transcript.
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
            data-testid="env-unlock-submit"
          >
            {busy ? 'Sending…' : 'Unlock'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
