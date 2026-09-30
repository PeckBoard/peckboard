import { useEffect, useState } from 'react'
import { useVoiceStore } from '../store/voice'
import { useResourcesStore } from '../store/resources'
import { getSpeechEngine, type VoiceOption } from '../voice/engine'
import { isIOS, isKokoroVoice, setIosUseBrowserVoice, useKokoroDevicePrefs } from '../voice/kokoro'
import ModelPicker from './ModelPicker'
import VoicePronunciations from './VoicePronunciations'

/** Recognition languages offered in the picker. The browser default (`''`)
 *  follows the page/OS language. */
const RECOGNITION_LANGS: { id: string; label: string }[] = [
  { id: '', label: 'Browser default' },
  { id: 'en-US', label: 'English (US)' },
  { id: 'en-GB', label: 'English (UK)' },
  { id: 'en-AU', label: 'English (Australia)' },
  { id: 'en-IN', label: 'English (India)' },
  { id: 'de-DE', label: 'German' },
  { id: 'fr-FR', label: 'French' },
  { id: 'es-ES', label: 'Spanish (Spain)' },
  { id: 'es-MX', label: 'Spanish (Mexico)' },
  { id: 'it-IT', label: 'Italian' },
  { id: 'pt-BR', label: 'Portuguese (Brazil)' },
  { id: 'pt-PT', label: 'Portuguese (Portugal)' },
  { id: 'nl-NL', label: 'Dutch' },
  { id: 'sv-SE', label: 'Swedish' },
  { id: 'da-DK', label: 'Danish' },
  { id: 'nb-NO', label: 'Norwegian' },
  { id: 'fi-FI', label: 'Finnish' },
  { id: 'pl-PL', label: 'Polish' },
  { id: 'cs-CZ', label: 'Czech' },
  { id: 'tr-TR', label: 'Turkish' },
  { id: 'ru-RU', label: 'Russian' },
  { id: 'uk-UA', label: 'Ukrainian' },
  { id: 'ja-JP', label: 'Japanese' },
  { id: 'ko-KR', label: 'Korean' },
  { id: 'zh-CN', label: 'Chinese (Mandarin, Simplified)' },
  { id: 'zh-TW', label: 'Chinese (Mandarin, Traditional)' },
  { id: 'hi-IN', label: 'Hindi' },
  { id: 'ar-SA', label: 'Arabic' },
]

/**
 * Settings → Voice. Per-browser speech prefs (voice, rate, pitch,
 * recognition language, auto-listen) live in localStorage via the voice
 * store; the assistant's model is server-side on the voice session.
 */
