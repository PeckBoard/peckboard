import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import {
  ASSISTANT_SETTING_RANGES,
  useAssistantSettings,
  type AssistantSettings,
} from '../store/assistantSettings'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import List from './List'
import type { MenuItem } from './Dropdown'

/** One entry of `GET /api/voice/memory`. */
interface MemoryEntry {
  id: string
  topic_title: string
  kind: 'did' | 'learned' | 'pending'
  summary: string
  source: 'assistant' | 'fallback'
  created_at: string
  expires_at: string
}

/** `GET /api/voice/memory/status`. */
interface MemoryStatus {
  pending: { due_at: string; reason: 'close' | 'idle'; topics: string[] } | null
  last_clear: { at: string; reason: 'close' | 'idle'; topic_title: string; kept: number } | null
}

type NumberField = keyof typeof ASSISTANT_SETTING_RANGES

const NUMBER_FIELDS: { key: NumberField; label: string; unit: string; testId: string }[] = [
  {
    key: 'retention_hours',
    label: 'Keep memories for',
    unit: 'hours',
    testId: 'voice-memory-retention',
  },
  { key: 'max_entries', label: 'Keep at most', unit: 'entries', testId: 'voice-memory-max' },
  {
    key: 'clear_delay_minutes',
    label: 'Clear the conversation',
    unit: 'minutes after a topic ends',
    testId: 'voice-memory-delay',
  },
  {
    key: 'idle_topic_end_minutes',
    label: 'A topic ends after',
    unit: 'quiet minutes',
    testId: 'voice-memory-idle',
  },
]

const KIND_LABEL: Record<MemoryEntry['kind'], string> = {
  did: 'did',
  learned: 'learned',
  pending: 'pending',
}

const POLL_MS = 5000

async function errorOf(res: Response, fallback: string): Promise<string> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  return body?.error ?? `${fallback} (${res.status})`
}

/** "in 35h", "in 4m", "in 20s", "now". */
function until(iso: string, now: number): string {
  const ms = new Date(iso).getTime() - now
  if (Number.isNaN(ms)) return iso
  if (ms <= 0) return 'now'
  const s = Math.round(ms / 1000)
  if (s < 60) return `in ${s}s`
  const m = Math.round(s / 60)
  if (m < 60) return `in ${m}m`
  const h = Math.floor(m / 60)
  return h < 48 ? `in ${h}h ${m % 60}m` : `in ${Math.floor(h / 24)}d`
}

function ago(iso: string, now: number): string {
  const ms = now - new Date(iso).getTime()
  if (Number.isNaN(ms)) return iso
  const m = Math.floor(ms / 60000)
  if (m < 1) return 'just now'
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  return h < 48 ? `${h}h ago` : `${Math.floor(h / 24)}d ago`
}

function rangeError(key: NumberField, raw: string): string | undefined {
  const [lo, hi] = ASSISTANT_SETTING_RANGES[key]
  if (!/^\d+$/.test(raw.trim())) return `Enter a whole number from ${lo} to ${hi}.`
  const n = Number(raw)
  if (n < lo || n > hi) return `Must be between ${lo} and ${hi}.`
  return undefined
}

/**
 * Settings → Assistant → Memory: the Assistant's temporary memory and the
 * automatic context clear after a topic ends (`service::assistant_memory`).
 * Settings, the current entries (with their expiry), and the clear status.
 */
