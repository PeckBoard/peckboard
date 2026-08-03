import { useEffect, useMemo, useState, type FormEvent } from 'react'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import PathAutocomplete from './PathAutocomplete'
import TlsSettingsSection from './TlsSettingsSection'
import FieldError from './FieldError'
import { authedFetch, useAuthStore } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import { useResourcesStore } from '../store/resources'
import { MIN_PASSWORD_LEN } from '../utils/password'
import type { Folder } from '../types/api'

interface ProviderVisibility {
  id: string
  display_name: string
  hidden: boolean
}

const STEPS = ['Password', 'Providers', 'Default model', 'Folder', 'HTTPS'] as const

interface Props {
  /** Called after `POST /api/settings/setup/complete` succeeds. */
  onDone: () => void
}

/**
 * First-run setup wizard, shown to the bootstrap admin exactly once (the
 * server seeds `setup_state` as incomplete only on a brand-new install).
 * Five steps: change the printed bootstrap password (mandatory), pick
 * visible providers, choose a default model, register a workspace folder,
 * and review TLS. Every step writes through the same settings endpoints
 * the Settings page uses, so each one stays re-runnable there later —
 * the wizard is convenience, not the only path.
 *
 * The modal is deliberately non-dismissible: steps 2–5 are one click to
 * pass, and only the explicit Finish marks setup complete, so closing the
 * browser mid-wizard brings it back on the next login.
 */
