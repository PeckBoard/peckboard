import { useEffect, useState } from 'react'
import { useVoiceStore } from '../../store/voice'

/** How long the last caption stays up once nobody is talking. */
const LINGER_MS = 4000

interface Caption {
  speaker: 'user' | 'assistant'
  text: string
}

/**
 * Live captions under the orb: what the user is saying (interim speech)
 * while they talk, the sentence being read aloud while the Assistant
 * speaks. The last caption lingers a few seconds, then fades. Off with the
 * Captions setting.
 */
export default function AssistantCaptions() {
  const enabled = useVoiceStore((s) => s.prefs.captions)
  const interim = useVoiceStore((s) => s.interim)
  const heard = useVoiceStore((s) => s.heard)
  const speaking = useVoiceStore((s) => s.speakingText)

  const userText = [heard, interim].filter(Boolean).join(' ').trim()
  const live: Caption | null = userText
    ? { speaker: 'user', text: userText }
    : speaking
      ? { speaker: 'assistant', text: speaking }
      : null

  const [last, setLast] = useState<Caption | null>(null)
  const [expired, setExpired] = useState(true)
  // Remember the newest caption (state derived during render, not in an
  // effect) so it can linger after the speech that produced it stops.
  if (live && (live.text !== last?.text || live.speaker !== last?.speaker)) setLast(live)
  if (live && expired) setExpired(false)

  const liveKey = live ? `${live.speaker}:${live.text}` : ''
  useEffect(() => {
    if (liveKey) return
    const t = setTimeout(() => setExpired(true), LINGER_MS)
    return () => clearTimeout(t)
  }, [liveKey])

  if (!enabled) return null
  const caption = live ?? last
  const visible = caption !== null && (live !== null || !expired)
  return (
    <div
      className="assistant-captions"
      data-testid="assistant-captions"
      data-visible={visible ? 'true' : 'false'}
      data-speaker={caption?.speaker}
      role="status"
      aria-live="polite"
      aria-label="Live captions"
    >
      {caption && (
        <p className="assistant-caption-text">
          {caption.speaker === 'user' && <span className="assistant-caption-who">You</span>}
          {caption.text}
          {caption.speaker === 'user' && live ? '…' : ''}
        </p>
      )}
    </div>
  )
}
