import { useEffect, useState } from 'react'
import { setMicrophone, useSpeechRecognitionPrefs } from '../voice/serverStt'

interface Microphone {
  id: string
  /** Empty until the page has been granted the microphone once. */
  label: string
}

/** Audio inputs, kept current as devices come and go. Chromium's "default" and
 *  "communications" entries are aliases of real devices, and the first option
 *  below already means the system default. */
function useMicrophones(): Microphone[] {
  const [mics, setMics] = useState<Microphone[]>([])
  useEffect(() => {
    const devices = navigator.mediaDevices
    if (!devices?.enumerateDevices) return
    let live = true
    const refresh = () => {
      devices
        .enumerateDevices()
        .then((all) => {
          if (!live) return
          setMics(
            all
              .filter(
                (d) =>
                  d.kind === 'audioinput' &&
                  d.deviceId &&
                  d.deviceId !== 'default' &&
                  d.deviceId !== 'communications',
              )
              .map((d) => ({ id: d.deviceId, label: d.label })),
          )
        })
        .catch(() => undefined)
    }
    refresh()
    devices.addEventListener?.('devicechange', refresh)
    return () => {
      live = false
      devices.removeEventListener?.('devicechange', refresh)
    }
  }, [])
  return mics
}

/**
 * Which microphone speech-server recognition captures from. Browser
 * recognition has no way to choose: it always uses the system's default input,
 * so the picker says so rather than offering a choice that would do nothing.
 */
export default function MicrophoneSelect({ serverRecognition }: { serverRecognition: boolean }) {
  const saved = useSpeechRecognitionPrefs((s) => s.microphoneId)
  const mics = useMicrophones()
  const missing = saved !== '' && mics.length > 0 && !mics.some((m) => m.id === saved)
  const unnamed = mics.some((m) => !m.label)
  const reason = !serverRecognition
    ? "Browser speech recognition always uses your system's default input. Choose Speech server above to pick one here, or change the default in your system's sound settings."
    : missing
      ? "That microphone isn't connected, so the system default is used until it is."
      : unnamed
        ? 'Turn the mic on once and allow it to see microphone names.'
        : null
  return (
    <label className="settings-row voice-settings-row">
      <span className="settings-label">Microphone</span>
      <span className="speech-server-field">
        <select
          className="form-input"
          data-testid="microphone-select"
          value={serverRecognition ? saved : ''}
          onChange={(e) => setMicrophone(e.target.value)}
          disabled={!serverRecognition}
        >
          <option value="">System default</option>
          {mics.map((m, i) => (
            <option key={m.id} value={m.id}>
              {m.label || `Microphone ${i + 1}`}
            </option>
          ))}
          {missing && <option value={saved}>Saved microphone (not connected)</option>}
        </select>
        {reason && (
          <span className="form-actions-reason" data-testid="microphone-reason">
            {reason}
          </span>
        )}
      </span>
    </label>
  )
}
