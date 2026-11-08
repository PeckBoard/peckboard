import { useEffect, useState, type FormEvent } from 'react'
import Modal from './Modal'
import HelpTip from './HelpTip'
import PathAutocomplete from './PathAutocomplete'
import ProviderSetupPanel from './ProviderSetupPanel'
import FieldError from './FieldError'
import { authedFetch, useAuthStore } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import { isProviderReady, useProviderSetupStore } from '../store/providerSetup'
import { MIN_PASSWORD_LEN } from '../utils/password'
import type { Folder } from '../types/api'

const STEPS = ['Welcome', 'Connect your AI', 'Where files live', 'Ready'] as const
const FOLDER_STEP = 2
const LAST_STEP = STEPS.length - 1

/** Name given to the ready-made workspace folder. */
const DEFAULT_FOLDER_NAME = 'My work'

/** Where to go once the wizard closes. */
export type SetupNext = 'chat' | 'project' | 'tour'

interface Props {
  /** Called after `POST /api/settings/setup/complete` succeeds, with the
   *  action the user picked on the last step (none for a plain Finish). */
  onDone: (next?: SetupNext) => void
}

/**
 * First-run setup wizard, shown to the bootstrap admin exactly once (the
 * server seeds `setup_state` as incomplete only on a brand-new install).
 * Four steps: say what PeckBoard is and replace the temporary password
 * (mandatory), connect an AI tool and pick the default model, choose where
 * files live, then hand off to a first chat, project, or tour. Every step
 * writes through the same endpoints Settings uses, so each stays re-runnable
 * there — the wizard is convenience, not the only path.
 *
 * The modal is deliberately non-dismissible: steps 2–4 are one click to
 * pass, and only an explicit finish marks setup complete, so closing the
 * browser mid-wizard brings it back on the next login.
 */
