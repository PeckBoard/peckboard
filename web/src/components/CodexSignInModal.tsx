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
 */
export default function CodexSignInModal({ account, onClose }: Props) {
  const startLogin = useCodexAccountsStore((s) => s.startLogin)
  const fetchAccounts = useCodexAccountsStore((s) => s.fetchAccounts)
  const live = useCodexAccountsStore((s) => s.accounts.find((a) => a.id === account.id))
  const authenticated = live?.authenticated ?? account.authenticated

  const [url, setUrl] = useState('')
  const [userCode, setUserCode] = useState('')
  const [error, setError] = useState('')
  const [starting, setStarting] = useState(false)
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const begin = async () => {
    setError('')
    setStarting(true)
    try {
      const prompt = await startLogin(account.id)
      setUrl(prompt.url)
      setUserCode(prompt.user_code)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start Codex login')
    } finally {
      setStarting(false)
    }
  }

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
    <Modal onClose={onClose} data-testid="codex-signin-modal">
      <h2>Sign in with ChatGPT</h2>
      <p className="form-hint">
        Account: <strong>{account.name}</strong>
      </p>

      {authenticated ? (
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
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void begin()}
            disabled={starting}
            autoFocus
            data-testid="codex-signin-start"
          >
            {starting ? 'Starting…' : 'Sign in with ChatGPT'}
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
          {authenticated ? 'Done' : 'Close'}
        </button>
      </div>
    </Modal>
  )
}
