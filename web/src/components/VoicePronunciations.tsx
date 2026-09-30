import { useCallback, useEffect, useMemo, useState, type FormEvent } from 'react'
import { authedFetch, useAuthStore } from '../store/auth'
import { useVoiceStore } from '../store/voice'
import {
  DEFAULT_KOKORO_VOICE,
  KOKORO_PREFIX,
  isKokoroVoice,
  preparePreviewPlayback,
} from '../voice/kokoro'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import List from './List'
import Modal from './Modal'
import type { MenuItem } from './Dropdown'

/** One server-side Kokoro lexicon entry (`GET /api/voice/lexicon`). */
interface LexiconEntry {
  word: string
  display: string
  respelling: string | null
  phonemes: string
  source: 'default' | 'user'
  updated_at: string
}

/** A word the TTS had to guess (`GET /api/voice/lexicon/unknown`). */
interface UnknownWord {
  word: string
  count: number
  first_seen: string
  last_seen: string
}

type Mode = 'respelling' | 'phonemes'

/** What the add/edit dialog opens with. `editing` locks the word — it is
 *  the entry's key. */
interface Draft {
  word: string
  mode: Mode
  text: string
  editing: boolean
}

const ADMIN_ONLY_REASON = 'Only admins can change pronunciations.'
/** Show the filter box once the list stops fitting at a glance. */
const FILTER_THRESHOLD = 8
const PREVIEW_DEBOUNCE_MS = 350

const lexPath = (word: string) => `/api/voice/lexicon/${encodeURIComponent(word)}`

/** The server's `{error}` body, else a status line; 403 reads as the
 *  admin-only rule rather than a bare "Forbidden". */
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

const RESERVED_WORD_ERROR = '“unknown” and “preview” are reserved and can’t be saved.'

/** The server keys entries by one word (possessives and plurals follow
 *  automatically), so catch the shape before the round trip. */
function checkWord(raw: string): string | null {
  const w = raw.trim()
  if (!w) return null
  if (!/^[\p{L}\p{N}]+$/u.test(w)) return 'One word, letters and digits only.'
  if (/^(unknown|preview)$/i.test(w)) return RESERVED_WORD_ERROR
  return null
}

/** Synthesize with Kokoro and play the WAV. Playback is primed before the
 *  fetch so the click's user activation still counts. */
