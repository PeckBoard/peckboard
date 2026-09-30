import { useEffect } from 'react'
import { useVoiceStore } from '../store/voice'
import { isKokoroVoice, prepareKokoro, useKokoroStatus } from '../voice/kokoro'

/**
 * Voice-panel notice for the first-use Kokoro download. Opening the panel
 * with a Kokoro voice selected asks the server to fetch/load the model;
 * replies use the browser voice until it is ready.
 */
export default function KokoroStatusNotice() {
  const voiceURI = useVoiceStore((s) => s.prefs.voiceURI)
  const state = useKokoroStatus((s) => s.state)
  const progress = useKokoroStatus((s) => s.progress)
  const kokoro = isKokoroVoice(voiceURI)

  useEffect(() => {
    if (kokoro) void prepareKokoro()
  }, [kokoro])

  if (!kokoro || state !== 'downloading') return null
  const pct = Math.round(progress * 100)
  return (
    <div className="voice-tts-status" role="status" data-testid="voice-tts-status">
      {pct >= 100 ? 'Loading natural voice…' : `Downloading natural voice… ${pct}%`}
      <span className="voice-tts-status-hint"> The browser voice speaks meanwhile.</span>
    </div>
  )
}