export default function SetupWizard({ onDone }: Props) {
  const [step, setStep] = useState(0)

  // ── Step 1: password (mandatory) ─────────────────────────────────
  const changePassword = useAuthStore((s) => s.changePassword)
  const [currentPassword, setCurrentPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [pwBusy, setPwBusy] = useState(false)
  const [pwError, setPwError] = useState('')
  const [pwDone, setPwDone] = useState(false)

  const pwValid =
    newPassword.length >= MIN_PASSWORD_LEN &&
    newPassword === confirmPassword &&
    currentPassword.length > 0

  const submitPassword = async (e: FormEvent) => {
    e.preventDefault()
    if (!pwValid || pwBusy) return
    setPwError('')
    setPwBusy(true)
    try {
      await changePassword(currentPassword, newPassword)
      setPwDone(true)
      setCurrentPassword('')
      setNewPassword('')
      setConfirmPassword('')
      setStep(1)
    } catch (err) {
      setPwError(err instanceof Error ? err.message : 'Failed to change password')
    } finally {
      setPwBusy(false)
    }
  }

  // ── Step 2: providers ────────────────────────────────────────────
  const [providers, setProviders] = useState<ProviderVisibility[] | null>(null)
  const [providersError, setProvidersError] = useState('')

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/settings/providers')
      .then((res) => (res.ok ? res.json() : null))
      .then((data: { providers?: ProviderVisibility[] } | null) => {
        if (!cancelled && data?.providers) setProviders(data.providers)
      })
      .catch(() => {
        if (!cancelled) setProvidersError('Could not load the provider list.')
      })
    return () => {
      cancelled = true
    }
  }, [])

  const toggleProvider = (id: string, hidden: boolean) => {
    setProvidersError('')
    // Optimistic; a refused save reverts so the checkboxes never lie.
    setProviders((prev) => prev?.map((p) => (p.id === id ? { ...p, hidden } : p)) ?? prev)
    void authedFetch(`/api/settings/providers/${id}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ hidden }),
    })
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
        fetchModels()
      })
      .catch((e: Error) => {
        setProviders(
          (prev) => prev?.map((p) => (p.id === id ? { ...p, hidden: !hidden } : p)) ?? prev,
        )
        setProvidersError(`Could not update provider: ${e.message}`)
      })
  }

  // ── Step 3: default model ────────────────────────────────────────
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const defaultModel = useResourcesStore((s) => s.defaultModel)
  const fetchDefaultModel = useResourcesStore((s) => s.fetchDefaultModel)
  const setDefaultModelLocal = useResourcesStore((s) => s.setDefaultModelLocal)
  const [modelError, setModelError] = useState('')

  useEffect(() => {
    fetchModels()
    fetchDefaultModel()
  }, [fetchModels, fetchDefaultModel])

  // `/api/models` already omits hidden providers, but the catalogue may
  // predate a toggle made on step 2 — drop models whose provider was just
  // hidden there. Filter by the hidden set (not the enabled set) so
  // providers absent from the settings list (e.g. mock) keep their models.
  // Bare ids default to `claude` (backend convention).
  const visibleModels = useMemo(() => {
    if (!providers) return models
    const hidden = new Set(providers.filter((p) => p.hidden).map((p) => p.id))
    return models.filter((m) => !hidden.has(m.id.includes(':') ? m.id.split(':')[0] : 'claude'))
  }, [models, providers])

  const changeDefaultModel = (model: string) => {
    const prev = defaultModel
    setModelError('')
    setDefaultModelLocal(model)
    void authedFetch('/api/settings/default-model', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model }),
    })
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
      })
      .catch((e: Error) => {
        setDefaultModelLocal(prev)
        setModelError(`Could not save the default model: ${e.message}`)
      })
  }

  // ── Step 4: workspace folder ─────────────────────────────────────
  const createFolder = useFoldersStore((s) => s.createFolder)
  const [folderName, setFolderName] = useState('')
  const [folderPath, setFolderPath] = useState('')
  const [folderCreateDir, setFolderCreateDir] = useState(false)
  const [folderPathExists, setFolderPathExists] = useState<boolean | null>(null)
  const [folderBusy, setFolderBusy] = useState(false)
  const [folderError, setFolderError] = useState('')
  const [createdFolder, setCreatedFolder] = useState<Folder | null>(null)

  const addFolder = async () => {
    if (folderBusy || !folderName.trim() || !folderPath.trim()) return
    setFolderError('')
    setFolderBusy(true)
    try {
      const folder = await createFolder(folderName.trim(), folderPath.trim(), folderCreateDir)
      setCreatedFolder(folder)
      setFolderName('')
      setFolderPath('')
    } catch (err) {
      setFolderError(err instanceof Error ? err.message : 'Failed to create folder')
    } finally {
      setFolderBusy(false)
    }
  }

  // ── Step 5: finish ───────────────────────────────────────────────
  const [finishBusy, setFinishBusy] = useState(false)
  const [finishError, setFinishError] = useState('')

  const finish = async () => {
    setFinishError('')
    setFinishBusy(true)
    try {
      const res = await authedFetch('/api/settings/setup/complete', { method: 'POST' })
      if (!res.ok) {
        const data = (await res.json().catch(() => null)) as { error?: string } | null
        throw new Error(data?.error || `HTTP ${res.status}`)
      }
      onDone()
    } catch (err) {
      setFinishError(err instanceof Error ? err.message : 'Failed to finish setup')
      setFinishBusy(false)
    }
  }

  const nextLabel = step === STEPS.length - 1 ? (finishBusy ? 'Finishing…' : 'Finish') : 'Next'
  const nextDisabled = step === 0 ? pwBusy || (!pwDone && !pwValid) : step === 4 && finishBusy

  const onNext = () => {
    if (step === STEPS.length - 1) {
      void finish()
    } else {
      setStep(step + 1)
    }
  }

  return (
    <Modal maxWidth={640} className="setup-wizard-modal" data-testid="setup-wizard">
      <h2>Welcome to PeckBoard 🐣</h2>
      <p className="form-hint setup-wizard-intro">
        A few one-time choices to get your install ready. Everything here can be changed again later
        in Settings.
      </p>

      <ol className="setup-wizard-steps" data-testid="setup-progress">
        {STEPS.map((label, i) => (
          <li
            key={label}
            className={`setup-wizard-step ${i === step ? 'current' : ''} ${i < step ? 'done' : ''}`}
            aria-current={i === step ? 'step' : undefined}
            data-testid={`setup-progress-step-${i + 1}`}
          >
            <span className="setup-wizard-step-dot" aria-hidden="true">
              {i < step ? '✓' : i + 1}
            </span>
            <span className="setup-wizard-step-label">{label}</span>
          </li>
        ))}
      </ol>

      <div className="setup-wizard-body">
        {step === 0 && (
          <form id="setup-pw-form" onSubmit={(e) => void submitPassword(e)}>
            <h3>Change the admin password</h3>
            <p className="form-hint">
              PeckBoard created this admin account on first start and printed its password to the
              server&apos;s console (stdout). Replace it with a password of your own — this step is
              required.
            </p>
            {pwDone ? (
              <p className="setup-wizard-done" role="status" data-testid="setup-pw-done">
                Password updated. You can move on.
              </p>
            ) : (
              <>
                <div className="form-field">
                  <label className="form-label" htmlFor="setup-pw-current">
                    Current password
                  </label>
                  <input
                    id="setup-pw-current"
                    data-testid="setup-pw-current"
                    className="form-input"
                    type="password"
                    autoComplete="current-password"
                    value={currentPassword}
                    onChange={(e) => setCurrentPassword(e.target.value)}
                    autoFocus
                    required
                  />
                  <span className="form-hint">The password printed to the server console.</span>
                </div>
                <div className="form-field">
                  <label className="form-label" htmlFor="setup-pw-new">
                    New password
                  </label>
                  <input
                    id="setup-pw-new"
                    data-testid="setup-pw-new"
                    className="form-input"
                    type="password"
                    autoComplete="new-password"
                    value={newPassword}
                    onChange={(e) => setNewPassword(e.target.value)}
                    minLength={MIN_PASSWORD_LEN}
                    required
                  />
                  <span className="form-hint">At least {MIN_PASSWORD_LEN} characters</span>
                </div>
                <div className="form-field">
                  <label className="form-label" htmlFor="setup-pw-confirm">
                    Confirm new password
                  </label>
                  <input
                    id="setup-pw-confirm"
                    data-testid="setup-pw-confirm"
                    className="form-input"
                    type="password"
                    autoComplete="new-password"
                    value={confirmPassword}
                    onChange={(e) => setConfirmPassword(e.target.value)}
                    required
                  />
                </div>
                <FieldError message={pwError || undefined} testId="setup-pw-error" />
              </>
            )}
          </form>
        )}

        {step === 1 && (
          <div>
            <h3>Enable providers</h3>
            <p className="form-hint">
              Pick which agent providers show up across PeckBoard. Hidden providers simply disappear
              from model pickers — nothing is uninstalled, and you can re-enable them any time in
              Settings → Providers.
            </p>
            {providers === null ? (
              <p className="settings-loading">Loading providers…</p>
            ) : (
              <div className="settings-info-grid">
                {providers.map((p) => (
                  <label className="settings-row" key={p.id}>
                    <span className="settings-label">{p.display_name}</span>
                    <input
                      type="checkbox"
                      checked={!p.hidden}
                      data-testid={`setup-provider-toggle-${p.id}`}
                      onChange={(e) => toggleProvider(p.id, !e.target.checked)}
                    />
                  </label>
                ))}
              </div>
            )}
            <FieldError message={providersError || undefined} testId="setup-providers-error" />
          </div>
        )}

        {step === 2 && (
          <div>
            <h3>Default model</h3>
            <p className="form-hint">
              Preselected for new sessions, cards, and reviews. Only models from the providers you
              left enabled are offered — type to search.
            </p>
            <ModelPicker
              value={defaultModel}
              onChange={changeDefaultModel}
              models={visibleModels}
              ariaLabel="Default model"
              testId="setup-default-model"
              emptyHint="No models available — enable a provider on the previous step."
              onOpen={fetchModels}
            />
            <FieldError message={modelError || undefined} testId="setup-model-error" />
          </div>
        )}

        {step === 3 && (
          <div>
            <h3>Register a workspace folder</h3>
            <p className="form-hint">
              Folders map to directories on disk; sessions and projects live inside them. You can
              skip this and add folders later in the Folders page.
            </p>
            {createdFolder && (
              <p className="setup-wizard-done" role="status" data-testid="setup-folder-done">
                Added <strong>{createdFolder.name}</strong> ({createdFolder.path}). You can add more
                later.
              </p>
            )}
            <div className="folder-create-fields">
              <input
                className="form-input"
                placeholder="Name (e.g. My Workspace)"
                value={folderName}
                data-testid="setup-folder-name"
                onChange={(e) => setFolderName(e.target.value)}
              />
              <PathAutocomplete
                value={folderPath}
                onChange={setFolderPath}
                onSubmit={() => void addFolder()}
                onExistsChange={setFolderPathExists}
                placeholder="Path (e.g. /home/me/projects)"
                testId="setup-folder-path"
              />
              {folderPath.trim().startsWith('/') && folderPathExists !== null && (
                <p className="form-hint" role="status" style={{ margin: 0 }}>
                  {folderPathExists
                    ? 'Directory exists on the server.'
                    : folderCreateDir
                      ? "Directory doesn't exist yet — it will be created."
                      : "Directory doesn't exist — check 'Create directory' below or fix the path."}
                </p>
              )}
              <label className="form-checkbox-label" style={{ fontSize: 'var(--text-sm)' }}>
                <input
                  type="checkbox"
                  checked={folderCreateDir}
                  onChange={(e) => setFolderCreateDir(e.target.checked)}
                />
                <span>Create directory if it doesn&apos;t exist</span>
              </label>
              <button
                type="button"
                className="btn-secondary"
                onClick={() => void addFolder()}
                disabled={folderBusy || !folderName.trim() || !folderPath.trim()}
                data-testid="setup-folder-add"
                style={{ alignSelf: 'flex-start' }}
              >
                {folderBusy ? 'Adding…' : 'Add Folder'}
              </button>
            </div>
            <FieldError message={folderError || undefined} testId="setup-folder-error" />
          </div>
        )}

        {step === 4 && (
          <div>
            <h3>TLS / HTTPS review</h3>
            <TlsSettingsSection />
            <p className="form-hint setup-wizard-outro">
              That&apos;s everything. Each of these steps can be re-run any time from Settings —
              Finish just puts this wizard to bed.
            </p>
            <FieldError message={finishError || undefined} testId="setup-finish-error" />
          </div>
        )}
      </div>

      <div className="form-actions setup-wizard-actions">
        {step > 0 && (
          <button
            type="button"
            className="btn-secondary"
            onClick={() => setStep(step - 1)}
            data-testid="setup-back"
          >
            Back
          </button>
        )}
        {step === 0 && !pwDone ? (
          <button
            type="submit"
            form="setup-pw-form"
            className="btn-primary"
            disabled={nextDisabled}
            data-testid="setup-next"
          >
            {pwBusy ? 'Saving…' : 'Next'}
          </button>
        ) : (
          <button
            type="button"
            className="btn-primary"
            onClick={onNext}
            disabled={nextDisabled}
            data-testid={step === STEPS.length - 1 ? 'setup-finish' : 'setup-next'}
          >
            {nextLabel}
          </button>
        )}
        {step === 0 && !pwDone && !pwValid && (
          <span className="form-actions-reason">Set a new admin password to continue.</span>
        )}
      </div>
    </Modal>
  )
}