export default function VoiceSettingsSection() {
  const prefs = useVoiceStore((s) => s.prefs)
  const setPrefs = useVoiceStore((s) => s.setPrefs)
  const model = useVoiceStore((s) => s.model)
  const ensureSession = useVoiceStore((s) => s.ensureSession)
  const setModel = useVoiceStore((s) => s.setModel)
  const testVoice = useVoiceStore((s) => s.testVoice)
  const recognitionSupported = useVoiceStore((s) => s.recognitionSupported)
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  const [voices, setVoices] = useState<VoiceOption[]>(() => getSpeechEngine().getVoices())
  const [modelError, setModelError] = useState<string | null>(null)
  // The store's error is what `ensureSession` left behind (e.g. no folder
  // yet); it only matters here while the model is still unknown.
  const sessionError = useVoiceStore((s) => s.error)
  const shownError = modelError ?? (model === null ? sessionError : null)
  const [modelBusy, setModelBusy] = useState(false)

  // `getVoices()` is empty until the browser has loaded its voice list;
  // `voiceschanged` fires when it has (and again if voices are installed).
  useEffect(() => {
    const engine = getSpeechEngine()
    const refresh = () => setVoices(engine.getVoices())
    refresh()
    return engine.onVoicesChanged(refresh)
  }, [])

  // The model lives on the voice session; get-or-create it to read it.
  useEffect(() => {
    if (model === null) void ensureSession()
  }, [model, ensureSession])

  useEffect(() => {
    if (models.length === 0) void fetchModels()
  }, [models.length, fetchModels])

  const onIOS = isIOS()
  const iosUseBrowserVoice = useKokoroDevicePrefs((s) => s.iosUseBrowserVoice)
  const synthesisSupported = getSpeechEngine().supportsSynthesis()

  const changeModel = async (id: string) => {
    if (!id || id === model) return
    setModelBusy(true)
    setModelError(null)
    try {
      await setModel(id)
      if (useVoiceStore.getState().model !== id) {
        setModelError(useVoiceStore.getState().error ?? "Couldn't save the voice model.")
      }
    } finally {
      setModelBusy(false)
    }
  }

  return (
    <>
      <section
        className="settings-section"
        data-testid="voice-speech-section"
        data-settings-anchor="voice-speech"
      >
        <h3>Speech</h3>
        <p className="form-hint">
          How the assistant sounds and which language it listens for. These are per-browser: the
          voices come from this device&apos;s speech engine.
        </p>
        {!recognitionSupported && (
          <p className="form-error" role="alert" data-testid="voice-unsupported">
            This browser has no speech recognition. Replies can still be read aloud, but the Listen
            button stays disabled — try Chrome, Edge, or Safari.
          </p>
        )}
        <div className="settings-info-grid">
          <label className="settings-row voice-settings-row">
            <span className="settings-label" id="voice-voice-label">
              Voice
            </span>
            <select
              className="form-input voice-settings-select"
              data-testid="voice-voice-select"
              aria-labelledby="voice-voice-label"
              value={prefs.voiceURI}
              onChange={(e) => setPrefs({ voiceURI: e.target.value })}
              disabled={!synthesisSupported}
            >
              {voices.some((v) => isKokoroVoice(v.voiceURI)) && (
                <optgroup label="Natural (server)">
                  {voices
                    .filter((v) => isKokoroVoice(v.voiceURI))
                    .map((v) => (
                      <option key={v.voiceURI} value={v.voiceURI}>
                        {v.name}
                      </option>
                    ))}
                </optgroup>
              )}
              <optgroup label="Browser">
                <option value="">Browser default</option>
                {voices
                  .filter((v) => !isKokoroVoice(v.voiceURI))
                  .map((v) => (
                    <option key={v.voiceURI} value={v.voiceURI}>
                      {v.name} ({v.lang}){v.default ? ' — default' : ''}
                    </option>
                  ))}
              </optgroup>
            </select>
          </label>
          <label className="settings-row voice-settings-row">
            <span className="settings-label">Rate</span>
            <input
              type="range"
              className="voice-settings-slider"
              data-testid="voice-rate"
              min={0.5}
              max={2}
              step={0.1}
              value={prefs.rate}
              onChange={(e) => setPrefs({ rate: Number(e.target.value) })}
              aria-label="Speech rate"
            />
            <span className="voice-settings-value" data-testid="voice-rate-value">
              {prefs.rate.toFixed(1)}×
            </span>
          </label>
          <label className="settings-row voice-settings-row">
            <span className="settings-label">Pitch</span>
            <input
              type="range"
              className="voice-settings-slider"
              data-testid="voice-pitch"
              min={0}
              max={2}
              step={0.1}
              value={prefs.pitch}
              onChange={(e) => setPrefs({ pitch: Number(e.target.value) })}
              aria-label="Speech pitch"
            />
            <span className="voice-settings-value" data-testid="voice-pitch-value">
              {prefs.pitch.toFixed(1)}
            </span>
          </label>
          <label className="settings-row voice-settings-row">
            <span className="settings-label" id="voice-lang-label">
              Language
            </span>
            <select
              className="form-input voice-settings-select"
              data-testid="voice-lang-select"
              aria-labelledby="voice-lang-label"
              value={prefs.lang}
              onChange={(e) => setPrefs({ lang: e.target.value })}
            >
              {RECOGNITION_LANGS.map((l) => (
                <option key={l.id || 'default'} value={l.id}>
                  {l.label}
                </option>
              ))}
            </select>
          </label>
          <label className="settings-row voice-settings-row">
            <span className="settings-label">Wait for me to finish (mid-sentence pause)</span>
            <input
              type="range"
              className="voice-settings-slider"
              data-testid="voice-max-pause"
              min={2000}
              max={10000}
              step={500}
              value={prefs.maxPauseMs}
              onChange={(e) => setPrefs({ maxPauseMs: Number(e.target.value) })}
              aria-label="Longest mid-sentence pause to wait out"
            />
            <span className="voice-settings-value" data-testid="voice-max-pause-value">
              {(prefs.maxPauseMs / 1000).toFixed(1)}s
            </span>
          </label>
          <label className="settings-row settings-row-toggle">
            <input
              type="checkbox"
              data-testid="voice-auto-listen"
              checked={prefs.autoListen}
              onChange={(e) => setPrefs({ autoListen: e.target.checked })}
            />
            <span className="settings-label">
              Turn the microphone on when the voice panel opens
            </span>
          </label>
          {onIOS && (
            <label className="settings-row settings-row-toggle">
              <input
                type="checkbox"
                data-testid="voice-ios-browser-voice"
                checked={iosUseBrowserVoice}
                onChange={(e) => setIosUseBrowserVoice(e.target.checked)}
              />
              <span className="settings-label">
                On iPhone/iPad, use the browser voice instead of Kokoro
              </span>
            </label>
          )}
        </div>
        <div className="voice-settings-actions">
          <button
            type="button"
            className="btn-secondary"
            data-testid="voice-test"
            onClick={testVoice}
            disabled={!synthesisSupported}
          >
            Test voice
          </button>
        </div>
      </section>

      <section
        className="settings-section"
        data-testid="voice-model-section"
        data-settings-anchor="voice-model"
      >
        <h3>Assistant Model</h3>
        <p className="form-hint">
          The model behind the voice session. A small, fast model keeps replies snappy — it only
          converses, routes work to other sessions, and relays their questions.
        </p>
        <ModelPicker
          value={model ?? ''}
          onChange={(id) => void changeModel(id)}
          models={models}
          ariaLabel="Voice assistant model"
          testId="voice-model"
          emptyHint="Loading models…"
          onOpen={fetchModels}
          disabled={modelBusy}
          valueLabel={model === null ? (sessionError ? 'Unavailable' : 'Loading…') : undefined}
        />
        {shownError && (
          <p className="form-error" role="alert" data-testid="settings-error-voice-model">
            {shownError}
          </p>
        )}
      </section>

      <VoicePronunciations />
    </>
  )
}
