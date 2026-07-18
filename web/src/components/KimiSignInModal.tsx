import { useEffect, useRef, useState } from 'react'
import { useKimiAccountsStore } from '../store/kimiAccounts'
import type { KimiAccount } from '../types/api'
import Modal from './Modal'

interface Props {
  account: KimiAccount
  onClose: () => void
}

/**
 * Device-code sign-in for a Kimi `device` account — the in-app equivalent of
 * `kimi login`. Pressing "Sign in" asks the server to spawn the `kimi` CLI,
 * which returns a `www.kimi.com/code/authorize_device` URL; the user opens it
 * and authorises in the browser while the CLI polls. We poll the account list
 * until it reads as `authenticated`, then show success. Mirrors
 * {@link GrokSignInModal}.
 */
export default function KimiSignInModal({ account, onClose }: Props) {
  const startLogin = useKimiAccountsStore((s) => s.startLogin)
  const fetchAccounts = useKimiAccountsStore((s) => s.fetchAccounts)
  // Re-read the live row so we notice the moment it flips to authenticated.
  const live = useKimiAccountsStore((s) => s.accounts.find((a) => a.id === account.id))
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
      setError(err instanceof Error ? err.message : 'Failed to start Kimi login')
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
    <Modal onClose={onClose} data-testid="kimi-signin-modal">
      <h2>Sign in to Kimi</h2>
      <p className="form-hint">
        Account: <strong>{account.name}</strong>
      </p>

      {authenticated ? (
        <div className="form-field" data-testid="kimi-signin-done">
          <p className="form-success">✓ Signed in. This account is ready to use.</p>
        </div>
      ) : error ? (
        <div className="form-field">
          <p className="form-error" data-testid="kimi-signin-error">
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
            data-testid="kimi-signin-url"
          >
            Open Kimi sign-in ↗
          </a>
          <span className="form-hint">
            Open the link, confirm the code shown, and approve in your browser. This dialog updates
            automatically once you&apos;re signed in.
          </span>
          <p className="settings-loading" data-testid="kimi-signin-waiting">
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
            data-testid="kimi-signin-start"
          >
            {starting ? 'Starting…' : 'Sign in with Kimi'}
          </button>
        </div>
      )}

      <div className="form-actions">
        <button
          type="button"
          className="btn-primary"
          onClick={onClose}
          data-testid="kimi-signin-close"
        >
          {authenticated ? 'Done' : 'Close'}
        </button>
      </div>
    </Modal>
  )
}
