import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { copyText } from '../lib/clipboard'
import { authedFetch } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import { useProjectsStore } from '../store/projects'
import { useTabsStore } from '../store/tabs'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import List from './List'
import Modal from './Modal'
import type { MenuItem } from './Dropdown'

/** One OAuth client (`GET /api/oauth-clients`). The secret is never part of
 *  this shape — it is returned exactly once, on create or rotate. */
interface OAuthClient {
  id: string
  name: string
  client_id: string
  folder_id: string
  folder_name: string
  project_id: string | null
  project_name: string | null
  /** Backing session `External: <name>` where the client's ask_user prompts land. */
  session_id: string
  created_at: string
  last_used_at: string | null
}

interface Endpoints {
  token_url: string
  mcp_url: string
  https_port: number
}

/** A freshly issued secret, shown once and then gone for good. */
interface IssuedSecret {
  client: OAuthClient
  secret: string
  rotated: boolean
}

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await authedFetch(path, {
    ...init,
    headers: init?.body ? { 'Content-Type': 'application/json' } : undefined,
  })
  if (!res.ok) {
    const body = (await res.json().catch(() => null)) as { error?: string } | null
    throw new Error(body?.error || `Request failed (${res.status})`)
  }
  return res.status === 204 ? (undefined as T) : ((await res.json()) as T)
}

function formatWhen(iso: string | null): string {
  if (!iso) return 'never'
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}

/** Same SPA navigation as the subagent "Open session" link: pushState plus a
 *  synthetic popstate so App's router picks it up, and the tab opens. */
function openSession(sessionId: string) {
  const path = `/sessions/${sessionId}`
  if (window.location.pathname !== path) window.history.pushState(null, '', path)
  window.dispatchEvent(new PopStateEvent('popstate'))
  void useTabsStore.getState().openTab('session', sessionId)
}

/** A read-only value with a copy button — endpoints, ids and the secret. */
function CopyField({
  id,
  label,
  value,
  multiline = false,
  testId,
}: {
  id: string
  label: string
  value: string
  multiline?: boolean
  testId: string
}) {
  const [copied, setCopied] = useState(false)
  return (
    <div className="form-field">
      <label className="form-label" htmlFor={id}>
        {label}
      </label>
      <div className="ext-access-copy-row">
        {multiline ? (
          <textarea
            id={id}
            className="form-input ext-access-mono"
            rows={value.split('\n').length}
            readOnly
            spellCheck={false}
            value={value}
            data-testid={testId}
          />
        ) : (
          <input
            id={id}
            className="form-input ext-access-mono"
            type="text"
            readOnly
            spellCheck={false}
            value={value}
            onFocus={(e) => e.target.select()}
            data-testid={testId}
          />
        )}
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={() =>
            void copyText(value).then((ok) => {
              setCopied(ok)
              if (ok) setTimeout(() => setCopied(false), 2000)
            })
          }
          aria-label={`Copy ${label}`}
          data-testid={`${testId}-copy`}
        >
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
    </div>
  )
}

/** One-time reveal of a client secret, with the two snippets an integrator
 *  needs: the token request and the MCP client config. */
