import { useCallback, useRef, useState, useEffect } from 'react'
import type { KeyboardEvent, ChangeEvent } from 'react'
import { authedFetch } from '../store/auth'

interface InputBarProps {
  sessionId: string
  agentWorking: boolean
}

export default function InputBar({ sessionId, agentWorking }: InputBarProps) {
  const [text, setText] = useState('')
  const [sending, setSending] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const resizeTextarea = useCallback(() => {
    const ta = textareaRef.current
    if (!ta) return
    ta.style.height = 'auto'
    const lineHeight = 20
    const maxHeight = lineHeight * 10
    ta.style.height = `${Math.min(ta.scrollHeight, maxHeight)}px`
  }, [])

  useEffect(() => {
    resizeTextarea()
  }, [text, resizeTextarea])

  const handleChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    setText(e.target.value)
  }

  const handleSend = useCallback(async () => {
    const trimmed = text.trim()
    if (!trimmed || sending) return
    setSending(true)
    try {
      await authedFetch(`/api/sessions/${sessionId}/events`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ kind: 'user', data: { text: trimmed } }),
      })
      setText('')
    } finally {
      setSending(false)
    }
  }, [text, sending, sessionId])

  const handleInterrupt = useCallback(async () => {
    await authedFetch(`/api/sessions/${sessionId}/interrupt`, {
      method: 'POST',
    })
  }, [sessionId])

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      handleSend()
    }
  }

  const canSend = text.trim().length > 0 && !sending

  return (
    <div className="input-bar">
      <div className="input-bar-inner">
        <textarea
          ref={textareaRef}
          className="input-textarea"
          rows={1}
          placeholder="Send a message..."
          value={text}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
          disabled={sending}
        />
        <div className="input-buttons">
          {agentWorking && (
            <button className="interrupt-btn" onClick={handleInterrupt} type="button">
              Stop
            </button>
          )}
          <button
            className="send-btn"
            onClick={handleSend}
            disabled={!canSend}
            type="button"
          >
            Send
          </button>
        </div>
      </div>
    </div>
  )
}
