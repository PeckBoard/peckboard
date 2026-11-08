import { useEffect, useRef, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useCodexAccountsStore } from '../store/codexAccounts'
import { useGrokAccountsStore } from '../store/grokAccounts'
import { useKimiAccountsStore } from '../store/kimiAccounts'
import {
  isProviderReady,
  pickDefaultModel,
  useProviderSetupStore,
  type ProviderSetupStatus,
} from '../store/providerSetup'
import { useResourcesStore } from '../store/resources'
import type { CodexAccount, GrokAccount, KimiAccount } from '../types/api'
import ClaudeAccountModal from './ClaudeAccountModal'
import CodexSignInModal from './CodexSignInModal'
import CursorSignInModal from './CursorSignInModal'
import HelpTip from './HelpTip'
import ModelPicker from './ModelPicker'
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

/** Which paid plan each tool signs in with, so a new user can tell which
 *  one they already pay for. Keyed by provider id. */
const PLAN_FOR: Record<string, string> = {
  claude: 'Claude Pro or Max',
  codex: 'ChatGPT Plus or Pro',
  cursor: 'a Cursor plan',
  grok: 'SuperGrok or X Premium+',
  kimi: 'a Kimi plan',
}

/** Settings only: the per-provider "Hide from pickers" switch. A provider
 *  gets one only when it appears in `hidden` (i.e. it is turned on). */
export interface ProviderVisibilityControl {
  hidden: Record<string, boolean>
  onToggle: (id: string, hidden: boolean) => void
}

function HideToggle({
  id,
  name,
  visibility,
}: {
  id: string
  name: string
  visibility?: ProviderVisibilityControl
}) {
  if (!visibility || !(id in visibility.hidden)) return null
  return (
    <label className="provider-setup-hide">
      <input
        type="checkbox"
        checked={visibility.hidden[id]}
        aria-label={`Hide ${name} from pickers`}
        data-testid={`provider-toggle-${id}`}
        onChange={(e) => visibility.onToggle(id, e.target.checked)}
      />
      <span>Hide from pickers</span>
    </label>
  )
}

function ProviderRow({
  provider: p,
  visibility,
}: {
  provider: ProviderSetupStatus
  visibility?: ProviderVisibilityControl
}) {
  const install = useProviderSetupStore((s) => s.install)
  const enable = useProviderSetupStore((s) => s.enable)
  const refreshOne = useProviderSetupStore((s) => s.refreshOne)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [dialog, setDialog] = useState<SignInDialog | null>(null)
  const [showDetails, setShowDetails] = useState(false)

  const installing = p.install_job?.running ?? false
  const failed = p.install_job?.succeeded === false && !p.installed
  const ready = isProviderReady(p)
  const showLog = (installing || failed) && (p.install_job?.log.length ?? 0) > 0
  const plan = PLAN_FOR[p.id]
  const needsCommand = !p.installed && !installing

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
          <span className="provider-setup-name">
            <span className="settings-label">{p.display_name}</span>
            {plan && (
              <span className="provider-setup-plan" data-testid={`provider-setup-plan-${p.id}`}>
                Uses {plan}
              </span>
            )}
          </span>
          <span
            className={`provider-setup-status ${ready ? 'ready' : ''}`}
            role="status"
            data-testid={`provider-setup-status-${p.id}`}
          >
            {statusText(p)}
          </span>
          <HideToggle id={p.id} name={p.display_name} visibility={visibility} />
        </div>
        <div className="provider-setup-buttons">
          {needsCommand && p.auto_install && (
            <button
              type="button"
              className="btn-link provider-setup-details-toggle"
              aria-expanded={showDetails}
              onClick={() => setShowDetails((v) => !v)}
              data-testid={`provider-setup-details-${p.id}`}
            >
              Details
            </button>
          )}
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
      </div>

      {/* Install runs the command for the user, so it hides behind Details;
          where the user has to run it themselves it stays in view. */}
      {needsCommand && (!p.auto_install || showDetails) && (
        <p
          className="form-hint provider-setup-command"
          data-testid={`provider-setup-command-${p.id}`}
        >
          {p.auto_install &&
            'Install runs this official command on the computer PeckBoard runs on: '}
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

/** Save the app-wide default model, keeping the local copy in step. Rejects
 *  (after restoring the previous value) when the server refuses. */
function saveDefaultModel(model: string): Promise<void> {
  const { defaultModel: prev, setDefaultModelLocal } = useResourcesStore.getState()
  setDefaultModelLocal(model)
  return authedFetch('/api/settings/default-model', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ model }),
  })
    .then(
      (res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
      },
      (e: unknown) => {
        throw e instanceof Error ? e : new Error('network error')
      },
    )
    .catch((e: Error) => {
      setDefaultModelLocal(prev)
      throw e
    })
}

