import { useEffect, useMemo, useRef, useState } from 'react'
import { useVoiceStore, type VoiceStatus } from '../store/voice'
import { foldVoiceTranscript, type VoiceTranscriptItem } from '../voice/text'
import VoiceActionCard from './VoiceActionCard'
import KokoroStatusNotice from './KokoroStatusNotice'

const STATUS_LABEL: Record<VoiceStatus, string> = {
  idle: 'Idle',
  listening: 'Listening',
  thinking: 'Thinking',
  speaking: 'Speaking',
}

function MicIcon({ size = 18 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <rect x="9" y="3" width="6" height="11" rx="3" />
      <path d="M5 11a7 7 0 0 0 14 0M12 18v3M8 21h8" />
    </svg>
  )
}

/**
 * The "Listen" trigger: a rail button (rendered inside the navigation
 * rail's bottom cluster, next to the connection dot and the user avatar)
 * that toggles the voice panel. It lives in the rail rather than floating
 * over the page so it can never sit on top of the composer's Send button,
 * a queued-message chip, or a failed-send Retry — anything fixed to the
 * bottom-right corner collides with those.
 */
export function VoiceListenButton() {
  const supported = useVoiceStore((s) => s.recognitionSupported)
  const panelOpen = useVoiceStore((s) => s.panelOpen)
  const status = useVoiceStore((s) => s.status)
  const openPanel = useVoiceStore((s) => s.openPanel)
  const closePanel = useVoiceStore((s) => s.closePanel)

  const title = supported
    ? panelOpen
      ? 'Close Assistant'
      : 'Assistant'
    : 'The Assistant needs a browser with speech recognition (Chrome, Edge, or Safari).'

  return (
    <button
      type="button"
      className={`rail-btn voice-rail-btn${panelOpen ? ' active' : ''}`}
      data-testid="voice-fab"
      data-status={status}
      title={title}
      aria-label="Listen"
      aria-expanded={panelOpen}
      aria-disabled={!supported}
      onClick={() => {
        if (!supported) return
        if (panelOpen) closePanel()
        else void openPanel()
      }}
    >
      <MicIcon />
      {status !== 'idle' && <span className="rail-btn-dot voice-rail-dot" aria-hidden />}
    </button>
  )
}

/**
 * Assistant panel: docked top-right, non-blocking, with the voice
 * session's transcript, a status indicator, the mic toggle, and a typed
 * fallback. The speech loop itself lives in `store/voice.ts`; this
 * component only renders it. Opened from [`VoiceListenButton`].
 */
