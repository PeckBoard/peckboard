import { useEffect, useRef, useState } from 'react'
import { useCodexAccountsStore } from '../store/codexAccounts'
import type { CodexAccount } from '../types/api'
import Modal from './Modal'

interface Props {
  account: CodexAccount
  onClose: () => void
}

/**
 * ChatGPT device-code sign-in for a Codex account — the in-app equivalent of
 * `codex login --device-auth`. Pressing "Sign in" asks the server to spawn
 * the `codex` CLI, which returns a `…/codex/device` URL plus a one-time
 * code; the user opens the URL, enters the code, and authorises while the
 * CLI polls. We poll the account list until it reads as `authenticated`.
 *
 * An account that already reads as authenticated can still be re-signed in:
 * its stored credentials may be stale or broken, and "authenticated" here
 * only means an `auth.json` exists. So when the dialog opens on such an
 * account we show the start screen, not a success screen — success is only
 * claimed once *this* attempt completes. The server stashes the old
 * credentials when the attempt starts, so the flip back to authenticated is
 * a real one.
 */
export default function CodexSignInModal({ account, onClose }: Props) {
  const startLogin = useCodexAccountsStore((s) => s.startLogin)
  const fetchAccounts = useCodexAccountsStore((s) => s.fetchAccounts)
  const live = useCodexAccountsStore((s) => s.accounts.find((a) => a.id === account.id))
  const authenticated = live?.authenticated ?? account.authenticated
  // Whether this dialog opened on an already-signed-in account (a re-sign-in).
  const [openedAuthenticated] = useState(authenticated)

  const [url, setUrl] = useState('')
  const [userCode, setUserCode] = useState('')
  const [error, setError] = useState('')
  const [starting, setStarting] = useState(false)
  const [attempted, setAttempted] = useState(false)
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  // Only call it signed in once this dialog's own attempt has landed —
  // otherwise a re-sign-in would open straight onto "✓ Signed in" with no
  // way to start one.
  const signedIn = authenticated && (attempted || !openedAuthenticated)

  const begin = async () => {
    setError('')
    setStarting(true)
    try {
      const prompt = await startLogin(account.id)
      // Starting a login stashes the account's old credentials server-side,
      // so re-read the list before flipping `attempted` — otherwise the
      // stale "authenticated" still in the store would read as success and
      // park the dialog on "✓ Signed in" (which also stops the poll below).
      await fetchAccounts()
      setUrl(prompt.url)
      setUserCode(prompt.user_code)
      setAttempted(true)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start Codex login')
    } finally {
      setStarting(false)
    }
  }

  useEffect(() => {
    if (!url || signedIn) return
    pollRef.current = setInterval(() => {
      void fetchAccounts()
    }, 3000)
    return () => {
      if (pollRef.current) clearInterval(pollRef.current)
    }
  }, [url, signedIn, fetchAccounts])

  return (
    <Modal onClose={onClose} data-testid="codex-signin-modal">
      <h2>Sign in with ChatGPT</h2>
      <p className="form-hint">
        Account: <strong>{account.name}</strong>
      </p>

      {signedIn ? (
        <div className="form-field" data-testid="codex-signin-done">
          <p className="form-success">✓ Signed in. This account is ready to use.</p>
        </div>
      ) : error ? (
        <div className="form-field">
          <p className="form-error" data-testid="codex-signin-error">
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
            data-testid="codex-signin-url"
          >
            Open ChatGPT sign-in ↗
          </a>
          {userCode && (
            <p className="form-hint" data-testid="codex-signin-code">
              Enter this one-time code: <strong>{userCode}</strong>
            </p>
          )}
          <span className="form-hint">
            Open the link, enter the code, and approve in your browser. This dialog updates
            automatically once you&apos;re signed in.
          </span>
          <p className="settings-loading" data-testid="codex-signin-waiting">
            Waiting for authorization…
          </p>
        </div>
      ) : (
        <div className="form-field">
          {openedAuthenticated && (
            <p className="form-hint" data-testid="codex-signin-resign-hint">
              This account is already signed in. Re-signing in replaces its current credentials —
              use it if Codex is failing with an authentication error.
            </p>
          )}
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void begin()}
            disabled={starting}
            autoFocus
            data-testid="codex-signin-start"
          >
            {starting
              ? 'Starting…'
              : openedAuthenticated
                ? 'Re-sign in with ChatGPT'
                : 'Sign in with ChatGPT'}
          </button>
        </div>
      )}

      <div className="form-actions">
        <button
          type="button"
          className="btn-primary"
          onClick={onClose}
          data-testid="codex-signin-close"
        >
          {signedIn ? 'Done' : 'Close'}
        </button>
      </div>
    </Modal>
  )
}