function SecretModal({
  issued,
  endpoints,
  onClose,
}: {
  issued: IssuedSecret
  endpoints: Endpoints | null
  onClose: () => void
}) {
  const tokenUrl = endpoints?.token_url ?? '<token_url>'
  const mcpUrl = endpoints?.mcp_url ?? '<mcp_url>'
  const curl = `curl -s -X POST ${tokenUrl} -u '${issued.client.client_id}:${issued.secret}' -d grant_type=client_credentials`
  const mcpConfig = JSON.stringify(
    { type: 'http', url: mcpUrl, headers: { Authorization: 'Bearer <access_token>' } },
    null,
    2,
  )
  return (
    <Modal onClose={onClose} maxWidth={680} data-testid="ext-client-secret-modal">
      <h2>
        {issued.rotated ? 'New Secret' : 'Client Created'} — {issued.client.name}
      </h2>
      <p className="form-error" role="alert" data-testid="ext-client-secret-warning">
        Copy the client secret now. It is shown only this once and cannot be retrieved later
        {issued.rotated ? ' — the previous secret no longer works.' : '.'}
      </p>
      <CopyField
        id="ext-secret-client-id"
        label="Client ID"
        value={issued.client.client_id}
        testId="ext-client-secret-client-id"
      />
      <CopyField
        id="ext-secret-value"
        label="Client secret"
        value={issued.secret}
        testId="ext-client-secret-value"
      />
      <CopyField
        id="ext-secret-token-url"
        label="Token URL"
        value={tokenUrl}
        testId="ext-client-secret-token-url"
      />
      <CopyField
        id="ext-secret-curl"
        label="Get an access token"
        value={curl}
        multiline
        testId="ext-client-secret-curl"
      />
      <CopyField
        id="ext-secret-mcp-config"
        label="MCP server config"
        value={mcpConfig}
        multiline
        testId="ext-client-secret-mcp-config"
      />
      <div className="form-actions">
        <button
          type="button"
          className="btn-primary"
          onClick={onClose}
          data-testid="ext-client-secret-done"
        >
          Done
        </button>
      </div>
    </Modal>
  )
}

