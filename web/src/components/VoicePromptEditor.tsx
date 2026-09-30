import { useCallback, useEffect, useState } from 'react'
import { authedFetch, useAuthStore } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'
import List from './List'
import Modal from './Modal'

/** `GET /api/voice/prompt`. */
interface ActivePrompt {
  content: string
  source: PromptSource
  is_default: boolean
  updated_at: string | null
  default_content: string
}

type PromptSource = 'default' | 'user' | 'assistant'

interface DiffStats {
  added: number
  removed: number
}

/** `GET /api/voice/prompt/history` row. */
interface HistoryEntry {
  id: string
  source: PromptSource
  note: string | null
  created_at: string
  created_by: string | null
  diff_stats: DiffStats
}

/** `GET /api/voice/prompt/history/{id}`. */
interface VersionDetail extends HistoryEntry {
  content: string
  diff: string
}

const ADMIN_ONLY_REASON = 'Only admins can change the assistant prompt.'

const SOURCE_LABEL: Record<PromptSource, string> = {
  default: 'default',
  user: 'you',
  assistant: 'assistant',
}

async function errorMessage(res: Response): Promise<string> {
  if (res.status === 403) return ADMIN_ONLY_REASON
  try {
    const body = (await res.json()) as { error?: string }
    if (body?.error) return body.error
  } catch {
    /* not JSON */
  }
  return `Request failed (HTTP ${res.status})`
}

function formatWhen(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}

/** Unified diff with the shared `.diff-line-*` coloring. */
function DiffView({ diff }: { diff: string }) {
  const lines = diff ? diff.replace(/\n$/, '').split('\n') : []
  if (lines.length === 0) return <p className="form-hint">No changes.</p>
  return (
    <pre className="diff-body voice-prompt-diff" data-testid="voice-prompt-diff">
      {lines.map((l, i) => {
        const cls =
          l.startsWith('+') && !l.startsWith('+++')
            ? 'diff-line-add'
            : l.startsWith('-') && !l.startsWith('---')
              ? 'diff-line-del'
              : l.startsWith('@@')
                ? 'diff-line-hunk'
                : 'diff-line'
        return (
          <div key={i} className={cls}>
            {l || ' '}
          </div>
        )
      })}
    </pre>
  )
}

/**
 * Settings → Voice → Assistant Prompt: the voice assistant's system prompt,
 * editable live. A save applies from the assistant's next turn. The history
 * lists every change (yours, the assistant's own, resets); open one to see
 * its diff and restore it.
 */