/**
 * "Default AI: <model> · change" under the wizard's provider list. While no
 * default is set it picks one from the first provider that becomes ready —
 * driven by store subscriptions (a sign-in finishing, models arriving), never
 * over a choice the user already made. Hidden while there are no models.
 */
function DefaultAiLine() {
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const fetchDefaultModel = useResourcesStore((s) => s.fetchDefaultModel)
  const defaultModel = useResourcesStore((s) => s.defaultModel)
  const [picking, setPicking] = useState(false)
  const [error, setError] = useState('')

  useEffect(() => {
    void fetchModels()
    void fetchDefaultModel()
  }, [fetchModels, fetchDefaultModel])

  useEffect(() => {
    // One automatic attempt per model id, so a refused save can't loop.
    const tried = new Set<string>()
    const tryPick = () => {
      const res = useResourcesStore.getState()
      const statuses = useProviderSetupStore.getState().providers
      if (!res.defaultModelLoaded || res.defaultModel || !statuses) return
      const pick = pickDefaultModel(
        statuses,
        res.models.map((m) => m.id),
      )
      if (!pick || tried.has(pick)) return
      tried.add(pick)
      saveDefaultModel(pick).catch((e: Error) =>
        setError(`Could not save the default AI: ${e.message}`),
      )
    }
    tryPick()
    const offSetup = useProviderSetupStore.subscribe(tryPick)
    const offResources = useResourcesStore.subscribe(tryPick)
    return () => {
      offSetup()
      offResources()
    }
  }, [])

  if (models.length === 0) return null

  const change = (model: string) => {
    setError('')
    saveDefaultModel(model).catch((e: Error) =>
      setError(`Could not save the default AI: ${e.message}`),
    )
  }
  const current = models.find((m) => m.id === defaultModel)

  return (
    <div className="provider-setup-default" data-testid="setup-default-ai">
      <span>
        Default AI:{' '}
        <strong data-testid="setup-default-ai-name">
          {current?.display_name ?? (defaultModel || 'none yet')}
        </strong>
        <HelpTip term="model" />
      </span>
      {' · '}
      <button
        type="button"
        className="btn-link"
        aria-expanded={picking}
        onClick={() => setPicking((v) => !v)}
        data-testid="setup-default-ai-change"
      >
        change
      </button>
      {picking && (
        <div className="provider-setup-default-picker">
          <ModelPicker
            value={defaultModel}
            onChange={change}
            models={models}
            ariaLabel="Default AI"
            testId="setup-default-model"
            emptyHint="No models yet — connect one of the tools above."
            onOpen={fetchModels}
          />
        </div>
      )}
      <FieldError message={error || undefined} testId="setup-model-error" />
    </div>
  )
}

interface PanelProps {
  /** Wizard: show the "Default AI" line (and auto-pick one) under the list. */
  showDefaultModel?: boolean
  /** Settings: give each turned-on provider a "Hide from pickers" switch. */
  visibility?: ProviderVisibilityControl
}

/**
 * Takes each CLI provider from nothing to usable: install the CLI, sign in on
 * the user's own subscription, turn the provider on. Shared by the setup
 * wizard and Settings → Providers. Neither step needs a working agent, which
 * matters because a new install doesn't have one yet.
 */
export default function ProviderSetupPanel({ showDefaultModel, visibility }: PanelProps) {
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
      <p className="form-hint provider-setup-advice" data-testid="provider-setup-advice">
        Have one of these plans? Start with that. Not sure? Claude or ChatGPT.
      </p>
      {providers.some((p) => !p.installed && !p.auto_install) && (
        <p className="form-hint" data-testid="provider-setup-install-note">
          Run the command shown under a tool on the computer PeckBoard runs on, then press Check
          again.
        </p>
      )}
      {providers.map((p) => (
        <ProviderRow key={p.id} provider={p} visibility={visibility} />
      ))}
      {others.length > 0 && (
        <details className="provider-setup-more" open={!!visibility}>
          <summary data-testid="provider-setup-more">Self-hosted models (advanced)</summary>
          <p className="form-hint">
            These connect to a model server you run, so there is nothing to install or sign in to.
            Set the server address in Settings → Providers.
          </p>
          {others.map((p) => (
            <div className="provider-setup-summary" key={p.id}>
              <div className="provider-setup-text">
                <span className="settings-label">{p.display_name}</span>
                <HideToggle id={p.id} name={p.display_name} visibility={visibility} />
              </div>
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
      {showDefaultModel && <DefaultAiLine />}
    </div>
  )
}
