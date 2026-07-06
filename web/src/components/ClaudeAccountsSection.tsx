import { useEffect, useState } from 'react'
import { useClaudeAccountsStore } from '../store/claudeAccounts'
import type { ClaudeAccount, PlanUsageEntry, WarnLevel } from '../types/api'
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

/** "14:05" when the reset is within 24h, "Mon 14:05" otherwise. */
function fmtReset(iso: string | null): string | null {
  if (!iso) return null
  const t = new Date(iso)
  if (Number.isNaN(t.getTime())) return null
  const time = t.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
  if (t.getTime() - Date.now() < 24 * 3_600_000) return time
  return `${t.toLocaleDateString([], { weekday: 'short' })} ${time}`
}

/** One "Session 34% · resets 14:05" fragment, or null for an absent bucket. */
function bucketText(
  label: string,
  bucket: { utilization: number; resets_at: string | null } | null,
): string | null {
  if (!bucket) return null
  const reset = fmtReset(bucket.resets_at)
  return `${label} ${Math.round(bucket.utilization)}%${reset ? ` · resets ${reset}` : ''}`
}

/** The `/usage` buckets of one login as a single compact line. */
function planLine(entry: PlanUsageEntry): string | null {
  const u = entry.usage
  if (!u) return null
  const parts = [
    bucketText('Session', u.five_hour),
    bucketText('Week', u.seven_day),
    bucketText('Opus', u.seven_day_opus),
    bucketText('Sonnet', u.seven_day_sonnet),
  ].filter((p): p is string => p != null)
  return parts.length > 0 ? parts.join('  ·  ') : null
}

/**
 * Subscription plan usage for the host's own ("Default") login — the same
 * numbers `claude /usage` shows, polled by the server every 30 minutes.
 */
function PlanUsagePanel({
  entry,
  refreshing,
  onRefresh,
}: {
  entry: PlanUsageEntry | undefined
  refreshing: boolean
  onRefresh: () => void
}) {
  const line = entry ? planLine(entry) : null
  return (
    <div className="acct-plan-panel" data-testid="claude-plan-usage">
      <div className="acct-plan-head">
        <span className="acct-plan-title">Plan usage — Default (host login)</span>
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={onRefresh}
          disabled={refreshing}
          data-testid="claude-plan-refresh"
        >
          {refreshing ? 'Refreshing…' : 'Refresh'}
        </button>
      </div>
      {line ? (
        <div className="acct-plan-line">{line}</div>
      ) : (
        <div className="acct-plan-line acct-plan-empty">
          {entry?.last_error ? 'Plan usage unavailable.' : 'Waiting for first poll…'}
        </div>
      )}
      {entry?.last_error && <div className="acct-plan-error">{entry.last_error}</div>}
      {entry?.fetched_at != null && (
        <div className="acct-plan-updated">
          Updated {new Date(entry.fetched_at).toLocaleTimeString()} · refreshes every 30 min
        </div>
      )}
    </div>
  )
}

function AccountRow({
  account,
  plan,
  onEdit,
  onDelete,
}: {
  account: ClaudeAccount
  plan: PlanUsageEntry | undefined
  onEdit: () => void
  onDelete: () => void
}) {
  const { usage } = account
  const meta = LEVEL_META[usage.level]
  const pct = usage.used_fraction != null ? Math.round(usage.used_fraction * 100) : null
  const accountPlanLine = plan ? planLine(plan) : null

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
        {accountPlanLine ? (
          <div className="acct-row-sub acct-plan-row" data-testid={`acct-plan-${account.id}`}>
            <span className="acct-plan-label">Plan</span>
            <span>{accountPlanLine}</span>
          </div>
        ) : plan?.last_error && account.kind === 'oauth_token' ? (
          <div
            className="acct-row-sub acct-plan-row acct-plan-error"
            data-testid={`acct-plan-error-${account.id}`}
          >
            <span className="acct-plan-label">Plan</span>
            <span>{plan.last_error}</span>
          </div>
        ) : null}
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
 * its budget warn level are shown inline, and the real subscription plan
 * usage (the `claude /usage` buckets, server-polled every 30 minutes) is
 * shown for the Default login and any subscription account whose token can
 * read it.
 */
export default function ClaudeAccountsSection() {
  const accounts = useClaudeAccountsStore((s) => s.accounts)
  const loaded = useClaudeAccountsStore((s) => s.loaded)
  const error = useClaudeAccountsStore((s) => s.error)
  const planUsage = useClaudeAccountsStore((s) => s.planUsage)
  const planUsageRefreshing = useClaudeAccountsStore((s) => s.planUsageRefreshing)
  const fetchAccounts = useClaudeAccountsStore((s) => s.fetchAccounts)
  const fetchPlanUsage = useClaudeAccountsStore((s) => s.fetchPlanUsage)
  const refreshPlanUsage = useClaudeAccountsStore((s) => s.refreshPlanUsage)
  const deleteAccount = useClaudeAccountsStore((s) => s.deleteAccount)

  const [modal, setModal] = useState<{ account: ClaudeAccount | null } | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<ClaudeAccount | null>(null)

  useEffect(() => {
    void fetchAccounts()
    void fetchPlanUsage()
  }, [fetchAccounts, fetchPlanUsage])

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

      <PlanUsagePanel
        entry={planUsage['default']}
        refreshing={planUsageRefreshing}
        onRefresh={() => void refreshPlanUsage()}
      />

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
              plan={planUsage[a.id]}
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
