import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useProviderSetupStore } from '../store/providerSetup'
import Modal from './Modal'

interface Props {
  onClose: () => void
}

/**
 * Browser sign-in for Cursor — the in-app equivalent of `cursor-agent login`.
 * The server spawns the CLI and returns the link it prints; the CLI completes
 * the sign-in once the user approves it, and we poll until it reports signed
 * in. Cursor has one host login rather than accounts, so there is nothing to
 * create first.
 */
export default function CursorSignInModal({ onClose }: Props) {
  const refreshOne = useProviderSetupStore((s) => s.refreshOne)
  const [url, setUrl] = useState('')
  const [error, setError] = useState('')
  const [starting, setStarting] = useState(false)
  const [signedIn, setSignedIn] = useState(false)

  const begin = async () => {
    setError('')
    setStarting(true)
    try {
      const res = await authedFetch('/api/provider-setup/cursor/login/start', { method: 'POST' })
      const body = (await res.json().catch(() => null)) as { url?: string; error?: string } | null
      if (!res.ok || !body?.url) throw new Error(body?.error || 'Failed to start Cursor login')
      setUrl(body.url)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start Cursor login')
    } finally {
      setStarting(false)
    }
  }

  useEffect(() => {
    if (!url || signedIn) return
    const timer = setInterval(() => {
      void refreshOne('cursor').then((next) => {
        if (next?.signed_in) setSignedIn(true)
      })
    }, 3000)
    return () => clearInterval(timer)
  }, [url, signedIn, refreshOne])

  return (
    <Modal onClose={onClose} data-testid="cursor-signin-modal">
      <h2>Sign in to Cursor</h2>

      {signedIn ? (
        <div className="form-field" data-testid="cursor-signin-done">
          <p className="form-success">✓ Signed in. Cursor is ready to use.</p>
        </div>
      ) : error ? (
        <div className="form-field">
          <p className="form-error" data-testid="cursor-signin-error">
            {error}
          </p>
          <button type="button" className="btn-secondary" onClick={() => void begin()}>
            Try again
          </button>
        </div>
      ) : url ? (
        <div className="form-field">
          <a
            className="form-link"
            href={url}
            target="_blank"
            rel="noreferrer noopener"
            data-testid="cursor-signin-url"
          >
            Open Cursor sign-in ↗
          </a>
          <span className="form-hint">
            Open the link and approve in your browser. This dialog updates automatically once
            you&apos;re signed in.
          </span>
          <p className="settings-loading" data-testid="cursor-signin-waiting">
            Waiting for authorization…
          </p>
        </div>
      ) : (
        <div className="form-field">
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void begin()}
            disabled={starting}
            autoFocus
            data-testid="cursor-signin-start"
          >
            {starting ? 'Starting…' : 'Sign in with Cursor'}
          </button>
        </div>
      )}

      <div className="form-actions">
        <button
          type="button"
          className="btn-primary"
          onClick={onClose}
          data-testid="cursor-signin-close"
        >
          {signedIn ? 'Done' : 'Close'}
        </button>
      </div>
    </Modal>
  )
}