export default function VoiceDock() {
  const supported = useVoiceStore((s) => s.recognitionSupported)
  const panelOpen = useVoiceStore((s) => s.panelOpen)
  const status = useVoiceStore((s) => s.status)
  const micOn = useVoiceStore((s) => s.micOn)
  const interim = useVoiceStore((s) => s.interim)
  const heard = useVoiceStore((s) => s.heard)
  const turnUnfinished = useVoiceStore((s) => s.turnUnfinished)
  const events = useVoiceStore((s) => s.events)
  const pending = useVoiceStore((s) => s.pending)
  const error = useVoiceStore((s) => s.error)
  const actions = useVoiceStore((s) => s.actions)
  const closePanel = useVoiceStore((s) => s.closePanel)
  const toggleMic = useVoiceStore((s) => s.toggleMic)
  const stopSpeaking = useVoiceStore((s) => s.stopSpeaking)
  const sendText = useVoiceStore((s) => s.sendText)
  const clearError = useVoiceStore((s) => s.clearError)
  const unlockSpeech = useVoiceStore((s) => s.unlockSpeech)

  const [draft, setDraft] = useState('')
  const listRef = useRef<HTMLDivElement>(null)

  const transcript = useMemo<VoiceTranscriptItem[]>(() => {
    const folded = foldVoiceTranscript(events)
    const extras: VoiceTranscriptItem[] = pending.map((p) => ({
      kind: 'user',
      key: p.tempId,
      text: p.text,
      ts: p.ts,
      pending: true,
    }))
    return [...folded, ...extras]
  }, [events, pending])

  useEffect(() => {
    const el = listRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [transcript, interim, heard])

  // Close the panel (and stop the mic / speech) when the app unmounts.
  useEffect(() => () => useVoiceStore.getState().closePanel(), [])

  const submitDraft = () => {
    const text = draft.trim()
    if (!text) return
    setDraft('')
    void sendText(text)
  }

  if (!panelOpen) return null

  return (
    <section
      className="voice-panel"
      data-testid="voice-panel"
      data-status={status}
      aria-label="Assistant"
      // Any click in the panel counts as the user gesture some browsers
      // need before they allow speech output.
      onPointerDown={unlockSpeech}
    >
      <header className="voice-panel-header">
        <h2 className="voice-panel-title">Assistant</h2>
        <span
          className={`voice-status voice-status-${status}`}
          data-testid="voice-status"
          role="status"
        >
          <span className="voice-status-dot" aria-hidden />
          {STATUS_LABEL[status]}
        </span>
        <button
          type="button"
          className="voice-panel-close"
          data-testid="voice-close"
          aria-label="Close Assistant"
          onClick={closePanel}
        >
          ×
        </button>
      </header>

      <div className="voice-transcript" ref={listRef} data-testid="voice-transcript">
        {transcript.length === 0 && !interim && !heard && (
          <p className="voice-transcript-empty">
            Just talk — the microphone stays on, and you can cut in while a reply is being read
            aloud. Updates from other sessions show up here as they arrive.
          </p>
        )}
        {transcript.map((it) => {
          if (it.kind === 'relay') {
            return (
              <div
                key={it.key}
                className="voice-line voice-line-relay"
                data-testid="voice-line-relay"
              >
                {it.text}
              </div>
            )
          }
          if (it.kind === 'assistant') {
            return (
              <div
                key={it.key}
                className={`voice-line voice-line-assistant${it.streaming ? ' streaming' : ''}`}
                data-testid="voice-line-assistant"
              >
                {it.text}
              </div>
            )
          }
          return (
            <div
              key={it.key}
              className={`voice-line voice-line-user${it.pending ? ' pending' : ''}`}
              data-testid="voice-line-user"
            >
              {it.text}
            </div>
          )
        })}
        {(heard || interim) && (
          <div
            className={`voice-line voice-line-user interim${turnUnfinished ? ' unfinished' : ''}`}
            data-testid="voice-interim"
            data-unfinished={turnUnfinished ? 'true' : undefined}
            title={turnUnfinished ? 'Waiting for you to finish the sentence' : undefined}
          >
            {[heard, interim].filter(Boolean).join(' ')}…
          </div>
        )}
      </div>

      {actions.map((a) => (
        <VoiceActionCard key={a.id} action={a} />
      ))}
      {error && (
        <div className="voice-error" role="alert" data-testid="voice-error">
          <span>{error}</span>
          <button type="button" className="voice-error-dismiss" onClick={clearError}>
            Dismiss
          </button>
        </div>
      )}
      <KokoroStatusNotice />

      <div className="voice-controls">
        <button
          type="button"
          className={`voice-mic${micOn ? ' active' : ''}`}
          data-testid="voice-mic"
          aria-pressed={micOn}
          aria-label={micOn ? 'Mute microphone' : 'Start listening'}
          title={micOn ? 'Listening — just talk. Click to mute the microphone.' : 'Start listening'}
          disabled={!supported}
          onClick={toggleMic}
        >
          <MicIcon size={26} />
        </button>
        {(status === 'speaking' || status === 'thinking') && (
          <button
            type="button"
            className="voice-stop"
            data-testid="voice-stop"
            onClick={stopSpeaking}
          >
            Stop speaking
          </button>
        )}
      </div>

      <form
        className="voice-type-row"
        onSubmit={(e) => {
          e.preventDefault()
          submitDraft()
        }}
      >
        <input
          type="text"
          className="form-input voice-type-input"
          data-testid="voice-type-input"
          placeholder="Or type instead…"
          aria-label="Type a message to the Assistant"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
        <button
          type="submit"
          className="voice-type-send"
          data-testid="voice-type-send"
          disabled={draft.trim() === ''}
        >
          Send
        </button>
      </form>
    </section>
  )
}
