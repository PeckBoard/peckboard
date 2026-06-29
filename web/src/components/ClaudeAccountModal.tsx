import { useState, type FormEvent } from 'react'
import { useClaudeAccountsStore } from '../store/claudeAccounts'
import type { ClaudeAccount, ClaudeAccountKind } from '../types/api'
import Modal from './Modal'

interface Props {
  /** Editing an existing account, or `null` to add a new one. */
  account: ClaudeAccount | null
  onClose: () => void
}

/** Preset rolling windows the budget is evaluated over. `null` = no budget. */
const WINDOW_OPTIONS: { label: string; hours: number | null }[] = [
  { label: 'No budget', hours: null },
  { label: 'Per 5 hours', hours: 5 },
  { label: 'Per day', hours: 24 },
  { label: 'Per week', hours: 168 },
  { label: 'Per 30 days', hours: 720 },
]

const KIND_OPTIONS: { value: ClaudeAccountKind; label: string; hint: string }[] = [
  {
    value: 'oauth_token',
    label: 'Subscription',
    hint: 'Sign in with your Claude account in the browser (Pro/Max).',
  },
  {
    value: 'api_key',
    label: 'API key',
    hint: 'Paste an Anthropic API key (sk-ant-…). Billed via the API.',
  },
]

/**
 * Add or edit a Claude account. The "login" flow is paste-a-token: the user
 * brings a subscription token (`claude setup-token`) or an API key. Editing
 * keeps the stored credential when the field is left blank, so a rename or
 * rebudget never has to re-enter the secret.
 */
