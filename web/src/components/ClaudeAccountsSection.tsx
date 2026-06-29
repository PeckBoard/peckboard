import { useEffect, useState } from 'react'
import { useClaudeAccountsStore } from '../store/claudeAccounts'
import type { ClaudeAccount, WarnLevel } from '../types/api'
import ClaudeAccountModal from './ClaudeAccountModal'
import ConfirmDialog from './ConfirmDialog'

/** Human label + badge class for each warn level. `none`/`ok` render quietly. */
const LEVEL_META: Record<WarnLevel, { label: string; cls: string } | null> = {
  none: null,
  ok: { label: 'OK', cls: 'acct-badge-ok' },
  warning: { label: 'Near limit', cls: 'acct-badge-warning' },
  critical: { label: 'Critical', cls: 'acct-badge-critical' },
  exceeded: { label: 'Over budget', cls: 'acct-badge-exceeded' },
}

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return String(n)
}

function AccountRow({
  account,
  onEdit,
  onDelete,
}: {
  account: ClaudeAccount
  onEdit: () => void
  onDelete: () => void
}) {
  const { usage } = account
  const meta = LEVEL_META[usage.level]
  const pct = usage.used_fraction != null ? Math.round(usage.used_fraction * 100) : null

  return (
    <div className="acct-row" data-testid={`acct-row-${account.id}`}>
      <div className="acct-row-main">
        <div className="acct-row-title">
          <span className="acct-name">{account.name}</span>
          <span className="acct-kind-tag">
            {account.kind === 'api_key' ? 'API key' : 'Subscription'}
          </span>
          {meta && (
            <span
              className={`acct-badge ${meta.cls}`}
              data-testid={`acct-badge-${account.id}`}
              data-level={usage.level}
            >
              {meta.label}
              {pct != null && ` · ${pct}%`}
            </span>
          )}
        </div>
        <div className="acct-row-sub">
          <span className="acct-hint">{account.credential_hint}</span>
          <span className="acct-spend">
            {fmtTokens(usage.total_tokens)} tok · ${usage.est_cost_usd.toFixed(2)}
            {account.budget_window_hours ? ` in last ${account.budget_window_hours}h` : ' all-time'}
          </span>
        </div>
      </div>
      <div className="acct-row-actions">
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={onEdit}
          data-testid={`acct-edit-${account.id}`}
        >
          Edit
        </button>
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={onDelete}
          data-testid={`acct-delete-${account.id}`}
        >
          Delete
        </button>
      </div>
    </div>
  )
}

/**
 * Settings section that manages the logged-in Claude accounts. Adding one is
 * the "login" flow (paste a subscription token or API key); each account then
 * shows up in every model picker as `[Name] Model`, so switching accounts is
 * just picking that model on a session. Per-account rolling-window spend and
 * its budget warn level are shown inline.
 */
export default function ClaudeAccountsSection() {
  const accounts = useClaudeAccountsStore((s) => s.accounts)
  const loaded = useClaudeAccountsStore((s) => s.loaded)
  const error = useClaudeAccountsStore((s) => s.error)
  const fetchAccounts = useClaudeAccountsStore((s) => s.fetchAccounts)
  const deleteAccount = useClaudeAccountsStore((s) => s.deleteAccount)

  const [modal, setModal] = useState<{ account: ClaudeAccount | null } | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<ClaudeAccount | null>(null)

  useEffect(() => {
    void fetchAccounts()
  }, [fetchAccounts])

  return (
    <section className="settings-section" data-testid="claude-accounts-section">
      <div className="settings-section-head">
        <h3>Claude Accounts</h3>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => setModal({ account: null })}
          data-testid="acct-add"
        >
          + Add account
        </button>
      </div>
      <p className="form-hint">
        Each account appears in the model picker as <code>[Name] Model</code>. Pick that model on a
        session to run it under that account. The host&apos;s own login is the implicit
        &ldquo;Default&rdquo; account.
      </p>

      {error && <p className="form-error">{error}</p>}

      {loaded && accounts.length === 0 ? (
        <p className="settings-loading">
          No accounts added yet — only the Default (host) login is in use.
        </p>
      ) : (
        <div className="acct-list">
          {accounts.map((a) => (
            <AccountRow
              key={a.id}
              account={a}
              onEdit={() => setModal({ account: a })}
              onDelete={() => setConfirmDelete(a)}
            />
          ))}
        </div>
      )}

      {modal && <ClaudeAccountModal account={modal.account} onClose={() => setModal(null)} />}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete account"
          message={`Remove "${confirmDelete.name}"? Sessions pinned to it will fall back to the Default login on their next turn. Recorded usage is kept.`}
          confirmLabel="Delete"
          danger
          onConfirm={() => {
            void deleteAccount(confirmDelete.id)
            setConfirmDelete(null)
          }}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
    </section>
  )
}
