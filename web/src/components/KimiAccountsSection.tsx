import { useEffect, useState } from 'react'
import { useKimiAccountsStore } from '../store/kimiAccounts'
import type { KimiAccount, WarnLevel } from '../types/api'
import ConfirmDialog from './ConfirmDialog'
import KimiAccountModal from './KimiAccountModal'
import KimiSignInModal from './KimiSignInModal'

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
  onSignIn,
  onDelete,
}: {
  account: KimiAccount
  onEdit: () => void
  onSignIn: () => void
  onDelete: () => void
}) {
  const { usage } = account
  const meta = LEVEL_META[usage.level]
  const pct = usage.used_fraction != null ? Math.round(usage.used_fraction * 100) : null
  const needsSignIn = account.kind === 'device' && !account.authenticated

  return (
    <div className="acct-row" data-testid={`kimi-acct-row-${account.id}`}>
      <div className="acct-row-main">
        <div className="acct-row-title">
          <span className="acct-name">{account.name}</span>
          <span className="acct-kind-tag">
            {account.kind === 'api_key' ? 'API key' : 'Sign in'}
          </span>
          {needsSignIn ? (
            <span
              className="acct-badge acct-badge-warning"
              data-testid={`kimi-acct-unauth-${account.id}`}
            >
              Not signed in
            </span>
          ) : (
            meta && (
              <span
                className={`acct-badge ${meta.cls}`}
                data-testid={`kimi-acct-badge-${account.id}`}
                data-level={usage.level}
              >
                {meta.label}
                {pct != null && ` · ${pct}%`}
              </span>
            )
          )}
        </div>
        <div className="acct-row-sub">
          <span className="acct-spend">
            {fmtTokens(usage.total_tokens)} tok · ${usage.est_cost_usd.toFixed(2)}
            {account.budget_window_hours ? ` in last ${account.budget_window_hours}h` : ' all-time'}
          </span>
        </div>
      </div>
      <div className="acct-row-actions">
        {account.kind === 'device' && (
          <button
            type="button"
            className="btn-secondary btn-sm"
            onClick={onSignIn}
            data-testid={`kimi-acct-signin-${account.id}`}
          >
            {account.authenticated ? 'Re-sign in' : 'Sign in'}
          </button>
        )}
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={onEdit}
          data-testid={`kimi-acct-edit-${account.id}`}
        >
          Edit
        </button>
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={onDelete}
          data-testid={`kimi-acct-delete-${account.id}`}
        >
          Delete
        </button>
      </div>
    </div>
  )
}

/**
 * Settings section that manages the logged-in Kimi accounts. Adding a device
 * account creates it, then opens the browser sign-in (`kimi login`); each
 * account shows up in every model picker as `[Name] Model`, so switching
 * accounts is just picking that model on a session. Mirrors the Grok
 * accounts section.
 */
export default function KimiAccountsSection() {
  const accounts = useKimiAccountsStore((s) => s.accounts)
  const loaded = useKimiAccountsStore((s) => s.loaded)
  const error = useKimiAccountsStore((s) => s.error)
  const fetchAccounts = useKimiAccountsStore((s) => s.fetchAccounts)
  const deleteAccount = useKimiAccountsStore((s) => s.deleteAccount)

  const [modal, setModal] = useState<{ account: KimiAccount | null } | null>(null)
  const [signIn, setSignIn] = useState<KimiAccount | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<KimiAccount | null>(null)

  useEffect(() => {
    void fetchAccounts()
  }, [fetchAccounts])

  return (
    <section className="settings-section" data-testid="kimi-accounts-section">
      <div className="settings-section-head">
        <h3>Kimi Accounts</h3>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => setModal({ account: null })}
          data-testid="kimi-acct-add"
        >
          + Add account
        </button>
      </div>
      <p className="form-hint">
        Each account appears in the model picker as <code>[Name] Model</code>. Pick that model on a
        session to run it under that account. The host&apos;s own <code>kimi</code> login is the
        implicit &ldquo;Default&rdquo; account.
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
              onSignIn={() => setSignIn(a)}
              onDelete={() => setConfirmDelete(a)}
            />
          ))}
        </div>
      )}

      {modal && (
        <KimiAccountModal
          account={modal.account}
          onClose={() => setModal(null)}
          onSaved={(toSignIn) => {
            if (toSignIn) setSignIn(toSignIn)
          }}
        />
      )}
      {signIn && <KimiSignInModal account={signIn} onClose={() => setSignIn(null)} />}
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
