import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { reloadServerVoices } from '../voice/kokoro'
import {
  setRecognitionSource,
  setSpeechServerConfigured,
  supportsServerRecognition,
  useSpeechRecognitionPrefs,
  type RecognitionSource,
} from '../voice/serverStt'
import FieldError from './FieldError'

interface ServerView {
  base_url: string
  model: string
  has_api_key: boolean
}

interface TestResult {
  ok: boolean
  voices?: number
  version?: string | null
  device?: string | null
  error?: string
}

async function errorText(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as { error?: string }
    if (body.error) return body.error
  } catch {
    // not JSON
  }
  return `HTTP ${res.status}`
}

/**
 * Settings → Assistant → Speech server: an external speech server
 * (VoiceStudio, or anything speaking OpenAI's audio API). Peckboard's
 * server talks to it, so the API key is write-only here — the page only
 * learns whether one is saved. Its voices join the Voice picker as
 * "Speech server — …".
 */
export default function SpeechServerSection() {
  const [view, setView] = useState<ServerView | null>(null)
  const [url, setUrl] = useState('')
  const [key, setKey] = useState('')
  const [loadError, setLoadError] = useState<string | null>(null)
  const [urlError, setUrlError] = useState<string | undefined>()
  const [busy, setBusy] = useState(false)
  const [saved, setSaved] = useState(false)
  const [test, setTest] = useState<TestResult | null>(null)
  const recognition = useSpeechRecognitionPrefs((s) => s.source)
  // Recognizing on the server needs a saved server, and a page that may
  // capture the microphone (a secure context).
  const canCapture = supportsServerRecognition()
  const recognitionLocked = !view?.base_url || !canCapture
  const recognitionReason =
    view === null
      ? null
      : !view.base_url
        ? 'Save a server address to recognize speech with it.'
        : !canCapture
          ? "This page can't stream the microphone (it needs HTTPS)."
          : null

  useEffect(() => {
    let live = true
    authedFetch('/api/voice/speech-server')
      .then(async (res) => {
        if (!res.ok) throw new Error(await errorText(res))
        const v = (await res.json()) as ServerView
        if (!live) return
        setView(v)
        setSpeechServerConfigured(v.base_url !== '')
        setUrl(v.base_url)
      })
      .catch((e: unknown) => live && setLoadError(e instanceof Error ? e.message : String(e)))
    return () => {
      live = false
    }
  }, [])

  const dirty = view !== null && (url.trim() !== view.base_url || key.trim() !== '')

  const save = async (body: Record<string, string>) => {
    setBusy(true)
    setUrlError(undefined)
    setSaved(false)
    setTest(null)
    try {
      const res = await authedFetch('/api/voice/speech-server', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      if (!res.ok) {
        setUrlError(await errorText(res))
        return false
      }
      const v = (await res.json()) as ServerView
      setView(v)
      setUrl(v.base_url)
      setSpeechServerConfigured(v.base_url !== '')
      setKey('')
      setSaved(true)
      reloadServerVoices()
      return true
    } finally {
      setBusy(false)
    }
  }

  const runTest = async () => {
    if (
      dirty &&
      !(await save({ base_url: url.trim(), ...(key.trim() ? { api_key: key.trim() } : {}) }))
    )
      return
    setBusy(true)
    setTest(null)
    try {
      const res = await authedFetch('/api/voice/speech-server/test', { method: 'POST' })
      setTest(
        res.ok ? ((await res.json()) as TestResult) : { ok: false, error: await errorText(res) },
      )
    } catch (e) {
      setTest({ ok: false, error: e instanceof Error ? e.message : String(e) })
    } finally {
      setBusy(false)
    }
  }

  const testLabel = (t: TestResult) => {
    if (!t.ok) return `Couldn't reach it: ${t.error ?? 'unknown error'}`
    const bits = [`${t.voices ?? 0} voice${t.voices === 1 ? '' : 's'}`]
    if (t.version) bits.push(`version ${t.version}`)
    if (t.device) bits.push(`on ${t.device}`)
    return `Connected — ${bits.join(', ')}.`
  }

  return (
    <section
      className="settings-section"
      data-testid="voice-speech-server-section"
      data-settings-anchor="voice-speech-server"
    >
      <h3>Speech Server</h3>
      <p className="form-hint">
        Use a VoiceStudio server (or any server with an OpenAI-compatible audio API) for more
        realistic voices, including cloned and designed ones. Install it from the App Manager, then
        paste its address and the API key from <code>~/.config/voicestudio/api-key</code> on that
        machine. Its voices appear in the Voice picker as &ldquo;Speech server&rdquo;. Real-time
        speech needs a GPU. It can also recognize what you say — for browsers without built-in
        speech recognition, such as the Android app.
      </p>
      {loadError ? (
        <p className="form-error" role="alert" data-testid="speech-server-load-error">
          {loadError}
        </p>
      ) : (
        <form
          className="settings-info-grid"
          onSubmit={(e) => {
            e.preventDefault()
            void save({ base_url: url.trim(), ...(key.trim() ? { api_key: key.trim() } : {}) })
          }}
        >
          <label className="settings-row voice-settings-row">
            <span className="settings-label">Address</span>
            <span className="speech-server-field">
              <input
                className="form-input"
                data-testid="speech-server-url"
                type="url"
                placeholder="http://gpu-box:3900"
                value={url}
                onChange={(e) => {
                  setUrl(e.target.value)
                  setSaved(false)
                }}
                disabled={view === null || busy}
                spellCheck={false}
                autoComplete="off"
              />
              <FieldError message={urlError} testId="speech-server-url-error" />
            </span>
          </label>
          <label className="settings-row voice-settings-row">
            <span className="settings-label">API key</span>
            <input
              className="form-input"
              data-testid="speech-server-key"
              type="password"
              placeholder={
                view?.has_api_key ? 'Saved — type to replace' : 'Optional on the same machine'
              }
              value={key}
              onChange={(e) => {
                setKey(e.target.value)
                setSaved(false)
              }}
              disabled={view === null || busy}
              autoComplete="new-password"
            />
          </label>
          <label className="settings-row voice-settings-row">
            <span className="settings-label">Speech recognition</span>
            <span className="speech-server-field">
              <select
                className="form-input"
                data-testid="speech-recognition-select"
                value={recognitionLocked ? 'browser' : recognition}
                onChange={(e) => setRecognitionSource(e.target.value as RecognitionSource)}
                disabled={recognitionLocked}
              >
                <option value="browser">Browser</option>
                <option value="server">Speech server</option>
              </select>
              {recognitionReason && (
                <span className="form-actions-reason" data-testid="speech-recognition-reason">
                  {recognitionReason}
                </span>
              )}
            </span>
          </label>
          <div className="voice-settings-actions">
            <button
              type="submit"
              className="btn-primary"
              data-testid="speech-server-save"
              disabled={!dirty || busy}
            >
              Save
            </button>
            <button
              type="button"
              className="btn-secondary"
              data-testid="speech-server-test"
              onClick={() => void runTest()}
              disabled={busy || !url.trim()}
            >
              Test connection
            </button>
            {view?.has_api_key && (
              <button
                type="button"
                className="btn-secondary"
                data-testid="speech-server-clear-key"
                onClick={() => void save({ api_key: '' })}
                disabled={busy}
              >
                Remove key
              </button>
            )}
            {!dirty && !busy && !url.trim() && (
              <span className="form-actions-reason">Enter the server address to test it.</span>
            )}
            {saved && !test && (
              <span className="form-hint" data-testid="speech-server-saved">
                Saved.
              </span>
            )}
          </div>
          {test && (
            <p
              className={test.ok ? 'form-hint' : 'form-error'}
              role={test.ok ? 'status' : 'alert'}
              data-testid="speech-server-test-result"
            >
              {testLabel(test)}
            </p>
          )}
        </form>
      )}
    </section>
  )
}