export default function VoicePromptEditor() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const [active, setActive] = useState<ActivePrompt | null>(null)
  const [history, setHistory] = useState<HistoryEntry[]>([])
  const [draft, setDraft] = useState('')
  const [loadError, setLoadError] = useState<string | null>(null)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [savedNote, setSavedNote] = useState<string | null>(null)
  const [confirmReset, setConfirmReset] = useState(false)
  const [resetError, setResetError] = useState<string | null>(null)
  const [resetting, setResetting] = useState(false)
  const [viewing, setViewing] = useState<VersionDetail | null>(null)
  const [viewError, setViewError] = useState<string | null>(null)
  const [restoring, setRestoring] = useState(false)
  const [version, setVersion] = useState(0)
  const refresh = useCallback(() => setVersion((v) => v + 1), [])

  useEffect(() => {
    let cancelled = false
    Promise.all([authedFetch('/api/voice/prompt'), authedFetch('/api/voice/prompt/history')])
      .then(async ([curRes, histRes]) => {
        if (!curRes.ok) throw new Error(await errorMessage(curRes))
        const cur = (await curRes.json()) as ActivePrompt
        const hist = histRes.ok ? ((await histRes.json()) as HistoryEntry[]) : []
        if (cancelled) return
        setActive(cur)
        setDraft(cur.content)
        setHistory(hist)
        setLoadError(null)
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setLoadError(e instanceof Error ? e.message : 'Could not load the assistant prompt.')
      })
    return () => {
      cancelled = true
    }
  }, [version])

  const unchanged = active !== null && draft.trimEnd() === active.content.trimEnd()
  const disabledReason = !isAdmin
    ? ADMIN_ONLY_REASON
    : active === null
      ? 'Loading…'
      : !draft.trim()
        ? 'The prompt can’t be empty.'
        : unchanged
          ? 'No changes to save.'
          : null

  /** PUT `content`; the error message, or null on success. */
  const put = async (content: string, note?: string): Promise<string | null> => {
    const res = await authedFetch('/api/voice/prompt', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ content, note }),
    })
    return res.ok ? null : await errorMessage(res)
  }

  const save = async () => {
    setSaving(true)
    setSaveError(null)
    setSavedNote(null)
    try {
      const error = await put(draft)
      if (error) {
        setSaveError(error)
        return
      }
      setSavedNote('Saved. The assistant uses it from its next turn.')
      refresh()
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : 'Could not save the prompt.')
    } finally {
      setSaving(false)
    }
  }

  const doReset = async () => {
    setResetting(true)
    setResetError(null)
    try {
      const res = await authedFetch('/api/voice/prompt/reset', { method: 'POST' })
      if (!res.ok) throw new Error(await errorMessage(res))
      setConfirmReset(false)
      setViewing(null)
      setSavedNote('Reset to the built-in default.')
      refresh()
    } catch (e) {
      setResetError(e instanceof Error ? e.message : 'Could not reset the prompt.')
    } finally {
      setResetting(false)
    }
  }

  const open = async (entry: HistoryEntry) => {
    setViewError(null)
    try {
      const res = await authedFetch(`/api/voice/prompt/history/${encodeURIComponent(entry.id)}`)
      if (!res.ok) throw new Error(await errorMessage(res))
      setViewing((await res.json()) as VersionDetail)
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : 'Could not load that version.')
    }
  }

  const restore = async () => {
    if (!viewing) return
    // A reset row means "the built-in default", which keeps tracking
    // future built-in updates — restore it as a reset, not a frozen copy.
    if (viewing.source === 'default') {
      await doReset()
      return
    }
    setRestoring(true)
    setViewError(null)
    try {
      const error = await put(
        viewing.content,
        `Restored version from ${formatWhen(viewing.created_at)}`,
      )
      if (error) {
        setViewError(error)
        return
      }
      setViewing(null)
      setSavedNote('Version restored. The assistant uses it from its next turn.')
      refresh()
    } catch (e) {
      setViewError(e instanceof Error ? e.message : 'Could not restore that version.')
    } finally {
      setRestoring(false)
    }
  }

  return (
    <section
      className="settings-section"
      data-testid="voice-prompt-section"
      data-settings-anchor="voice-prompt"
    >
      <div className="settings-section-head">
        <h3>Assistant Prompt</h3>
        {active && (
          <span
            className="list-view-tag"
            data-testid="voice-prompt-status"
            title={
              active.updated_at ? `Last changed ${formatWhen(active.updated_at)}` : 'Never changed'
            }
          >
            {active.is_default ? 'default' : 'modified'}
          </span>
        )}
      </div>
      <p className="form-hint">
        The voice assistant&apos;s instructions. Changes apply from its next turn — no restart. The
        assistant can also change this itself when you ask it to behave differently.
      </p>
      {loadError && (
        <div className="form-error" role="alert" data-testid="voice-prompt-load-error">
          <span>{loadError}</span>{' '}
          <button type="button" className="btn-secondary" onClick={refresh}>
            Retry
          </button>
        </div>
      )}
      <textarea
        className="form-input voice-lexicon-mono voice-prompt-textarea"
        aria-label="Assistant prompt"
        spellCheck={false}
        value={draft}
        readOnly={!isAdmin}
        onChange={(e) => {
          setDraft(e.target.value)
          setSavedNote(null)
        }}
        data-testid="voice-prompt-textarea"
      />
      {saveError && (
        <p className="form-error" role="alert" data-testid="voice-prompt-error">
          {saveError}
        </p>
      )}
      {savedNote && (
        <p className="form-hint" role="status" data-testid="voice-prompt-saved">
          {savedNote}
        </p>
      )}
      <div className="form-actions">
        {!saving && disabledReason && (
          <span className="form-actions-reason" data-testid="voice-prompt-disabled-reason">
            {disabledReason}
          </span>
        )}
        <button
          type="button"
          className="btn-secondary"
          onClick={() => {
            setResetError(null)
            setConfirmReset(true)
          }}
          disabled={!isAdmin || !active || active.is_default}
          title={active?.is_default ? 'Already the built-in default' : undefined}
          data-testid="voice-prompt-reset"
        >
          Reset to default
        </button>
        <button
          type="button"
          className="btn-primary"
          onClick={() => void save()}
          disabled={saving || !!disabledReason}
          data-testid="voice-prompt-save"
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>

      <h4 className="voice-lexicon-subhead">History</h4>
      <List<HistoryEntry>
        items={history}
        getKey={(h) => h.id}
        bodyClassName="list-view-rows"
        onActivate={(h) => void open(h)}
        renderItem={(h) => (
          <>
            <span className="list-view-name" data-testid={`voice-prompt-history-${h.id}`}>
              {h.note || (h.source === 'default' ? 'Reset to default' : 'Edited')}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag" data-testid="voice-prompt-history-source">
                {SOURCE_LABEL[h.source]}
              </span>
              {h.source !== 'default' && (
                <span>
                  <span className="diff-added">+{h.diff_stats.added}</span>{' '}
                  <span className="diff-removed">&minus;{h.diff_stats.removed}</span>
                </span>
              )}
              <span>{formatWhen(h.created_at)}</span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="voice-prompt-history-empty">
            {active ? 'No changes yet — the built-in default is in use.' : 'Loading…'}
          </div>
        }
      />

      {viewing && (
        <Modal onClose={() => setViewing(null)} maxWidth={760} data-testid="voice-prompt-version">
          <h2>Prompt Version</h2>
          <p className="form-hint">
            {formatWhen(viewing.created_at)} · {SOURCE_LABEL[viewing.source]}
            {viewing.note ? ` · ${viewing.note}` : ''}
          </p>
          <DiffView diff={viewing.diff} />
          {viewError && (
            <p className="form-error" role="alert" data-testid="voice-prompt-version-error">
              {viewError}
            </p>
          )}
          <div className="form-actions">
            {!isAdmin && <span className="form-actions-reason">{ADMIN_ONLY_REASON}</span>}
            <button type="button" className="btn-secondary" onClick={() => setViewing(null)}>
              Close
            </button>
            <button
              type="button"
              className="btn-primary"
              onClick={() => void restore()}
              disabled={!isAdmin || restoring || resetting}
              data-testid="voice-prompt-restore"
            >
              {restoring ? 'Restoring…' : 'Restore this version'}
            </button>
          </div>
        </Modal>
      )}
      {confirmReset && (
        <ConfirmDialog
          title="Reset Assistant Prompt"
          message="Replace your edited prompt with the built-in default? Your version stays in the history, and the default keeps updating with new releases."
          confirmLabel="Reset"
          danger
          busy={resetting}
          busyLabel="Resetting…"
          error={resetError}
          testId="voice-prompt-reset-confirm"
          onConfirm={() => void doReset()}
          onCancel={() => setConfirmReset(false)}
        />
      )}
    </section>
  )
}