export default function AssistantMemorySection() {
  const settings = useAssistantSettings((s) => s.settings)
  const settingsError = useAssistantSettings((s) => s.error)
  const loadSettings = useAssistantSettings((s) => s.load)
  const save = useAssistantSettings((s) => s.save)

  const [drafts, setDrafts] = useState<Partial<Record<NumberField, string>>>({})
  const [entries, setEntries] = useState<MemoryEntry[]>([])
  const [status, setStatus] = useState<MemoryStatus | null>(null)
  const [loaded, setLoaded] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [confirmClear, setConfirmClear] = useState(false)
  const [clearing, setClearing] = useState(false)
  const [clearError, setClearError] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())
  const [version, setVersion] = useState(0)
  const refresh = useCallback(() => setVersion((v) => v + 1), [])

  useEffect(() => {
    if (!settings) void loadSettings()
  }, [settings, loadSettings])

  // Promise-chain shape: an effect body that awaits and then setStates
  // trips react-hooks/set-state-in-effect.
  useEffect(() => {
    let cancelled = false
    const fetchAll = () =>
      Promise.all([authedFetch('/api/voice/memory'), authedFetch('/api/voice/memory/status')])
        .then(async ([memRes, stRes]) => {
          if (!memRes.ok) throw new Error(await errorOf(memRes, "Couldn't load memory"))
          const mem = (await memRes.json()) as { entries: MemoryEntry[] }
          const st = stRes.ok ? ((await stRes.json()) as MemoryStatus) : null
          if (cancelled) return
          setEntries(mem.entries)
          setStatus(st)
          setLoadError(null)
          setLoaded(true)
        })
        .catch((e: unknown) => {
          if (cancelled) return
          setLoadError(e instanceof Error ? e.message : String(e))
          setLoaded(true)
        })
    void fetchAll()
    const poll = window.setInterval(() => void fetchAll(), POLL_MS)
    const tick = window.setInterval(() => setNow(Date.now()), 1000)
    return () => {
      cancelled = true
      window.clearInterval(poll)
      window.clearInterval(tick)
    }
  }, [version])

  const fieldValue = (key: NumberField) => drafts[key] ?? (settings ? String(settings[key]) : '')

  const commit = async (key: NumberField) => {
    const raw = drafts[key]
    if (raw === undefined || !settings || rangeError(key, raw)) return
    if (Number(raw) === settings[key]) {
      setDrafts((d) => ({ ...d, [key]: undefined }))
      return
    }
    if (await save({ [key]: Number(raw) } as Partial<AssistantSettings>)) {
      setDrafts((d) => ({ ...d, [key]: undefined }))
    }
  }

  const deleteEntry = async (e: MemoryEntry) => {
    setActionError(null)
    try {
      const res = await authedFetch(`/api/voice/memory/${encodeURIComponent(e.id)}`, {
        method: 'DELETE',
      })
      if (!res.ok && res.status !== 404) throw new Error(await errorOf(res, "Couldn't delete"))
      setEntries((list) => list.filter((x) => x.id !== e.id))
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err))
    }
  }

  const clearAll = async () => {
    setClearing(true)
    setClearError(null)
    try {
      const res = await authedFetch('/api/voice/memory', { method: 'DELETE' })
      if (!res.ok) throw new Error(await errorOf(res, "Couldn't clear"))
      setConfirmClear(false)
      refresh()
    } catch (e) {
      setClearError(e instanceof Error ? e.message : String(e))
    } finally {
      setClearing(false)
    }
  }

  const entryMenu = (e: MemoryEntry): MenuItem[] => [
    {
      label: 'Delete',
      danger: true,
      onSelect: () => void deleteEntry(e),
      testId: `voice-memory-delete-${e.id}`,
    },
  ]

  const disabled = settings === null

  return (
    <section
      className="settings-section"
      data-testid="voice-memory-section"
      data-settings-anchor="voice-memory"
    >
      <div className="settings-section-head">
        <h3>Memory</h3>
        <button
          type="button"
          className="btn-secondary btn-sm"
          disabled={entries.length === 0}
          onClick={() => {
            setClearError(null)
            setConfirmClear(true)
          }}
          data-testid="voice-memory-clear-all"
        >
          Clear all
        </button>
      </div>
      <p className="form-hint">
        When a topic ends, the Assistant notes what it did, learned, and left open, then starts a
        fresh conversation once things are quiet. Notes expire on their own and never hold
        transcripts or secrets.
      </p>

      <label className="settings-row settings-row-toggle">
        <input
          type="checkbox"
          data-testid="voice-memory-enabled"
          checked={settings?.memory_enabled ?? true}
          disabled={disabled}
          onChange={(e) => void save({ memory_enabled: e.target.checked })}
        />
        <span className="settings-label">Remember facts across topics</span>
      </label>

      <div className="form-field">
        <label className="form-label" htmlFor="voice-memory-auto-clear">
          Clear the conversation
        </label>
        <select
          id="voice-memory-auto-clear"
          className="form-input"
          data-testid="voice-memory-auto-clear"
          value={settings?.auto_clear ?? 'after_topic'}
          disabled={disabled}
          onChange={(e) =>
            void save({ auto_clear: e.target.value as AssistantSettings['auto_clear'] })
          }
        >
          <option value="after_topic">After a topic ends</option>
          <option value="off">Never (off)</option>
        </select>
      </div>

      {NUMBER_FIELDS.map((f) => {
        const raw = drafts[f.key]
        const err = raw === undefined ? undefined : rangeError(f.key, raw)
        const [lo, hi] = ASSISTANT_SETTING_RANGES[f.key]
        return (
          <div className="form-field" key={f.key}>
            <label className="form-label" htmlFor={f.testId}>
              {f.label}
            </label>
            <div className="settings-row">
              <input
                id={f.testId}
                type="number"
                inputMode="numeric"
                min={lo}
                max={hi}
                step={1}
                className="form-input voice-memory-number"
                data-testid={f.testId}
                value={fieldValue(f.key)}
                disabled={disabled}
                onChange={(e) => setDrafts((d) => ({ ...d, [f.key]: e.target.value }))}
                onBlur={() => void commit(f.key)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') void commit(f.key)
                }}
              />
              <span className="form-hint">{f.unit}</span>
            </div>
            <FieldError message={err} testId={`${f.testId}-error`} />
          </div>
        )
      })}
      {settingsError && (
        <p className="form-error" role="alert" data-testid="voice-memory-settings-error">
          {settingsError}
        </p>
      )}

      <div className="voice-memory-status" data-testid="voice-memory-status">
        {status?.pending ? (
          <p className="form-hint" data-testid="voice-memory-pending">
            Clearing the conversation after “{status.pending.topics.at(-1)}”{' '}
            {until(status.pending.due_at, now) === 'now'
              ? 'as soon as it is quiet'
              : until(status.pending.due_at, now)}
            .
          </p>
        ) : (
          <p className="form-hint" data-testid="voice-memory-pending-none">
            No clear pending.
          </p>
        )}
        {status?.last_clear && (
          <p className="form-hint" data-testid="voice-memory-last-clear">
            Last cleared {ago(status.last_clear.at, now)} after “{status.last_clear.topic_title}”
            {status.last_clear.reason === 'idle' ? ' (went quiet)' : ''} · {status.last_clear.kept}{' '}
            {status.last_clear.kept === 1 ? 'memory' : 'memories'} kept.
          </p>
        )}
      </div>

      {loadError && (
        <div className="form-error" role="alert" data-testid="voice-memory-load-error">
          <span>{loadError}</span>{' '}
          <button type="button" className="btn-secondary" onClick={refresh}>
            Retry
          </button>
        </div>
      )}
      {actionError && (
        <p className="form-error" role="alert" data-testid="voice-memory-action-error">
          {actionError}
        </p>
      )}

      <List<MemoryEntry>
        items={entries}
        getKey={(e) => e.id}
        bodyClassName="list-view-rows"
        onActivate={() => {}}
        getMenuItems={entryMenu}
        renderItem={(e) => (
          <>
            <span className="list-view-name" data-testid={`voice-memory-entry-${e.id}`}>
              {e.summary}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag">{KIND_LABEL[e.kind]}</span>
              <span>{e.topic_title}</span>
              {e.source === 'fallback' && <span className="list-view-tag">auto</span>}
              <span data-testid={`voice-memory-expiry-${e.id}`}>
                expires {until(e.expires_at, now)}
              </span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="voice-memory-empty">
            {loadError
              ? 'Memory could not be loaded.'
              : loaded
                ? 'Nothing remembered.'
                : 'Loading…'}
          </div>
        }
      />

      {confirmClear && (
        <ConfirmDialog
          title="Clear Memory"
          message={`Forget all ${entries.length} ${entries.length === 1 ? 'memory' : 'memories'}? The Assistant stops seeing them from its next turn; what was said in the current conversation stays until it is cleared.`}
          confirmLabel="Clear all"
          danger
          busy={clearing}
          busyLabel="Clearing…"
          error={clearError}
          testId="voice-memory-clear-confirm"
          onConfirm={() => void clearAll()}
          onCancel={() => setConfirmClear(false)}
        />
      )}
    </section>
  )
}
