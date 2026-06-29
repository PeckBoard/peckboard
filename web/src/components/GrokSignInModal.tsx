import { useEffect, useRef, useState } from 'react'
import { useGrokAccountsStore } from '../store/grokAccounts'
import type { GrokAccount } from '../types/api'
import Modal from './Modal'

interface Props {
  account: GrokAccount
  onClose: () => void
}

/**
 * Device-code sign-in for a Grok `device` account — the in-app equivalent of
 * `grok login --device-auth`. Pressing "Sign in" asks the server to spawn the
 * `grok` CLI, which returns a `accounts.x.ai/oauth2/device` URL; the user opens
 * it and authorises in the browser while the CLI polls. We poll the account
 * list until it reads as `authenticated`, then show success. This mirrors the
 * Claude "Open sign-in ↗" link, but completion is automatic (no code to paste).
 */
export default function GrokSignInModal({ account, onClose }: Props) {
  const startLogin = useGrokAccountsStore((s) => s.startLogin)
  const fetchAccounts = useGrokAccountsStore((s) => s.fetchAccounts)
  // Re-read the live row so we notice the moment it flips to authenticated.
  const live = useGrokAccountsStore((s) => s.accounts.find((a) => a.id === account.id))
  const authenticated = live?.authenticated ?? account.authenticated

  const [url, setUrl] = useState('')
  const [error, setError] = useState('')
  const [starting, setStarting] = useState(false)
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const begin = async () => {
    setError('')
    setStarting(true)
    try {
      const { url } = await startLogin(account.id)
      setUrl(url)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start Grok login')
    } finally {
      setStarting(false)
    }
  }

  // While waiting on a URL (login in flight) and not yet authenticated, poll
  // the account list so the dialog flips to success on its own.
  useEffect(() => {
    if (!url || authenticated) return
    pollRef.current = setInterval(() => {
      void fetchAccounts()
    }, 3000)
    return () => {
      if (pollRef.current) clearInterval(pollRef.current)
    }
  }, [url, authenticated, fetchAccounts])

  return (
    <Modal onClose={onClose} data-testid="grok-signin-modal">
      <h2>Sign in to Grok</h2>
      <p className="form-hint">
        Account: <strong>{account.name}</strong>
      </p>

      {authenticated ? (
        <div className="form-field" data-testid="grok-signin-done">
          <p className="form-success">✓ Signed in. This account is ready to use.</p>
        </div>
      ) : error ? (
        <div className="form-field">
          <p className="form-error" data-testid="grok-signin-error">
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
            data-testid="grok-signin-url"
          >
            Open Grok sign-in ↗
          </a>
          <span className="form-hint">
            Open the link, confirm the code shown, and approve in your browser. This dialog updates
            automatically once you&apos;re signed in.
          </span>
          <p className="settings-loading" data-testid="grok-signin-waiting">
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
            data-testid="grok-signin-start"
          >
            {starting ? 'Starting…' : 'Sign in with Grok'}
          </button>
        </div>
      )}

      <div className="form-actions">
        <button
          type="button"
          className="btn-primary"
          onClick={onClose}
          data-testid="grok-signin-close"
        >
          {authenticated ? 'Done' : 'Close'}
        </button>
      </div>
    </Modal>
  )
}
