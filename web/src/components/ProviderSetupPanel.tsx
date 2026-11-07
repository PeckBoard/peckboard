import { useEffect, useRef, useState } from 'react'
import { useCodexAccountsStore } from '../store/codexAccounts'
import { useGrokAccountsStore } from '../store/grokAccounts'
import { useKimiAccountsStore } from '../store/kimiAccounts'
import {
  isProviderReady,
  useProviderSetupStore,
  type ProviderSetupStatus,
} from '../store/providerSetup'
import type { CodexAccount, GrokAccount, KimiAccount } from '../types/api'
import ClaudeAccountModal from './ClaudeAccountModal'
import CodexSignInModal from './CodexSignInModal'
import CursorSignInModal from './CursorSignInModal'
import FieldError from './FieldError'
import GrokSignInModal from './GrokSignInModal'
import KimiSignInModal from './KimiSignInModal'

/** The newest lines of an installer's output are the useful ones. */
const LOG_TAIL_LINES = 6

const NEW_ACCOUNT_DEFAULTS = {
  budget_window_hours: null,
  budget_limit_usd: null,
  budget_limit_tokens: null,
  warn_threshold: 0.75,
  critical_threshold: 0.9,
}

type SignInDialog =
  | { kind: 'claude' }
  | { kind: 'cursor' }
  | { kind: 'codex'; account: CodexAccount }
  | { kind: 'grok'; account: GrokAccount }
  | { kind: 'kimi'; account: KimiAccount }

/** A device account waiting for sign-in, reused so a retry doesn't add another. */
async function deviceAccountFor(id: string): Promise<SignInDialog> {
  if (id === 'codex') {
    const store = useCodexAccountsStore.getState()
    await store.fetchAccounts()
    const existing = useCodexAccountsStore.getState().accounts.find((a) => !a.authenticated)
    const account =
      existing ??
      (await store.createAccount({ name: 'ChatGPT', kind: 'device', ...NEW_ACCOUNT_DEFAULTS }))
    return { kind: 'codex', account }
  }
  if (id === 'grok') {
    const store = useGrokAccountsStore.getState()
    await store.fetchAccounts()
    const existing = useGrokAccountsStore
      .getState()
      .accounts.find((a) => a.kind === 'device' && !a.authenticated)
    const account =
      existing ??
      (await store.createAccount({
        name: 'Grok',
        kind: 'device',
        credential: '',
        ...NEW_ACCOUNT_DEFAULTS,
      }))
    return { kind: 'grok', account }
  }
  const store = useKimiAccountsStore.getState()
  await store.fetchAccounts()
  const existing = useKimiAccountsStore
    .getState()
    .accounts.find((a) => a.kind === 'device' && !a.authenticated)
  const account =
    existing ??
    (await store.createAccount({
      name: 'Kimi',
      kind: 'device',
      credential: '',
      ...NEW_ACCOUNT_DEFAULTS,
    }))
  return { kind: 'kimi', account }
}

function statusText(p: ProviderSetupStatus): string {
  if (p.install_job?.running) return 'Installing…'
  if (!p.installed) return 'Not installed'
  const version = p.version ? ` · ${p.version}` : ''
  if (!p.signed_in) return `Installed, not signed in${version}`
  const who = p.account ? ` as ${p.account.name}` : ''
  if (!isProviderReady(p)) return `Signed in${who}, not turned on${version}`
  return `✓ Ready${who}${version}`
}

