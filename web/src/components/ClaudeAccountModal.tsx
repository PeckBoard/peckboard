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
    label: 'Subscription token',
    hint: 'Run `claude setup-token` in a terminal and paste the token (Pro/Max).',
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
  const editing = account !== null

  const [name, setName] = useState(account?.name ?? '')
  const [kind, setKind] = useState<ClaudeAccountKind>(account?.kind ?? 'oauth_token')
  const [credential, setCredential] = useState('')
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

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    if (!name.trim()) {
      setError('Name is required')
      return
    }
    if (!editing && !credential.trim()) {
      setError('Paste a token or API key')
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
        credential: credential.trim(),
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
                onClick={() => setKind(k.value)}
                data-testid={`acct-kind-${k.value}`}
              >
                {k.label}
              </button>
            ))}
          </div>
          <span className="form-hint">{KIND_OPTIONS.find((k) => k.value === kind)?.hint}</span>
        </div>

        <div className="form-field">
          <label className="form-label" htmlFor="acct-credential">
            {kind === 'api_key' ? 'API key' : 'Token'}
            {editing && ' (leave blank to keep current)'}
          </label>
          <input
            id="acct-credential"
            className="form-input"
            type="password"
            value={credential}
            onChange={(e) => setCredential(e.target.value)}
            placeholder={
              editing
                ? `Keeping ${account.credential_hint}`
                : kind === 'api_key'
                  ? 'sk-ant-…'
                  : 'Paste setup-token'
            }
            autoComplete="off"
            data-testid="acct-credential"
          />
        </div>

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