export default function ClaudeAccountModal({ account, onClose }: Props) {
  const createAccount = useClaudeAccountsStore((s) => s.createAccount)
  const updateAccount = useClaudeAccountsStore((s) => s.updateAccount)
  const startLogin = useClaudeAccountsStore((s) => s.startLogin)
  const editing = account !== null

  const [name, setName] = useState(account?.name ?? '')
  const [kind, setKind] = useState<ClaudeAccountKind>(account?.kind ?? 'oauth_token')
  const [credential, setCredential] = useState('')

  // Browser-login (`oauth_token`) state. `loginUrl` is empty until the user
  // generates one; `verifier`/`state` are the PKCE material echoed back on
  // save; `loginCode` is the `code#state` string pasted from the browser.
  const [loginUrl, setLoginUrl] = useState('')
  const [loginVerifier, setLoginVerifier] = useState('')
  const [loginState, setLoginState] = useState('')
  const [loginCode, setLoginCode] = useState('')
  const [startingLogin, setStartingLogin] = useState(false)
  const [windowHours, setWindowHours] = useState<number | null>(
    account?.budget_window_hours ?? null,
  )
  const [limitUsd, setLimitUsd] = useState(
    account?.budget_limit_usd != null ? String(account.budget_limit_usd) : '',
  )
  const [limitTokens, setLimitTokens] = useState(
    account?.budget_limit_tokens != null ? String(account.budget_limit_tokens) : '',
  )
  const [warnPct, setWarnPct] = useState(Math.round((account?.warn_threshold ?? 0.75) * 100))
  const [criticalPct, setCriticalPct] = useState(
    Math.round((account?.critical_threshold ?? 0.9) * 100),
  )
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  const hasBudget = windowHours !== null
  // A finished browser login is ready to submit once a code has been pasted
  // against a generated PKCE pair.
  const hasLogin = Boolean(loginCode.trim() && loginVerifier && loginState)

  /** Switch credential type, discarding any in-progress login/paste so the
   *  two paths never bleed into each other. */
  const handleKindChange = (next: ClaudeAccountKind) => {
    setKind(next)
    setError('')
    setCredential('')
    setLoginUrl('')
    setLoginVerifier('')
    setLoginState('')
    setLoginCode('')
  }

  /** Generate a Claude authorize URL and reveal the code field. */
  const handleStartLogin = async () => {
    setError('')
    setStartingLogin(true)
    try {
      const { url, verifier, state } = await startLogin()
      setLoginUrl(url)
      setLoginVerifier(verifier)
      setLoginState(state)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start Claude login')
    } finally {
      setStartingLogin(false)
    }
  }

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    if (!name.trim()) {
      setError('Name is required')
      return
    }
    if (kind === 'oauth_token') {
      // A new subscription account must complete the browser login; an edit
      // may keep its current token (no new login).
      if (!editing && !hasLogin) {
        setError('Sign in with Claude to continue')
        return
      }
    } else if (!editing && !credential.trim()) {
      setError('Paste an API key')
      return
    }
    if (warnPct > criticalPct) {
      setError('Warn % must be ≤ critical %')
      return
    }
    const parseNum = (s: string): number | null => {
      const t = s.trim()
      if (!t) return null
      const n = Number(t)
      return Number.isFinite(n) && n > 0 ? n : null
    }
    setLoading(true)
    try {
      const input = {
        name: name.trim(),
        kind,
        credential: kind === 'api_key' ? credential.trim() : '',
        login:
          kind === 'oauth_token' && hasLogin
            ? { code: loginCode.trim(), verifier: loginVerifier, state: loginState }
            : undefined,
        budget_window_hours: hasBudget ? windowHours : null,
        budget_limit_usd: hasBudget ? parseNum(limitUsd) : null,
        budget_limit_tokens: hasBudget ? parseNum(limitTokens) : null,
        warn_threshold: warnPct / 100,
        critical_threshold: criticalPct / 100,
      }
      if (editing) {
        await updateAccount(account.id, input)
      } else {
        await createAccount(input)
      }
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save account')
    } finally {
      setLoading(false)
    }
  }

  return (
    <Modal onClose={onClose} data-testid="claude-account-modal">
      <h2>{editing ? `Edit ${account.name}` : 'Add Claude Account'}</h2>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="acct-name">
            Account name
          </label>
          <input
            id="acct-name"
            className="form-input"
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. Work, Personal"
            autoFocus
            required
            data-testid="acct-name"
          />
        </div>

        <div className="form-field">
          <span className="form-label">Credential type</span>
          <div className="theme-toggle">
            {KIND_OPTIONS.map((k) => (
              <button
                key={k.value}
                type="button"
                className={`theme-btn ${kind === k.value ? 'active' : ''}`}
                onClick={() => handleKindChange(k.value)}
                data-testid={`acct-kind-${k.value}`}
              >
                {k.label}
              </button>
            ))}
          </div>
          <span className="form-hint">{KIND_OPTIONS.find((k) => k.value === kind)?.hint}</span>
        </div>

        {kind === 'api_key' ? (
          <div className="form-field">
            <label className="form-label" htmlFor="acct-credential">
              API key
              {editing && ' (leave blank to keep current)'}
            </label>
            <input
              id="acct-credential"
              className="form-input"
              type="password"
              value={credential}
              onChange={(e) => setCredential(e.target.value)}
              placeholder={editing ? `Keeping ${account.credential_hint}` : 'sk-ant-…'}
              autoComplete="off"
              data-testid="acct-credential"
            />
          </div>
        ) : (
          <div className="form-field">
            <span className="form-label">
              Claude login
              {editing && ' (optional — re-authenticate)'}
            </span>
            {!loginUrl ? (
              <button
                type="button"
                className="btn-secondary"
                onClick={handleStartLogin}
                disabled={startingLogin}
                data-testid="acct-login-start"
              >
                {startingLogin
                  ? 'Generating…'
                  : editing
                    ? 'Re-authenticate with Claude'
                    : 'Generate login URL'}
              </button>
            ) : (
              <>
                <a
                  className="form-link"
                  href={loginUrl}
                  target="_blank"
                  rel="noreferrer noopener"
                  data-testid="acct-login-url"
                >
                  Open Claude sign-in ↗
                </a>
                <span className="form-hint">
                  Sign in, then paste the code Claude shows you back here.
                </span>
                <input
                  id="acct-login-code"
                  className="form-input"
                  type="text"
                  value={loginCode}
                  onChange={(e) => setLoginCode(e.target.value)}
                  placeholder="Paste code here"
                  autoComplete="off"
                  data-testid="acct-login-code"
                />
              </>
            )}
            {editing && !loginUrl && (
              <span className="form-hint">Keeping {account.credential_hint}</span>
            )}
          </div>
        )}

        <div className="form-field">
          <label className="form-label" htmlFor="acct-window">
            Usage budget
          </label>
          <select
            id="acct-window"
            className="form-input"
            value={windowHours === null ? '' : String(windowHours)}
            onChange={(e) => setWindowHours(e.target.value === '' ? null : Number(e.target.value))}
            data-testid="acct-window"
          >
            {WINDOW_OPTIONS.map((w) => (
              <option key={w.label} value={w.hours === null ? '' : String(w.hours)}>
                {w.label}
              </option>
            ))}
          </select>
          <span className="form-hint">Warn as spend in this rolling window nears the cap.</span>
        </div>

        {hasBudget && (
          <>
            <div className="form-row">
              <div className="form-field">
                <label className="form-label" htmlFor="acct-limit-usd">
                  Cost cap (USD)
                </label>
                <input
                  id="acct-limit-usd"
                  className="form-input"
                  type="number"
                  min="0"
                  step="0.01"
                  value={limitUsd}
                  onChange={(e) => setLimitUsd(e.target.value)}
                  placeholder="optional"
                  data-testid="acct-limit-usd"
                />
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="acct-limit-tokens">
                  Token cap
                </label>
                <input
                  id="acct-limit-tokens"
                  className="form-input"
                  type="number"
                  min="0"
                  step="1000"
                  value={limitTokens}
                  onChange={(e) => setLimitTokens(e.target.value)}
                  placeholder="optional"
                  data-testid="acct-limit-tokens"
                />
              </div>
            </div>
            <div className="form-row">
              <div className="form-field">
                <label className="form-label" htmlFor="acct-warn">
                  Warn at %
                </label>
                <input
                  id="acct-warn"
                  className="form-input"
                  type="number"
                  min="1"
                  max="100"
                  value={warnPct}
                  onChange={(e) => setWarnPct(Number(e.target.value))}
                  data-testid="acct-warn"
                />
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="acct-critical">
                  Critical at %
                </label>
                <input
                  id="acct-critical"
                  className="form-input"
                  type="number"
                  min="1"
                  max="100"
                  value={criticalPct}
                  onChange={(e) => setCriticalPct(Number(e.target.value))}
                  data-testid="acct-critical"
                />
              </div>
            </div>
          </>
        )}

        {error && <p className="form-error">{error}</p>}
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-primary" type="submit" disabled={loading} data-testid="acct-save">
            {loading ? 'Saving…' : editing ? 'Save' : 'Add Account'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
