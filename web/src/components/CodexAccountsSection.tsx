import { useEffect, useState } from 'react'
import { useAuthStore } from '../store/auth'
import { useCodexAccountsStore } from '../store/codexAccounts'
import {
  AccountDeleteConflict,
  describeAccountRefs,
  type AccountDeleteRefs,
} from '../store/accountDeleteGuard'
import type { CodexAccount, WarnLevel } from '../types/api'
import ConfirmDialog from './ConfirmDialog'
import CodexAccountModal from './CodexAccountModal'
import CodexSignInModal from './CodexSignInModal'

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
  account: CodexAccount
  onEdit: () => void
  onSignIn: () => void
  onDelete: () => void
}) {
  const { usage } = account
  const meta = LEVEL_META[usage.level]
  const pct = usage.used_fraction != null ? Math.round(usage.used_fraction * 100) : null
  const needsSignIn = !account.authenticated

  return (
    <div className="acct-row" data-testid={`codex-acct-row-${account.id}`}>
      <div className="acct-row-main">
        <div className="acct-row-title">
          <span className="acct-name">{account.name}</span>
          <span className="acct-kind-tag">ChatGPT</span>
          {needsSignIn ? (
            <span
              className="acct-badge acct-badge-warning"
              data-testid={`codex-acct-unauth-${account.id}`}
            >
              Not signed in
            </span>
          ) : (
            meta && (
              <span
                className={`acct-badge ${meta.cls}`}
                data-testid={`codex-acct-badge-${account.id}`}
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
      {useAuthStore((s) => s.user?.role === 'admin') && (
        <div className="acct-row-actions">
          <button
            type="button"
            className="btn-secondary btn-sm"
            onClick={onSignIn}
            data-testid={`codex-acct-signin-${account.id}`}
          >
            {account.authenticated ? 'Re-sign in' : 'Sign in'}
          </button>
          <button
            type="button"
            className="btn-secondary btn-sm"
            onClick={onEdit}
            data-testid={`codex-acct-edit-${account.id}`}
          >
            Edit
          </button>
          <button
            type="button"
            className="btn-secondary btn-sm"
            onClick={onDelete}
            data-testid={`codex-acct-delete-${account.id}`}
          >
            Delete
          </button>
        </div>
      )}
    </div>
  )
}

/**
 * Settings section that manages Codex ChatGPT logins. Adding an account
 * creates it, then opens ChatGPT device sign-in (`codex login --device-auth`);
 * each account shows up in every model picker as `[Name] Model`.
 */
export default function CodexAccountsSection() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const accounts = useCodexAccountsStore((s) => s.accounts)
  const loaded = useCodexAccountsStore((s) => s.loaded)
  const error = useCodexAccountsStore((s) => s.error)
  const fetchAccounts = useCodexAccountsStore((s) => s.fetchAccounts)
  const deleteAccount = useCodexAccountsStore((s) => s.deleteAccount)
  const setError = useCodexAccountsStore((s) => s.setError)
  const [modal, setModal] = useState<{ account: CodexAccount | null } | null>(null)
  const [signIn, setSignIn] = useState<CodexAccount | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<CodexAccount | null>(null)
  const [forceDelete, setForceDelete] = useState<{
    account: CodexAccount
    refs: AccountDeleteRefs
  } | null>(null)

  useEffect(() => {
    void fetchAccounts()
  }, [fetchAccounts])

  return (
    <section className="settings-section" data-testid="codex-accounts-section">
      <div className="settings-section-head">
        <h3>Codex Accounts</h3>
        {isAdmin && (
          <button
            type="button"
            className="btn-primary btn-sm"
            onClick={() => setModal({ account: null })}
            data-testid="codex-acct-add"
          >
            + Add account
          </button>
        )}
      </div>
      <p className="form-hint">
        Sign in with ChatGPT (not an API key). Each account appears in the model picker as{' '}
        <code>[Name] Model</code>. The host&apos;s own <code>codex login</code> is the implicit
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
              onSignIn={() => setSignIn(a)}
              onDelete={() => setConfirmDelete(a)}
            />
          ))}
        </div>
      )}

      {modal && (
        <CodexAccountModal
          account={modal.account}
          onClose={() => setModal(null)}
          onSaved={(toSignIn) => {
            if (toSignIn) setSignIn(toSignIn)
          }}
        />
      )}
      {signIn && <CodexSignInModal account={signIn} onClose={() => setSignIn(null)} />}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete account"
          message={`Remove "${confirmDelete.name}"? Sessions pinned to it will fall back to the Default login on their next turn. Recorded usage is kept.`}
          confirmLabel="Delete"
          danger
          onConfirm={() => {
            const target = confirmDelete
            setConfirmDelete(null)
            setError(null)
            void deleteAccount(target.id).catch((e: unknown) => {
              if (e instanceof AccountDeleteConflict) {
                setForceDelete({ account: target, refs: e.refs })
              } else if (e instanceof Error) {
                setError(e.message)
              }
            })
          }}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      {forceDelete && (
        <ConfirmDialog
          title="Account still in use"
          message={describeAccountRefs(forceDelete.account.name, forceDelete.refs)}
          confirmLabel="Delete anyway"
          danger
          onConfirm={() => {
            const target = forceDelete.account
            setForceDelete(null)
            setError(null)
            void deleteAccount(target.id, true).catch((e: Error) => setError(e.message))
          }}
          onCancel={() => setForceDelete(null)}
        />
      )}
    </section>
  )
}
