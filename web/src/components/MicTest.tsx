import { useState } from 'react'
import { useVoiceStore } from '../store/voice'
import { runMicDiagnostics } from '../voice/micDiagnostics'

/** Records what this browser or app does when asked for the microphone. */
export default function MicTest() {
  const [lines, setLines] = useState<string[]>([])
  const [running, setRunning] = useState(false)

  const run = async () => {
    setRunning(true)
    setLines([])
    const voice = useVoiceStore.getState()
    // Two recognizers can't share the mic: free it, then put it back.
    const micWasOn = voice.micOn
    if (micWasOn) voice.toggleMic()
    try {
      await runMicDiagnostics(voice.prefs.lang, (line) => setLines((all) => [...all, line]))
    } finally {
      if (micWasOn) useVoiceStore.getState().toggleMic()
      setRunning(false)
    }
  }

  return (
    <div className="settings-row voice-settings-row">
      <span className="settings-label">Mic test</span>
      <span className="speech-server-field">
        <button
          type="button"
          className="btn-secondary mic-test-button"
          data-testid="mic-test"
          disabled={running}
          onClick={() => void run()}
        >
          {running ? 'Testing… say something' : 'Test microphone'}
        </button>
        {lines.length > 0 && (
          <pre className="mic-test-output" data-testid="mic-test-output">
            {lines.join('\n')}
          </pre>
        )}
      </span>
    </div>
  )
}
