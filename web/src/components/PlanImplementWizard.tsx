import { useEffect, useMemo, useState } from 'react'

import { authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'
import Modal from './Modal'

interface ProjectLite {
  id: string
  name: string
}
interface AccountLite {
  id: string
  name: string
}

interface PlanImplementWizardProps {
  sessionId: string
  onClose: () => void
  /** Navigate to the authoring session once the instruction is sent. */
  onSent: (sessionId: string) => void
}

/** Multi-step wizard that turns a proposed plan into worker cards: pick the
 *  target project, provider, and account, then hand the authoring session an
 *  instruction to create the cards — the AI chooses the best model and system
 *  prompt per card (no non-thinking model for the planning). No new backend
 *  is needed: the session already has `create_card`. */
export default function PlanImplementWizard({
  sessionId,
  onClose,
  onSent,
}: PlanImplementWizardProps) {
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const [projects, setProjects] = useState<ProjectLite[]>([])
  const [accounts, setAccounts] = useState<AccountLite[]>([])
  const [projectId, setProjectId] = useState('')
  const [providerId, setProviderId] = useState('')
  const [accountId, setAccountId] = useState('')
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    void fetchModels()
    void authedFetch('/api/projects')
      .then((r) => (r.ok ? r.json() : []))
      .then((data) => setProjects(Array.isArray(data) ? data : (data.projects ?? [])))
      .catch(() => setProjects([]))
  }, [fetchModels])

  // Accounts for the chosen provider (claude/grok expose account lists; every
  // provider offers the implicit host "Default").
  useEffect(() => {
    setAccountId('')
    const endpoint =
      providerId === 'claude'
        ? '/api/claude-accounts'
        : providerId === 'grok'
          ? '/api/grok-accounts'
          : null
    if (!endpoint) {
      setAccounts([])
      return
    }
    void authedFetch(endpoint)
      .then((r) => (r.ok ? r.json() : []))
      .then((data) => setAccounts(Array.isArray(data) ? data : (data.accounts ?? [])))
      .catch(() => setAccounts([]))
  }, [providerId])

  const projectName = useMemo(
    () => projects.find((p) => p.id === projectId)?.name ?? '',
    [projects, projectId],
  )

  const submit = async () => {
    if (!projectId || !providerId) return
    setBusy(true)
    const acct = accountId ? `@${accountId}` : ''
    const scope = `${providerId}${acct}`
    const message =
      `Turn the plan you proposed into worker cards in project "${projectName}" (id ${projectId}).\n` +
      `Use provider+account ${scope} for the workers. For EACH card:\n` +
      `- pick the BEST model for that card's work within ${scope} (compare with list_models / get_model_guidance; ` +
      `use a thinking model for anything non-trivial — never a non-thinking model for planning),\n` +
      `- set the correct system_prompt_name (implement / research / debug / review / docs) for the work,\n` +
      `- give a clear title + description and set depends_on where order matters.\n` +
      `Create the cards with create_card, then summarize what you created.`
    try {
      await authedFetch(`/api/sessions/${sessionId}/message`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text: message }),
      })
      onSent(sessionId)
    } finally {
      setBusy(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={480} data-testid="plan-wizard">
      <div className="plan-wizard">
        <h2>Create cards from this plan</h2>

        <label className="plan-wizard__field">
          <span>Project</span>
          <select value={projectId} onChange={(e) => setProjectId(e.target.value)}>
            <option value="">Select a project…</option>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </label>

        <label className="plan-wizard__field">
          <span>Provider</span>
          <select value={providerId} onChange={(e) => setProviderId(e.target.value)}>
            <option value="">Select a provider…</option>
            {providers.map((p) => (
              <option key={p.id} value={p.id}>
                {p.display_name}
              </option>
            ))}
          </select>
        </label>

        <label className="plan-wizard__field">
          <span>Account</span>
          <select value={accountId} onChange={(e) => setAccountId(e.target.value)}>
            <option value="">Default</option>
            {accounts.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </select>
        </label>

        <p className="plan-wizard__hint">
          The session picks the best model and system prompt for each card within the chosen
          provider and account.
        </p>

        <div className="plan-wizard__actions">
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn btn--primary"
            disabled={busy || !projectId || !providerId}
            onClick={() => void submit()}
            data-testid="plan-wizard-create"
          >
            Create cards
          </button>
        </div>
      </div>
    </Modal>
  )
}
