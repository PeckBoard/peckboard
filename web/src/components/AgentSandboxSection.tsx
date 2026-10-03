import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'

type SandboxMode = 'enforce' | 'warn' | 'off'

interface SandboxState {
  mode: SandboxMode
  extra_rw: string[]
  status: { supported: boolean; enforced: boolean; abi: number; platform: string }
}

const MODE_LABELS: Record<SandboxMode, string> = {
  enforce: 'Enforce',
  warn: 'Warn only',
  off: 'Off',
}

/**
 * Settings → Security: the agent sandbox (Landlock on Linux) that keeps
 * agent processes out of PeckBoard's data dir. Shows a warning banner
 * whenever new agents would run unsandboxed (mode not enforced, or the
 * kernel lacks Landlock).
 */
export default function AgentSandboxSection() {
  const [state, setState] = useState<SandboxState | null>(null)
  const [extraText, setExtraText] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [confirmMode, setConfirmMode] = useState<SandboxMode | null>(null)

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/settings/agent-sandbox')
      .then((res) => (res.ok ? (res.json() as Promise<SandboxState>) : null))
      .then((data) => {
        if (cancelled || !data) return
        setState(data)
        setExtraText(data.extra_rw.join('\n'))
      })
      .catch(() => {
        if (!cancelled) setError('Could not load the agent sandbox status.')
      })
    return () => {
      cancelled = true
    }
  }, [])

  const save = async (mode: SandboxMode, extra: string[]) => {
    setError(null)
    try {
      const res = await authedFetch('/api/settings/agent-sandbox/config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ mode, extra_rw: extra }),
      })
      const data = (await res.json().catch(() => null)) as
        | (SandboxState & { error?: string })
        | null
      if (!res.ok || !data) throw new Error(data?.error ?? `HTTP ${res.status}`)
      setState(data)
      setExtraText(data.extra_rw.join('\n'))
    } catch (e) {
      setError(`Could not save the agent sandbox setting: ${(e as Error).message}`)
    }
  }

  const extraPaths = () =>
    extraText
      .split('\n')
      .map((l) => l.trim())
      .filter(Boolean)

  const pickMode = (mode: SandboxMode) => {
    if (!state || mode === state.mode) return
    if (mode === 'enforce') void save(mode, state.extra_rw)
    else setConfirmMode(mode)
  }

  const unsandboxed = state && !state.status.enforced

  return (
    <section
      className="settings-section"
      data-testid="agent-sandbox-section"
      data-settings-anchor="agent-sandbox"
    >
      <h3>Agent Sandbox</h3>
      <p className="form-hint">
        Agent processes (provider CLIs, run_command, background tasks, MCP servers, the browser) run
        in a sandbox that blocks PeckBoard&apos;s data directory — its secrets, database and other
        sessions&apos; tokens — and leaves only project folders, temp dirs and toolchain caches
        writable. sudo is unavailable inside it. Changes apply to newly started agents.
      </p>
      {unsandboxed && (
        <p className="form-error" role="alert" data-testid="agent-sandbox-banner">
          {state.status.supported
            ? 'The agent sandbox is not enforced: agents can read PeckBoard secrets on this host.'
            : `Agents can read PeckBoard secrets on this host: the agent sandbox is unavailable on ${state.status.platform} (needs Linux with Landlock).`}
        </p>
      )}
      {state && (
        <>
          <label className="form-label" htmlFor="agent-sandbox-mode">
            Mode
          </label>
          <select
            id="agent-sandbox-mode"
            className="form-input"
            value={state.mode}
            onChange={(e) => pickMode(e.target.value as SandboxMode)}
            data-testid="agent-sandbox-mode"
          >
            {(Object.keys(MODE_LABELS) as SandboxMode[]).map((m) => (
              <option key={m} value={m}>
                {MODE_LABELS[m]}
              </option>
            ))}
          </select>
          <p className="form-hint" data-testid="agent-sandbox-status">
            {state.status.enforced
              ? `Enforced (Landlock ABI ${state.status.abi}).`
              : 'New agents start unsandboxed.'}
          </p>
          <label className="form-label" htmlFor="agent-sandbox-extra">
            Extra paths agents may write (one absolute path per line)
          </label>
          <textarea
            id="agent-sandbox-extra"
            className="form-input"
            rows={3}
            value={extraText}
            onChange={(e) => setExtraText(e.target.value)}
            data-testid="agent-sandbox-extra"
          />
          <div className="form-actions">
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={() => void save(state.mode, extraPaths())}
              data-testid="agent-sandbox-save-paths"
            >
              Save paths
            </button>
          </div>
        </>
      )}
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {confirmMode && state && (
        <ConfirmDialog
          testId="agent-sandbox-confirm"
          danger
          title="Run agents without the sandbox?"
          message="Newly started agents will be able to read PeckBoard's secrets (the JWT signing key, vault keys, the database and other sessions' tokens) and write to its data directory. Only do this to debug a tool the sandbox breaks."
          confirmLabel={`Switch to ${MODE_LABELS[confirmMode]}`}
          cancelLabel="Keep enforcing"
          onConfirm={() => {
            const mode = confirmMode
            setConfirmMode(null)
            void save(mode, state.extra_rw)
          }}
          onCancel={() => setConfirmMode(null)}
        />
      )}
    </section>
  )
}