async function playTts(body: { text: string; voice: string; phonemes?: string; speed?: number }) {
  const play = preparePreviewPlayback()
  const res = await authedFetch('/api/voice/tts', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  if (res.status === 503)
    throw new Error('The natural voice is still downloading — try again shortly.')
  if (!res.ok) throw new Error(await errorMessage(res))
  await play(await res.arrayBuffer())
}

/** The Kokoro voice id Play uses: the selected voice when it is a Kokoro
 *  one, else the default — pronunciations only apply to Kokoro. */
function useKokoroVoiceId(): string {
  const voiceURI = useVoiceStore((s) => s.prefs.voiceURI)
  return (isKokoroVoice(voiceURI) ? voiceURI : DEFAULT_KOKORO_VOICE).slice(KOKORO_PREFIX.length)
}

function formatSeen(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}

function PronunciationModal({
  draft,
  onClose,
  onSaved,
}: {
  draft: Draft
  onClose: () => void
  onSaved: () => void
}) {
  const voice = useKokoroVoiceId()
  const rate = useVoiceStore((s) => s.prefs.rate)
  const [word, setWord] = useState(draft.word)
  const [mode, setMode] = useState<Mode>(draft.mode)
  const [text, setText] = useState(draft.text)
  // The last preview result, keyed by the input it was computed for: a
  // result for a stale input just reads as "still checking".
  const [result, setResult] = useState<{
    key: string
    phonemes: string | null
    error: string | null
  }>({ key: '', phonemes: null, error: null })
  const [formError, setFormError] = useState('')
  const [playError, setPlayError] = useState('')
  const [busy, setBusy] = useState(false)

  const value = text.trim()
  const inputKey = `${mode}|${value}`
  const current = value && result.key === inputKey ? result : null
  const previewing = !!value && !current
  const preview = current ?? { phonemes: null, error: null }

  // Debounced server-side preview: the server owns the respelling → IPA
  // rules and the phoneme validation, so what's shown is what gets saved.
  useEffect(() => {
    if (!value) return
    const key = `${mode}|${value}`
    const ctrl = new AbortController()
    const timer = setTimeout(() => {
      authedFetch('/api/voice/lexicon/preview', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ [mode]: value }),
        signal: ctrl.signal,
      })
        .then(async (res) => {
          if (res.ok) {
            const body = (await res.json()) as { phonemes: string }
            setResult({ key, phonemes: body.phonemes, error: null })
          } else {
            setResult({ key, phonemes: null, error: await errorMessage(res) })
          }
        })
        .catch((e: unknown) => {
          if (e instanceof DOMException && e.name === 'AbortError') return
          setResult({ key, phonemes: null, error: 'Could not check the pronunciation.' })
        })
    }, PREVIEW_DEBOUNCE_MS)
    return () => {
      clearTimeout(timer)
      ctrl.abort()
    }
  }, [mode, value])

  const [wordServerError, setWordServerError] = useState('')
  const wordError = checkWord(word) ?? wordServerError

  const disabledReason = !word.trim()
    ? 'Enter the word.'
    : wordError
      ? 'Fix the word first.'
      : !value
        ? mode === 'respelling'
          ? 'Enter a respelling.'
          : 'Enter the phonemes.'
        : previewing
          ? 'Checking the pronunciation…'
          : preview.error
            ? 'Fix the pronunciation first.'
            : null

  const play = () => {
    if (!preview.phonemes || !word.trim() || wordError) return
    setPlayError('')
    playTts({ text: word.trim(), phonemes: preview.phonemes, voice, speed: rate }).catch(
      (e: unknown) => setPlayError(e instanceof Error ? e.message : 'Playback failed.'),
    )
  }

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (disabledReason || busy) return
    setBusy(true)
    setFormError('')
    try {
      const res = await authedFetch(lexPath(word.trim()), {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ display: word.trim(), [mode]: value }),
      })
      if (res.ok) {
        onSaved()
        return
      }
      const message = await errorMessage(res)
      // 405: the path collides with a fixed route (`unknown`, `preview`).
      // A 400 naming the word is about the word; any other 400 is about
      // the pronunciation; anything else is form-level.
      if (res.status === 405) setWordServerError(RESERVED_WORD_ERROR)
      else if (res.status === 400 && /\bword\b|display/i.test(message)) setWordServerError(message)
      else if (res.status === 400) setResult({ key: inputKey, phonemes: null, error: message })
      else setFormError(message)
    } catch {
      setFormError('Could not save the pronunciation.')
    }
    setBusy(false)
  }

  return (
    <Modal onClose={onClose} maxWidth={520} data-testid="voice-lexicon-modal">
      <h2>{draft.editing ? 'Edit Pronunciation' : 'Add Pronunciation'}</h2>
      <form onSubmit={handleSubmit}>
        <div className="form-field">
          <label className="form-label" htmlFor="voice-lexicon-word">
            Word
          </label>
          <input
            id="voice-lexicon-word"
            className="form-input"
            type="text"
            value={word}
            autoComplete="off"
            placeholder="e.g. Peckboard"
            disabled={draft.editing}
            autoFocus={!draft.editing && !draft.word}
            onChange={(e) => {
              setWord(e.target.value)
              setWordServerError('')
            }}
            data-testid="voice-lexicon-word"
          />
          <FieldError message={wordError} testId="voice-lexicon-word-error" />
        </div>
        <div className="form-field">
          <span className="form-label" id="voice-lexicon-mode-label">
            Pronunciation
          </span>
          <div className="theme-toggle" role="group" aria-labelledby="voice-lexicon-mode-label">
            {(['respelling', 'phonemes'] as Mode[]).map((m) => (
              <button
                key={m}
                type="button"
                className={`theme-btn ${mode === m ? 'active' : ''}`}
                aria-pressed={mode === m}
                onClick={() => setMode(m)}
                data-testid={`voice-lexicon-mode-${m}`}
              >
                {m === 'respelling' ? 'Respelling' : 'Phonemes'}
              </button>
            ))}
          </div>
          <input
            id="voice-lexicon-text"
            className="form-input voice-lexicon-mono"
            type="text"
            value={text}
            autoComplete="off"
            spellCheck={false}
            aria-labelledby="voice-lexicon-mode-label"
            placeholder={mode === 'respelling' ? 'PECK-board' : 'pˈɛkbɔːɹd'}
            autoFocus={draft.editing || !!draft.word}
            onChange={(e) => setText(e.target.value)}
            data-testid="voice-lexicon-text"
          />
          <span className="form-hint">
            {mode === 'respelling'
              ? 'Spell it how it sounds, syllables split by hyphens. The ALL-CAPS syllable takes the main stress: PECK-board, koh-KOH-roh.'
              : 'Raw IPA as Kokoro reads it, with ˈ before the stressed syllable.'}
          </span>
          <FieldError message={preview.error ?? undefined} testId="voice-lexicon-text-error" />
          {!preview.error && text.trim() && (
            <div className="voice-lexicon-preview">
              <span className="form-hint">Phonemes:</span>{' '}
              <code className="voice-lexicon-mono" data-testid="voice-lexicon-preview">
                {previewing || !preview.phonemes ? '…' : preview.phonemes}
              </code>
              <button
                type="button"
                className="btn-secondary btn-sm"
                onClick={play}
                disabled={previewing || !preview.phonemes || !word.trim() || !!wordError}
                data-testid="voice-lexicon-draft-play"
              >
                Play
              </button>
            </div>
          )}
          {playError && (
            <p className="form-error" role="alert" data-testid="voice-lexicon-play-error">
              {playError}
            </p>
          )}
        </div>
        {formError && (
          <p className="form-error" role="alert" data-testid="voice-lexicon-form-error">
            {formError}
          </p>
        )}
        <div className="form-actions">
          {!busy && disabledReason && (
            <span className="form-actions-reason" data-testid="voice-lexicon-disabled-reason">
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
            data-testid="voice-lexicon-save"
          >
            {busy ? 'Saving…' : 'Save'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/**
 * Settings → Voice → Pronunciations: the server-side Kokoro lexicon.
 * Edits apply to the next thing spoken — no restart. The "Unknown words"
 * list is what the TTS had to guess recently, each one a candidate entry.
 */
export default function VoicePronunciations() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const voiceURI = useVoiceStore((s) => s.prefs.voiceURI)
  const rate = useVoiceStore((s) => s.prefs.rate)
  const voice = useKokoroVoiceId()

  const [entries, setEntries] = useState<LexiconEntry[]>([])
  const [unknown, setUnknown] = useState<UnknownWord[]>([])
  const [loaded, setLoaded] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [query, setQuery] = useState('')
  const [draft, setDraft] = useState<Draft | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<LexiconEntry | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState(false)
  // Bumped to refetch both lists (after a save, delete, or Retry).
  const [version, setVersion] = useState(0)
  const refresh = useCallback(() => setVersion((v) => v + 1), [])

  // Promise-chain shape: an effect body that awaits and then setStates
  // trips react-hooks/set-state-in-effect.
  useEffect(() => {
    let cancelled = false
    Promise.all([authedFetch('/api/voice/lexicon'), authedFetch('/api/voice/lexicon/unknown')])
      .then(async ([lexRes, unkRes]) => {
        if (!lexRes.ok) throw new Error(await errorMessage(lexRes))
        const lex = (await lexRes.json()) as LexiconEntry[]
        const unk = unkRes.ok ? ((await unkRes.json()) as UnknownWord[]) : []
        if (cancelled) return
        setEntries(lex)
        setUnknown(unk)
        setLoadError(null)
        setLoaded(true)
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setLoadError(e instanceof Error ? e.message : 'Could not load pronunciations.')
        setLoaded(true)
      })
    return () => {
      cancelled = true
    }
  }, [version])

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return entries
    return entries.filter(
      (e) => e.display.toLowerCase().includes(q) || e.word.toLowerCase().includes(q),
    )
  }, [entries, query])

  const play = (e: LexiconEntry) => {
    setActionError(null)
    playTts({ text: e.display || e.word, voice, speed: rate }).catch((err: unknown) =>
      setActionError(err instanceof Error ? err.message : 'Playback failed.'),
    )
  }

  const edit = (e: LexiconEntry) =>
    setDraft({
      word: e.display || e.word,
      mode: e.respelling ? 'respelling' : 'phonemes',
      text: e.respelling ?? e.phonemes,
      editing: true,
    })

  const add = (word = '') => setDraft({ word, mode: 'respelling', text: '', editing: false })

  const dismiss = async (u: UnknownWord) => {
    setActionError(null)
    try {
      const res = await authedFetch(`/api/voice/lexicon/unknown/${encodeURIComponent(u.word)}`, {
        method: 'DELETE',
      })
      if (!res.ok) throw new Error(await errorMessage(res))
      setUnknown((list) => list.filter((x) => x.word !== u.word))
    } catch (e) {
      setActionError(e instanceof Error ? e.message : 'Could not dismiss the word.')
    }
  }

  const doDelete = async () => {
    if (!confirmDelete) return
    setDeleting(true)
    setDeleteError(null)
    try {
      const res = await authedFetch(lexPath(confirmDelete.word), { method: 'DELETE' })
      if (!res.ok) throw new Error(await errorMessage(res))
      setConfirmDelete(null)
      refresh()
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : 'Could not delete the pronunciation.')
    } finally {
      setDeleting(false)
    }
  }

  const adminItem = (item: MenuItem): MenuItem =>
    isAdmin ? item : { ...item, disabled: true, hint: 'Admins only' }

  const entryMenu = (e: LexiconEntry): MenuItem[] => [
    { label: 'Play', onSelect: () => play(e), testId: `voice-lexicon-play-${e.word}` },
    adminItem({ label: 'Edit', onSelect: () => edit(e), testId: `voice-lexicon-edit-${e.word}` }),
    { divider: true },
    adminItem({
      label: 'Delete',
      danger: true,
      onSelect: () => {
        setDeleteError(null)
        setConfirmDelete(e)
      },
      testId: `voice-lexicon-delete-${e.word}`,
    }),
  ]

  const unknownMenu = (u: UnknownWord): MenuItem[] => [
    adminItem({
      label: 'Add pronunciation',
      onSelect: () => add(u.word),
      testId: `voice-unknown-add-${u.word}`,
    }),
    adminItem({
      label: 'Dismiss',
      onSelect: () => void dismiss(u),
      testId: `voice-unknown-dismiss-${u.word}`,
    }),
  ]

  return (
    <section
      className="settings-section"
      data-testid="voice-pronunciations-section"
      data-settings-anchor="voice-pronunciations"
    >
      <div className="settings-section-head">
        <h3>Pronunciations</h3>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => add()}
          disabled={!isAdmin}
          data-testid="voice-lexicon-add"
        >
          + Add word
        </button>
      </div>
      <p className="form-hint">
        How the natural voice says names and jargon. Changes apply to the next thing spoken. Plurals
        and possessives follow automatically; British voices use the same entries.
      </p>
      {!isKokoroVoice(voiceURI) && (
        <p className="form-hint" data-testid="voice-lexicon-browser-note">
          Pronunciations apply to the Kokoro (natural) voices only — your selected browser voice
          ignores them. Play previews with a Kokoro voice.
        </p>
      )}
      {!isAdmin && (
        <span className="form-actions-reason" data-testid="voice-lexicon-readonly-reason">
          {ADMIN_ONLY_REASON}
        </span>
      )}
      {loadError && (
        <div className="form-error" role="alert" data-testid="voice-lexicon-load-error">
          <span>{loadError}</span>{' '}
          <button type="button" className="btn-secondary" onClick={refresh}>
            Retry
          </button>
        </div>
      )}
      {actionError && (
        <p className="form-error" role="alert" data-testid="voice-lexicon-action-error">
          {actionError}
        </p>
      )}

      {entries.length > FILTER_THRESHOLD && (
        <input
          type="search"
          className="form-input voice-lexicon-filter"
          placeholder="Filter words…"
          aria-label="Filter pronunciations"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          data-testid="voice-lexicon-filter"
        />
      )}
      <List<LexiconEntry>
        items={filtered}
        getKey={(e) => e.word}
        bodyClassName="list-view-rows"
        onActivate={(e) => (isAdmin ? edit(e) : play(e))}
        getMenuItems={entryMenu}
        renderItem={(e) => (
          <>
            <span className="list-view-name" data-testid={`voice-lexicon-row-${e.word}`}>
              {e.display || e.word}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag" data-testid={`voice-lexicon-tag-${e.word}`}>
                {e.source === 'default' ? 'default' : 'custom'}
              </span>
              <code className="voice-lexicon-mono" data-testid={`voice-lexicon-pron-${e.word}`}>
                {e.respelling ?? e.phonemes}
              </code>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="voice-lexicon-empty">
            {loadError
              ? 'Pronunciations could not be loaded.'
              : !loaded
                ? 'Loading…'
                : query
                  ? 'No words match.'
                  : 'No pronunciations yet.'}
          </div>
        }
      />

      <h4 className="voice-lexicon-subhead">Unknown Words</h4>
      <p className="form-hint">
        Words the voice recently had to guess. Add a pronunciation, or dismiss the ones it gets
        right.
      </p>
      <List<UnknownWord>
        items={unknown}
        getKey={(u) => u.word}
        bodyClassName="list-view-rows"
        onActivate={(u) => {
          if (isAdmin) add(u.word)
        }}
        getMenuItems={unknownMenu}
        renderItem={(u) => (
          <>
            <span className="list-view-name" data-testid={`voice-unknown-row-${u.word}`}>
              {u.word}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag">{u.count}× heard</span>
              <span>last {formatSeen(u.last_seen)}</span>
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="voice-unknown-empty">
            {loaded ? 'No unknown words.' : 'Loading…'}
          </div>
        }
      />

      {draft && (
        <PronunciationModal
          draft={draft}
          onClose={() => setDraft(null)}
          onSaved={() => {
            setDraft(null)
            refresh()
          }}
        />
      )}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete Pronunciation"
          message={
            confirmDelete.source === 'default'
              ? `Remove the built-in pronunciation of “${confirmDelete.display}”? The voice will guess it again.`
              : `Delete the pronunciation of “${confirmDelete.display}”? The voice will guess it again.`
          }
          confirmLabel="Delete"
          danger
          busy={deleting}
          busyLabel="Deleting…"
          error={deleteError}
          testId="voice-lexicon-delete-confirm"
          onConfirm={() => void doDelete()}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
    </section>
  )
}
