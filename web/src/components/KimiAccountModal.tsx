import { useState, type FormEvent } from 'react'
import { useKimiAccountsStore } from '../store/kimiAccounts'
import type { KimiAccount, KimiAccountKind } from '../types/api'
import Modal from './Modal'

interface Props {
  /** Editing an existing account, or `null` to add a new one. */
  account: KimiAccount | null
  /** Called after a successful save. For a brand-new `device` account the
   *  created account is passed back so the caller can immediately launch the
   *  browser sign-in; edits and api_key accounts pass `null`. */
  onSaved: (signIn: KimiAccount | null) => void
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

const KIND_OPTIONS: { value: KimiAccountKind; label: string; hint: string }[] = [
  {
    value: 'device',
    label: 'Sign in',
    hint: 'Sign in with your Kimi account in the browser (device code).',
  },
  {
    value: 'api_key',
    label: 'API key',
    hint: 'Paste a Moonshot AI API key (sk-…). Billed via the API.',
  },
]

/**
 * Add or edit a Kimi account. Like Grok (and unlike Claude's paste-back token
 * flow), a Kimi `device` account is created here first and then signed in via
 * the browser (the row's "Sign in" button / {@link KimiSignInModal}) — so this
 * modal only collects the name, kind, optional API key, and budget. On
 * creating a new device account it hands the account back so the caller can
 * launch sign-in straight away.
 */
export default function KimiAccountModal({ account, onSaved, onClose }: Props) {
  const createAccount = useKimiAccountsStore((s) => s.createAccount)
  const updateAccount = useKimiAccountsStore((s) => s.updateAccount)
  const editing = account !== null

  const [name, setName] = useState(account?.name ?? '')
  const [kind, setKind] = useState<KimiAccountKind>(account?.kind ?? 'device')
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
  // The credential type is fixed once an account exists (it determines how it
  // authenticates); only new accounts can choose.
  const kindLocked = editing

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    if (!name.trim()) {
      setError('Name is required')
      return
    }
    if (kind === 'api_key' && !editing && !credential.trim()) {
      setError('Paste a Moonshot AI API key')
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
        // A fresh device account needs to sign in next; an api_key account is
        // ready as soon as it's created.
        onSaved(created.kind === 'device' ? created : null)
      }
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save account')
    } finally {
      setLoading(false)
    }
  }

  return (
    <Modal onClose={onClose} data-testid="kimi-account-modal">
      <h2>{editing ? `Edit ${account.name}` : 'Add Kimi Account'}</h2>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="kimi-acct-name">
            Account name
          </label>
          <input
            id="kimi-acct-name"
            className="form-input"
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. Work, Personal"
            autoFocus
            required
            data-testid="kimi-acct-name"
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
                onClick={() => !kindLocked && setKind(k.value)}
                disabled={kindLocked && kind !== k.value}
                data-testid={`kimi-acct-kind-${k.value}`}
              >
                {k.label}
              </button>
            ))}
          </div>
          <span className="form-hint">
            {kindLocked
              ? 'Credential type is fixed after creation — add another account to use the other type.'
              : KIND_OPTIONS.find((k) => k.value === kind)?.hint}
          </span>
        </div>

        {kind === 'api_key' ? (
          <div className="form-field">
            <label className="form-label" htmlFor="kimi-acct-credential">
              API key
              {editing && ' (leave blank to keep current)'}
            </label>
            <input
              id="kimi-acct-credential"
              className="form-input"
              type="password"
              value={credential}
              onChange={(e) => setCredential(e.target.value)}
              placeholder={editing ? 'Keeping current key' : 'sk-…'}
              autoComplete="off"
              data-testid="kimi-acct-credential"
            />
          </div>
        ) : (
          !editing && (
            <p className="form-hint" data-testid="kimi-acct-device-hint">
              You&apos;ll sign in with Kimi in the browser right after adding the account.
            </p>
          )
        )}

        <div className="form-field">
          <label className="form-label" htmlFor="kimi-acct-window">
            Usage budget
          </label>
          <select
            id="kimi-acct-window"
            className="form-input"
            value={windowHours === null ? '' : String(windowHours)}
            onChange={(e) => setWindowHours(e.target.value === '' ? null : Number(e.target.value))}
            data-testid="kimi-acct-window"
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
                <label className="form-label" htmlFor="kimi-acct-limit-usd">
                  Cost cap (USD)
                </label>
                <input
                  id="kimi-acct-limit-usd"
                  className="form-input"
                  type="number"
                  min="0"
                  step="0.01"
                  value={limitUsd}
                  onChange={(e) => setLimitUsd(e.target.value)}
                  placeholder="optional"
                  data-testid="kimi-acct-limit-usd"
                />
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="kimi-acct-limit-tokens">
                  Token cap
                </label>
                <input
                  id="kimi-acct-limit-tokens"
                  className="form-input"
                  type="number"
                  min="0"
                  step="1000"
                  value={limitTokens}
                  onChange={(e) => setLimitTokens(e.target.value)}
                  placeholder="optional"
                  data-testid="kimi-acct-limit-tokens"
                />
              </div>
            </div>
            <div className="form-row">
              <div className="form-field">
                <label className="form-label" htmlFor="kimi-acct-warn">
                  Warn at %
                </label>
                <input
                  id="kimi-acct-warn"
                  className="form-input"
                  type="number"
                  min="1"
                  max="100"
                  value={warnPct}
                  onChange={(e) => setWarnPct(Number(e.target.value))}
                  data-testid="kimi-acct-warn"
                />
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="kimi-acct-critical">
                  Critical at %
                </label>
                <input
                  id="kimi-acct-critical"
                  className="form-input"
                  type="number"
                  min="1"
                  max="100"
                  value={criticalPct}
                  onChange={(e) => setCriticalPct(Number(e.target.value))}
                  data-testid="kimi-acct-critical"
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
            data-testid="kimi-acct-save"
          >
            {loading ? 'Saving…' : editing ? 'Save' : 'Add Account'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