/** New client: a name plus the folder (and optional project) it is scoped to. */
function CreateClientModal({
  onClose,
  onCreated,
}: {
  onClose: () => void
  onCreated: (client: OAuthClient, secret: string) => void
}) {
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const projects = useProjectsStore((s) => s.projects)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const [name, setName] = useState('')
  const [chosenFolderId, setChosenFolderId] = useState<string | null>(null)
  const folderId = chosenFolderId ?? folders[0]?.id ?? ''
  const [projectId, setProjectId] = useState('')
  const [nameError, setNameError] = useState('')
  const [folderError, setFolderError] = useState('')
  const [projectError, setProjectError] = useState('')
  const [formError, setFormError] = useState('')
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    void fetchFolders()
    void fetchProjects()
  }, [fetchFolders, fetchProjects])

  const folderProjects = projects.filter((p) => p.folder_id === folderId)

  const disabledReason = !name.trim()
    ? 'Enter a name for the client.'
    : !folderId
      ? 'Add a folder first.'
      : null

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason || busy) return
    setNameError('')
    setFolderError('')
    setProjectError('')
    setFormError('')
    setBusy(true)
    try {
      const res = await api<{ client: OAuthClient; client_secret: string }>('/api/oauth-clients', {
        method: 'POST',
        body: JSON.stringify({
          name: name.trim(),
          folder_id: folderId,
          ...(projectId ? { project_id: projectId } : {}),
        }),
      })
      onCreated(res.client, res.client_secret)
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to create client'
      if (/project/i.test(message)) setProjectError(message)
      else if (/folder/i.test(message)) setFolderError(message)
      else if (/name/i.test(message)) setNameError(message)
      else setFormError(message)
      setBusy(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={520} data-testid="ext-client-create-modal">
      <h2>New OAuth Client</h2>
      <p className="form-hint">
        The client can only reach the folder (and project) chosen here. Its secret is shown once,
        right after it is created.
      </p>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="ext-client-name">
            Name
          </label>
          <input
            id="ext-client-name"
            className="form-input"
            type="text"
            value={name}
            autoComplete="off"
            placeholder="e.g. ci-bot"
            maxLength={200}
            onChange={(e) => {
              setName(e.target.value)
              setNameError('')
            }}
            autoFocus
            data-testid="ext-client-name"
          />
          <FieldError message={nameError} testId="ext-client-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ext-client-folder">
            Folder
          </label>
          {folders.length > 0 ? (
            <select
              id="ext-client-folder"
              className="form-input"
              value={folderId}
              onChange={(e) => {
                setChosenFolderId(e.target.value)
                // A project belongs to one folder; keeping the old pick would
                // post a project outside the chosen scope.
                setProjectId('')
                setFolderError('')
                setProjectError('')
              }}
              data-testid="ext-client-folder"
            >
              {folders.map((f) => (
                <option key={f.id} value={f.id}>
                  {f.name} — {f.path}
                </option>
              ))}
            </select>
          ) : (
            <p className="form-help">No folders yet.</p>
          )}
          <FieldError message={folderError} testId="ext-client-folder-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="ext-client-project">
            Project <span className="optional">(optional)</span>
          </label>
          <select
            id="ext-client-project"
            className="form-input"
            value={projectId}
            onChange={(e) => {
              setProjectId(e.target.value)
              setProjectError('')
            }}
            disabled={!folderId}
            data-testid="ext-client-project"
          >
            <option value="">None — the whole folder</option>
            {folderProjects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
          <FieldError message={projectError} testId="ext-client-project-error" />
        </div>
        {formError && (
          <p className="form-error" data-testid="ext-client-form-error">
            {formError}
          </p>
        )}
        <div className="form-actions">
          {!busy && disabledReason && (
            <span className="form-actions-reason" data-testid="ext-client-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="ext-client-submit"
          >
            {busy ? 'Creating…' : 'Create client'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/**
 * Settings → External Access: OAuth `client_credentials` clients that let an
 * outside system call Peckboard's MCP server, each scoped to one folder (and
 * optionally one project). Every client has a backing `External: <name>`
 * session where its ask_user prompts appear; the row opens it.
 *
 * Admin-only: the page is hidden from non-admins and the routes are
 * admin-gated server-side.
 */
export default function ExternalAccessSection() {
  const [clients, setClients] = useState<OAuthClient[]>([])
  const [loaded, setLoaded] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [endpoints, setEndpoints] = useState<Endpoints | null>(null)
  const [endpointsError, setEndpointsError] = useState<string | null>(null)
  const [showCreate, setShowCreate] = useState(false)
  const [issued, setIssued] = useState<IssuedSecret | null>(null)
  const [confirmRotate, setConfirmRotate] = useState<OAuthClient | null>(null)
  const [confirmRevoke, setConfirmRevoke] = useState<OAuthClient | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const fetchClients = useCallback(
    () =>
      api<OAuthClient[]>('/api/oauth-clients').then(
        (cs) => {
          setClients(cs)
          setError(null)
          setLoaded(true)
        },
        (e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load clients'),
      ),
    [],
  )

  useEffect(() => {
    void fetchClients()
    api<Endpoints>('/api/oauth-clients/endpoints').then(
      (ep) => {
        setEndpoints(ep)
        setEndpointsError(null)
      },
      (e: unknown) =>
        setEndpointsError(e instanceof Error ? e.message : 'Failed to load endpoints'),
    )
  }, [fetchClients])

  const buildMenu = (c: OAuthClient): MenuItem[] => [
    { label: 'Open session', onSelect: () => openSession(c.session_id) },
    {
      label: 'Rotate secret',
      onSelect: () => {
        setActionError(null)
        setConfirmRotate(c)
      },
    },
    { divider: true },
    {
      label: 'Revoke',
      danger: true,
      onSelect: () => {
        setActionError(null)
        setConfirmRevoke(c)
      },
    },
  ]

  return (
    <section className="settings-section ext-access" data-testid="external-access-section">
      <div className="settings-section-head">
        <h3>External Access</h3>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => setShowCreate(true)}
          data-testid="ext-client-new"
        >
          + New client
        </button>
      </div>
      <p className="form-hint">
        Let an outside system drive Peckboard through its MCP server. Each client trades its ID and
        secret at the token URL (OAuth <code>client_credentials</code>) for a bearer token, and can
        only reach the folder — and optional project — it was created for. Its activity is recorded
        in its <em>External: &lt;name&gt;</em> session.
      </p>

      <div className="ext-access-endpoints" data-testid="ext-access-endpoints">
        {endpoints ? (
          <>
            <CopyField
              id="ext-endpoint-token"
              label="Token URL"
              value={endpoints.token_url}
              testId="ext-endpoint-token-url"
            />
            <CopyField
              id="ext-endpoint-mcp"
              label="MCP URL"
              value={endpoints.mcp_url}
              testId="ext-endpoint-mcp-url"
            />
          </>
        ) : endpointsError ? (
          <p className="form-error" data-testid="ext-endpoints-error">
            {endpointsError}
          </p>
        ) : (
          <p className="form-hint">Loading endpoints…</p>
        )}
      </div>

      {error && (
        <div className="form-error" data-testid="ext-clients-error">
          <span>{error}</span>{' '}
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void fetchClients()}
            data-testid="ext-clients-retry"
          >
            Retry
          </button>
        </div>
      )}

      <List<OAuthClient>
        items={clients}
        getKey={(c) => c.id}
        bodyClassName="list-view-rows"
        onActivate={(c) => openSession(c.session_id)}
        getMenuItems={buildMenu}
        renderItem={(c) => (
          <>
            <span className="list-view-name" data-testid={`ext-client-row-${c.name}`}>
              {c.name}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag" data-testid={`ext-client-scope-${c.name}`}>
                {c.project_name ? `${c.folder_name} › ${c.project_name}` : c.folder_name}
              </span>
              <code className="ext-access-mono" data-testid={`ext-client-id-${c.name}`}>
                {c.client_id}
              </code>
              <span>Created {formatWhen(c.created_at)}</span>
              <span>Last used {formatWhen(c.last_used_at)}</span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="ext-clients-empty">
            {error
              ? 'The client list could not be loaded.'
              : loaded
                ? 'No OAuth clients yet — create one to let an external system in.'
                : 'Loading…'}
          </div>
        }
      />

      {showCreate && (
        <CreateClientModal
          onClose={() => setShowCreate(false)}
          onCreated={(client, secret) => {
            setShowCreate(false)
            setClients((cs) => [...cs, client])
            setIssued({ client, secret, rotated: false })
          }}
        />
      )}
      {issued && (
        <SecretModal issued={issued} endpoints={endpoints} onClose={() => setIssued(null)} />
      )}
      {confirmRotate && (
        <ConfirmDialog
          title="Rotate secret"
          message={`Issue a new secret for "${confirmRotate.name}"? The current secret stops working immediately; anything using it must be updated with the new one.`}
          confirmLabel="Rotate"
          busy={busy}
          busyLabel="Rotating…"
          error={actionError}
          testId="ext-client-rotate-confirm"
          onConfirm={() => {
            const target = confirmRotate
            setActionError(null)
            setBusy(true)
            void api<{ client_secret: string }>(`/api/oauth-clients/${target.id}/rotate-secret`, {
              method: 'POST',
            })
              .then((res) => {
                setConfirmRotate(null)
                setIssued({ client: target, secret: res.client_secret, rotated: true })
              })
              .catch((e: unknown) =>
                setActionError(e instanceof Error ? e.message : 'Failed to rotate secret'),
              )
              .finally(() => setBusy(false))
          }}
          onCancel={() => setConfirmRotate(null)}
        />
      )}
      {confirmRevoke && (
        <ConfirmDialog
          title="Revoke client"
          message={`Revoke "${confirmRevoke.name}"? Its secret and any access tokens stop working immediately and the external system loses access. This cannot be undone.`}
          confirmLabel="Revoke"
          danger
          busy={busy}
          busyLabel="Revoking…"
          error={actionError}
          testId="ext-client-revoke-confirm"
          onConfirm={() => {
            const target = confirmRevoke
            setActionError(null)
            setBusy(true)
            void api(`/api/oauth-clients/${target.id}`, { method: 'DELETE' })
              .then(() => {
                setConfirmRevoke(null)
                setClients((cs) => cs.filter((c) => c.id !== target.id))
              })
              .catch((e: unknown) =>
                setActionError(e instanceof Error ? e.message : 'Failed to revoke client'),
              )
              .finally(() => setBusy(false))
          }}
          onCancel={() => setConfirmRevoke(null)}
        />
      )}
    </section>
  )
}