export default function SetupWizard({ onDone }: Props) {
  const [step, setStep] = useState(0)

  // ── Step 1: welcome + password (mandatory) ───────────────────────
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
  const providerStatuses = useProviderSetupStore((s) => s.providers)
  const anyProviderReady = providerStatuses?.some(isProviderReady) ?? false

  // ── Step 3: where files live ─────────────────────────────────────
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const createFolder = useFoldersStore((s) => s.createFolder)
  const [suggestedPath, setSuggestedPath] = useState<string | null>(null)
  const [homeLoaded, setHomeLoaded] = useState(false)
  const [customOpen, setCustomOpen] = useState(false)
  const [folderName, setFolderName] = useState('')
  const [folderPath, setFolderPath] = useState('')
  const [folderCreateDir, setFolderCreateDir] = useState(true)
  const [folderPathExists, setFolderPathExists] = useState<boolean | null>(null)
  const [folderBusy, setFolderBusy] = useState(false)
  const [folderError, setFolderError] = useState('')
  const [createdFolder, setCreatedFolder] = useState<Folder | null>(null)

  useEffect(() => {
    void fetchFolders()
    let cancelled = false
    authedFetch('/api/folders/home')
      .then((r) => (r.ok ? r.json() : null))
      .then((data: { suggested?: string | null } | null) => {
        if (cancelled) return
        setSuggestedPath(data?.suggested ?? null)
        setHomeLoaded(true)
      })
      .catch(() => {
        if (!cancelled) setHomeLoaded(true)
      })
    return () => {
      cancelled = true
    }
  }, [fetchFolders])

  // A folder made here, or one that already exists on a re-run.
  const doneFolder = createdFolder ?? folders[0] ?? null
  // With nothing to offer (no home dir), the fields are the only way in.
  const showCustom = customOpen || (homeLoaded && !suggestedPath && !doneFolder)
  const customFilled = folderName.trim() !== '' || folderPath.trim() !== ''

  const saveFolder = async (name: string, path: string, create: boolean): Promise<boolean> => {
    setFolderError('')
    setFolderBusy(true)
    try {
      const folder = await createFolder(name, path, create)
      setCreatedFolder(folder)
      setCustomOpen(false)
      setFolderName('')
      setFolderPath('')
      return true
    } catch (err) {
      setFolderError(err instanceof Error ? err.message : 'Could not add the folder.')
      return false
    } finally {
      setFolderBusy(false)
    }
  }

  const takeSuggested = () => {
    if (folderBusy || !suggestedPath) return
    void saveFolder(DEFAULT_FOLDER_NAME, suggestedPath, true)
  }

  /** What blocks the typed folder from saving, in plain words. */
  const customProblem = (): string | null => {
    if (!folderName.trim()) return 'Give the folder a name.'
    if (!folderPath.trim()) return 'Enter where the folder is on this computer.'
    return null
  }

  /** Save the typed folder, or say why it can't be saved. Never drops it. */
  const saveCustom = async (): Promise<boolean> => {
    if (folderBusy) return false
    const problem = customProblem()
    if (problem) {
      setFolderError(problem)
      return false
    }
    return saveFolder(folderName.trim(), folderPath.trim(), folderCreateDir)
  }

  // ── Step 4: ready ────────────────────────────────────────────────
  const [finishBusy, setFinishBusy] = useState(false)
  const [finishError, setFinishError] = useState('')

  const finish = async (next?: SetupNext) => {
    if (finishBusy) return
    setFinishError('')
    setFinishBusy(true)
    try {
      const res = await authedFetch('/api/settings/setup/complete', { method: 'POST' })
      if (!res.ok) {
        const data = (await res.json().catch(() => null)) as { error?: string } | null
        throw new Error(data?.error || `HTTP ${res.status}`)
      }
      onDone(next)
    } catch (err) {
      setFinishError(err instanceof Error ? err.message : 'Failed to finish setup')
      setFinishBusy(false)
    }
  }

  const onNext = async () => {
    if (step === LAST_STEP) {
      void finish()
      return
    }
    // Typed folder fields are saved, or block with the reason — never lost.
    if (step === FOLDER_STEP && showCustom && customFilled) {
      if (!(await saveCustom())) return
    }
    setStep(step + 1)
  }

  const nextDisabled =
    step === 0 ? pwBusy || (!pwDone && !pwValid) : step === LAST_STEP ? finishBusy : folderBusy

  return (
    <Modal maxWidth={640} className="setup-wizard-modal" data-testid="setup-wizard">
      <h2 className="setup-wizard-title">Welcome to PeckBoard 🐣</h2>

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
            <p className="setup-wizard-lead" data-testid="setup-what-is">
              PeckBoard gives you AI helpers that do real work on this computer — chat with one, or
              hand a board of tasks to several. They run on an AI plan you already pay for.
            </p>
            <p className="form-hint">
              First, swap the temporary password for one of your own. This step is required.
            </p>
            {pwDone ? (
              <p className="setup-wizard-done" role="status" data-testid="setup-pw-done">
                Password updated. You can move on.
              </p>
            ) : (
              <>
                <div className="form-field">
                  <label className="form-label" htmlFor="setup-pw-current">
                    Password you just signed in with
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
                  <span className="form-hint">
                    The temporary one shown when PeckBoard first started.
                  </span>
                </div>
                <div className="setup-wizard-pw-pair">
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
                      Type it again
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
                </div>
                <FieldError message={pwError || undefined} testId="setup-pw-error" />
              </>
            )}
          </form>
        )}

        {step === 1 && (
          <div>
            <h3>Connect your AI</h3>
            <p className="form-hint">
              PeckBoard works through one of these AI tools, signed in with your own plan. Set up at
              least one; you can add more later in Settings.
            </p>
            <ProviderSetupPanel showDefaultModel />
          </div>
        )}

        {step === FOLDER_STEP && (
          <div>
            <h3>
              Where files live <HelpTip term="folder" />
            </h3>
            <p className="form-hint">
              The AI can read and change files in this folder. You can add more later.
            </p>

            {doneFolder ? (
              <p className="setup-wizard-done" role="status" data-testid="setup-folder-done">
                ✓ Using <strong>{doneFolder.name}</strong>
                <span className="setup-wizard-path">{doneFolder.path}</span>
              </p>
            ) : (
              suggestedPath &&
              !customOpen && (
                <div className="setup-wizard-choice" data-testid="setup-folder-default">
                  <span className="setup-wizard-path" data-testid="setup-folder-default-path">
                    {suggestedPath}
                  </span>
                  <button
                    type="button"
                    className="btn-primary"
                    onClick={takeSuggested}
                    disabled={folderBusy}
                    data-testid="setup-folder-use-default"
                  >
                    {folderBusy ? 'Setting up…' : 'Use this folder'}
                  </button>
                </div>
              )
            )}

            {!showCustom && (
              <button
                type="button"
                className="btn-link setup-wizard-other"
                onClick={() => {
                  setFolderError('')
                  setCustomOpen(true)
                }}
                data-testid="setup-folder-choose-other"
              >
                {doneFolder ? 'Add another folder…' : 'Choose another folder…'}
              </button>
            )}

            {showCustom && (
              <div className="folder-create-fields">
                <input
                  className="form-input"
                  placeholder="Name (e.g. My work)"
                  aria-label="Folder name"
                  value={folderName}
                  data-testid="setup-folder-name"
                  onChange={(e) => setFolderName(e.target.value)}
                />
                <PathAutocomplete
                  value={folderPath}
                  onChange={setFolderPath}
                  onSubmit={() => void saveCustom()}
                  onExistsChange={setFolderPathExists}
                  placeholder="Where it is (e.g. /home/me/projects)"
                  testId="setup-folder-path"
                />
                {folderPath.trim().startsWith('/') && folderPathExists !== null && (
                  <p className="form-hint" role="status" style={{ margin: 0 }}>
                    {folderPathExists
                      ? 'This folder exists.'
                      : folderCreateDir
                        ? "This folder doesn't exist yet — it will be created."
                        : "This folder doesn't exist — tick the box below or fix the path."}
                  </p>
                )}
                <label className="form-checkbox-label" style={{ fontSize: 'var(--text-sm)' }}>
                  <input
                    type="checkbox"
                    checked={folderCreateDir}
                    onChange={(e) => setFolderCreateDir(e.target.checked)}
                    data-testid="setup-folder-create-dir"
                  />
                  <span>Create it if it doesn&apos;t exist</span>
                </label>
                <div className="setup-wizard-inline-actions">
                  <button
                    type="button"
                    className="btn-secondary"
                    onClick={() => void saveCustom()}
                    disabled={folderBusy}
                    data-testid="setup-folder-add"
                  >
                    {folderBusy ? 'Adding…' : 'Add folder'}
                  </button>
                  {(suggestedPath || doneFolder) && (
                    <button
                      type="button"
                      className="btn-link"
                      onClick={() => {
                        setCustomOpen(false)
                        setFolderName('')
                        setFolderPath('')
                        setFolderError('')
                      }}
                      data-testid="setup-folder-cancel-custom"
                    >
                      Cancel
                    </button>
                  )}
                </div>
              </div>
            )}
            <FieldError message={folderError || undefined} testId="setup-folder-error" />
          </div>
        )}

        {step === LAST_STEP && (
          <div>
            <h3>You&apos;re ready</h3>
            <p className="form-hint">What would you like to do first?</p>
            <div className="setup-wizard-final">
              <button
                type="button"
                className="setup-wizard-final-action"
                onClick={() => void finish('chat')}
                disabled={finishBusy}
                data-testid="setup-final-chat"
              >
                <span className="setup-wizard-final-title">Start a chat</span>
                <span className="setup-wizard-final-hint">
                  Ask an AI helper to do something now.
                </span>
              </button>
              <button
                type="button"
                className="setup-wizard-final-action"
                onClick={() => void finish('project')}
                disabled={finishBusy}
                data-testid="setup-final-project"
              >
                <span className="setup-wizard-final-title">Create a project</span>
                <span className="setup-wizard-final-hint">
                  A board of tasks the AI works through.
                </span>
              </button>
              <button
                type="button"
                className="setup-wizard-final-action"
                onClick={() => void finish('tour')}
                disabled={finishBusy}
                data-testid="setup-final-tour"
              >
                <span className="setup-wizard-final-title">Show me around</span>
                <span className="setup-wizard-final-hint">A short checklist to get started.</span>
              </button>
            </div>
            <p className="form-hint setup-wizard-outro" data-testid="setup-remote-note">
              Using PeckBoard from your phone or another computer? See Settings → Secure connection
              (HTTPS).
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
            className={step === LAST_STEP ? 'btn-secondary' : 'btn-primary'}
            onClick={() => void onNext()}
            disabled={nextDisabled}
            data-testid={step === LAST_STEP ? 'setup-finish' : 'setup-next'}
          >
            {step === LAST_STEP ? (finishBusy ? 'Finishing…' : 'Finish') : 'Next'}
          </button>
        )}
        {step === 0 && !pwDone && !pwValid && (
          <span className="form-actions-reason">Set a new password to continue.</span>
        )}
        {step === 1 && providerStatuses && !anyProviderReady && (
          <span className="form-actions-reason" data-testid="setup-no-provider-hint">
            You can skip this, but the AI can&apos;t work until one tool is connected.
          </span>
        )}
        {step === FOLDER_STEP && !doneFolder && !(showCustom && customFilled) && (
          <span className="form-actions-reason" data-testid="setup-no-folder-hint">
            You can skip this and add a folder later.
          </span>
        )}
        {step === FOLDER_STEP && showCustom && customFilled && (
          <span className="form-actions-reason" data-testid="setup-folder-next-hint">
            Next adds the folder above.
          </span>
        )}
      </div>
    </Modal>
  )
}
