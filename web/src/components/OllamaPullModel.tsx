import { useRef, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useResourcesStore } from '../store/resources'

/**
 * "Pull a model" widget for the Ollama settings section. Sends the
 * entered reference to `POST /api/ollama/pull` (which proxies Ollama's
 * streaming NDJSON progress) and renders a live progress bar. Accepts
 * anything `ollama pull` does: registry models (`llama3.2`,
 * `qwen2.5-coder:7b`) or Hugging Face GGUF repos
 * (`hf.co/<user>/<repo>[:quant]`), with an optional `@<server>` suffix
 * to target one of the named additional servers.
 *
 * On success the model picker is refreshed via `fetchModels` — the
 * provider's autodiscovery sees the new model immediately, no restart
 * or settings change needed.
 */

interface PullLine {
  status?: string
  error?: string
  total?: number
  completed?: number
}

export default function OllamaPullModel() {
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  const [model, setModel] = useState('')
  const [pulling, setPulling] = useState(false)
  const [status, setStatus] = useState<string | null>(null)
  const [percent, setPercent] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<string | null>(null)
  const abortRef = useRef<AbortController | null>(null)

  const startPull = async () => {
    const ref = model.trim()
    if (ref === '' || pulling) return
    setPulling(true)
    setError(null)
    setDone(null)
    setStatus('starting…')
    setPercent(null)
    const controller = new AbortController()
    abortRef.current = controller
    let sawSuccess = false

    // One NDJSON progress line from Ollama: update the bar, remember
    // the terminal "success", and turn an in-stream error into a throw.
    const consume = (raw: string) => {
      const line = raw.trim()
      if (line === '') return
      let parsed: PullLine
      try {
        parsed = JSON.parse(line) as PullLine
      } catch {
        return
      }
      if (parsed.error) throw new Error(parsed.error)
      if (parsed.status) {
        setStatus(parsed.status)
        if (parsed.status === 'success') sawSuccess = true
      }
      if (
        typeof parsed.total === 'number' &&
        parsed.total > 0 &&
        typeof parsed.completed === 'number'
      ) {
        setPercent(Math.min(100, Math.round((parsed.completed / parsed.total) * 100)))
      } else if (parsed.status && !parsed.total) {
        // A new phase (verifying, writing manifest…) without byte
        // counts — drop the stale percentage from the previous layer.
        setPercent(null)
      }
    }

    try {
      const res = await authedFetch('/api/ollama/pull', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model: ref }),
        signal: controller.signal,
      })
      if (!res.ok) {
        const data: unknown = await res.json().catch(() => ({}))
        const msg =
          typeof data === 'object' &&
          data !== null &&
          'error' in data &&
          typeof (data as { error: unknown }).error === 'string'
            ? (data as { error: string }).error
            : `HTTP ${res.status}`
        throw new Error(msg)
      }
      if (!res.body) throw new Error('No response stream')

      const reader = res.body.getReader()
      const decoder = new TextDecoder()
      let buf = ''
      for (;;) {
        const { done: eof, value } = await reader.read()
        if (eof) break
        buf += decoder.decode(value, { stream: true })
        let idx = buf.indexOf('\n')
        while (idx >= 0) {
          consume(buf.slice(0, idx))
          buf = buf.slice(idx + 1)
          idx = buf.indexOf('\n')
        }
      }
      consume(buf)

      if (!sawSuccess) throw new Error('Pull ended before completing')
      setDone(ref)
      setModel('')
      void fetchModels()
    } catch (e) {
      if (controller.signal.aborted) {
        setError('Pull cancelled.')
      } else {
        setError(e instanceof Error ? e.message : 'Network error')
      }
    } finally {
      setPulling(false)
      setStatus(null)
      setPercent(null)
      abortRef.current = null
    }
  }

  return (
    <div className="ollama-pull" data-testid="ollama-pull">
      <h4>Pull a Model</h4>
      <p className="form-hint">
        Download a model onto the configured server: an Ollama registry name (llama3.2,
        qwen2.5-coder:7b) or a Hugging Face GGUF repo (hf.co/&lt;user&gt;/&lt;repo&gt;, optionally
        :&lt;quant&gt;). Add @&lt;server name&gt; to pull onto one of the additional servers.
      </p>
      <div className="ollama-pull-row">
        <input
          className="form-input"
          type="text"
          value={model}
          placeholder="llama3.2 or hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF"
          onChange={(e) => {
            setModel(e.target.value)
            setDone(null)
            setError(null)
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter') void startPull()
          }}
          disabled={pulling}
          data-testid="ollama-pull-input"
        />
        {pulling ? (
          <button
            type="button"
            className="plugin-settings-save"
            onClick={() => abortRef.current?.abort()}
            data-testid="ollama-pull-cancel"
          >
            Cancel
          </button>
        ) : (
          <button
            type="button"
            className="plugin-settings-save"
            onClick={() => void startPull()}
            disabled={model.trim() === ''}
            data-testid="ollama-pull-button"
          >
            Pull
          </button>
        )}
      </div>
      {pulling && (
        <div className="ollama-pull-progress" data-testid="ollama-pull-progress">
          <div className="ollama-pull-progress-track">
            <div
              className="ollama-pull-progress-fill"
              style={{
                width: `${percent ?? 100}%`,
                opacity: percent === null ? 0.35 : 1,
              }}
            />
          </div>
          <span className="ollama-pull-status">
            {status}
            {percent !== null ? ` — ${percent}%` : ''}
          </span>
        </div>
      )}
      {error && <p className="plugin-settings-error">{error}</p>}
      {done && (
        <p className="plugin-settings-success">
          Pulled {done} — now available in the model picker as ollama:{done}.
        </p>
      )}
    </div>
  )
}
