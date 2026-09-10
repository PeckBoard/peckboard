import { useState, type FormEvent } from 'react'
import { useCodexAccountsStore } from '../store/codexAccounts'
import type { CodexAccount } from '../types/api'
import Modal from './Modal'

interface Props {
  /** Editing an existing account, or `null` to add a new one. */
  account: CodexAccount | null
  /** Called after a successful save. For a brand-new account the created
   *  account is passed back so the caller can immediately launch ChatGPT
   *  sign-in; edits pass `null`. */
  onSaved: (signIn: CodexAccount | null) => void
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

/**
 * Add or edit a Codex account. ChatGPT sign-in is the only credential type:
 * the account is created here first, then signed in via the row's "Sign in"
 * button / {@link CodexSignInModal}. This modal collects the name and
 * optional budget.
 */
export default function CodexAccountModal({ account, onSaved, onClose }: Props) {
  const createAccount = useCodexAccountsStore((s) => s.createAccount)
  const updateAccount = useCodexAccountsStore((s) => s.updateAccount)
  const editing = account !== null

  const [name, setName] = useState(account?.name ?? '')
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
        kind: 'device' as const,
        budget_window_hours: hasBudget ? windowHours : null,
        budget_limit_usd: hasBudget ? parseNum(limitUsd) : null,
        budget_limit_tokens: hasBudget ? parseNum(limitTokens) : null,
        warn_threshold: warnPct / 100,
        critical_threshold: criticalPct / 100,
      }
      if (editing) {
        await updateAccount(account.id, input)
        onSaved(null)
      } else {
        const created = await createAccount(input)
        onSaved(created)
      }
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save account')
    } finally {
      setLoading(false)
    }
  }

  return (
    <Modal onClose={onClose} data-testid="codex-account-modal">
      <h2>{editing ? `Edit ${account.name}` : 'Add Codex Account'}</h2>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="codex-acct-name">
            Account name
          </label>
          <input
            id="codex-acct-name"
            className="form-input"
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. Work, Personal"
            autoFocus
            required
            data-testid="codex-acct-name"
          />
        </div>

        {!editing && (
          <p className="form-hint" data-testid="codex-acct-device-hint">
            You&apos;ll sign in with ChatGPT in the browser right after adding the account.
          </p>
        )}

        <div className="form-field">
          <label className="form-label" htmlFor="codex-acct-window">
            Usage budget
          </label>
          <select
            id="codex-acct-window"
            className="form-input"
            value={windowHours === null ? '' : String(windowHours)}
            onChange={(e) => setWindowHours(e.target.value === '' ? null : Number(e.target.value))}
            data-testid="codex-acct-window"
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
                <label className="form-label" htmlFor="codex-acct-limit-usd">
                  Cost cap (USD)
                </label>
                <input
                  id="codex-acct-limit-usd"
                  className="form-input"
                  type="number"
                  min="0"
                  step="0.01"
                  value={limitUsd}
                  onChange={(e) => setLimitUsd(e.target.value)}
                  placeholder="optional"
                  data-testid="codex-acct-limit-usd"
                />
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="codex-acct-limit-tokens">
                  Token cap
                </label>
                <input
                  id="codex-acct-limit-tokens"
                  className="form-input"
                  type="number"
                  min="0"
                  step="1000"
                  value={limitTokens}
                  onChange={(e) => setLimitTokens(e.target.value)}
                  placeholder="optional"
                  data-testid="codex-acct-limit-tokens"
                />
              </div>
            </div>
            <div className="form-row">
              <div className="form-field">
                <label className="form-label" htmlFor="codex-acct-warn">
                  Warn at %
                </label>
                <input
                  id="codex-acct-warn"
                  className="form-input"
                  type="number"
                  min="1"
                  max="100"
                  value={warnPct}
                  onChange={(e) => setWarnPct(Number(e.target.value))}
                  data-testid="codex-acct-warn"
                />
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="codex-acct-critical">
                  Critical at %
                </label>
                <input
                  id="codex-acct-critical"
                  className="form-input"
                  type="number"
                  min="1"
                  max="100"
                  value={criticalPct}
                  onChange={(e) => setCriticalPct(Number(e.target.value))}
                  data-testid="codex-acct-critical"
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
          <button
            className="btn-primary"
            type="submit"
            disabled={loading}
            data-testid="codex-acct-save"
          >
            {loading ? 'Saving…' : editing ? 'Save' : 'Add Account'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