function ProviderRow({ provider: p }: { provider: ProviderSetupStatus }) {
  const install = useProviderSetupStore((s) => s.install)
  const enable = useProviderSetupStore((s) => s.enable)
  const refreshOne = useProviderSetupStore((s) => s.refreshOne)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [dialog, setDialog] = useState<SignInDialog | null>(null)

  const installing = p.install_job?.running ?? false
  const failed = p.install_job?.succeeded === false && !p.installed
  const ready = isProviderReady(p)
  const showLog = (installing || failed) && (p.install_job?.log.length ?? 0) > 0

  const logRef = useRef<HTMLPreElement>(null)
  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight
  }, [p.install_job?.log.length])

  const run = async (action: () => Promise<void>) => {
    setError('')
    setBusy(true)
    try {
      await action()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Something went wrong.')
    } finally {
      setBusy(false)
    }
  }

  const signIn = () =>
    run(async () => {
      // On first, so the models are there the moment the sign-in lands.
      if (!p.active || p.hidden) await enable(p.id)
      if (p.sign_in === 'claude') setDialog({ kind: 'claude' })
      else if (p.sign_in === 'host_cursor') setDialog({ kind: 'cursor' })
      else setDialog(await deviceAccountFor(p.id))
    })

  const closeDialog = () => {
    setDialog(null)
    void refreshOne(p.id)
  }

  return (
    <div className="provider-setup-row" data-testid={`provider-setup-${p.id}`}>
      <div className="provider-setup-summary">
        <div className="provider-setup-text">
          <span className="settings-label">{p.display_name}</span>
          <span
            className={`provider-setup-status ${ready ? 'ready' : ''}`}
            role="status"
            data-testid={`provider-setup-status-${p.id}`}
          >
            {statusText(p)}
          </span>
        </div>
        {!installing && !p.installed && p.auto_install && (
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void run(() => install(p.id))}
            disabled={busy}
            data-testid={`provider-setup-install-${p.id}`}
          >
            {failed ? 'Retry install' : 'Install'}
          </button>
        )}
        {!p.installed && !p.auto_install && (
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void refreshOne(p.id)}
            data-testid={`provider-setup-recheck-${p.id}`}
          >
            Check again
          </button>
        )}
        {p.installed && !installing && !p.signed_in && (
          <button
            type="button"
            className="btn-primary"
            onClick={() => void signIn()}
            disabled={busy}
            data-testid={`provider-setup-signin-${p.id}`}
          >
            Sign in
          </button>
        )}
        {p.installed && !installing && p.signed_in && !ready && (
          <button
            type="button"
            className="btn-primary"
            onClick={() => void run(() => enable(p.id))}
            disabled={busy}
            data-testid={`provider-setup-enable-${p.id}`}
          >
            Turn on
          </button>
        )}
      </div>

      {!p.installed && !installing && (
        <p className="form-hint provider-setup-command">
          <code>{p.install_command}</code>
        </p>
      )}
      {showLog && (
        <pre className="provider-setup-log" ref={logRef} data-testid={`provider-setup-log-${p.id}`}>
          {p.install_job?.log.slice(-LOG_TAIL_LINES).join('\n')}
        </pre>
      )}
      <FieldError
        message={(failed ? p.install_job?.error : null) || error || undefined}
        testId={`provider-setup-error-${p.id}`}
      />

      {dialog?.kind === 'claude' && (
        <ClaudeAccountModal account={null} quickSetup onClose={closeDialog} />
      )}
      {dialog?.kind === 'cursor' && <CursorSignInModal onClose={closeDialog} />}
      {dialog?.kind === 'codex' && (
        <CodexSignInModal account={dialog.account} onClose={closeDialog} />
      )}
      {dialog?.kind === 'grok' && (
        <GrokSignInModal account={dialog.account} onClose={closeDialog} />
      )}
      {dialog?.kind === 'kimi' && (
        <KimiSignInModal account={dialog.account} onClose={closeDialog} />
      )}
    </div>
  )
}

/**
 * Takes each CLI provider from nothing to usable: install the CLI, sign in on
 * the user's own subscription, turn the provider on. Shared by the setup
 * wizard and Settings → Providers. Neither step needs a working agent, which
 * matters because a new install doesn't have one yet.
 */
export default function ProviderSetupPanel() {
  const providers = useProviderSetupStore((s) => s.providers)
  const others = useProviderSetupStore((s) => s.others)
  const loadError = useProviderSetupStore((s) => s.error)
  const load = useProviderSetupStore((s) => s.load)
  const refreshOne = useProviderSetupStore((s) => s.refreshOne)
  const enable = useProviderSetupStore((s) => s.enable)
  const [enableError, setEnableError] = useState('')

  useEffect(() => {
    void load()
  }, [load])

  // An install keeps running on the server if the user steps away, so this
  // re-derives from the status rather than from having clicked Install.
  const runningIds = (providers ?? [])
    .filter((p) => p.install_job?.running)
    .map((p) => p.id)
    .join(',')
  useEffect(() => {
    if (!runningIds) return
    const timer = setInterval(() => {
      runningIds.split(',').forEach((id) => void refreshOne(id))
    }, 1500)
    return () => clearInterval(timer)
  }, [runningIds, refreshOne])

  if (!providers) {
    return loadError ? (
      <FieldError message={loadError} testId="provider-setup-load-error" />
    ) : (
      <p className="settings-loading">Checking your providers…</p>
    )
  }

  return (
    <div className="provider-setup" data-testid="provider-setup">
      {providers.some((p) => !p.installed) && (
        <p className="form-hint" data-testid="provider-setup-install-note">
          {providers[0]?.auto_install
            ? "Install runs the vendor's official installer below on this server, as the user PeckBoard runs as."
            : 'Run the command below on the server, then check again.'}
        </p>
      )}
      {providers.map((p) => (
        <ProviderRow key={p.id} provider={p} />
      ))}
      {others.length > 0 && (
        <details className="provider-setup-more">
          <summary data-testid="provider-setup-more">More providers</summary>
          <p className="form-hint">
            These connect to a model server you run, so there is nothing to install or sign in to.
            Set the server address in Settings → Providers.
          </p>
          {others.map((p) => (
            <div className="provider-setup-summary" key={p.id}>
              <span className="settings-label">{p.display_name}</span>
              {p.active && !p.hidden ? (
                <span className="provider-setup-status ready">✓ On</span>
              ) : (
                <button
                  type="button"
                  className="btn-secondary"
                  onClick={() => {
                    setEnableError('')
                    enable(p.id).catch((e: Error) => setEnableError(e.message))
                  }}
                  data-testid={`provider-setup-enable-${p.id}`}
                >
                  Turn on
                </button>
              )}
            </div>
          ))}
          <FieldError message={enableError || undefined} testId="provider-setup-enable-error" />
        </details>
      )}
    </div>
  )
}
