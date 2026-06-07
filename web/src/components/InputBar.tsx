import { useCallback, useRef, useState, useEffect } from 'react'
import type { KeyboardEvent, ChangeEvent } from 'react'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'

interface InputBarProps {
  sessionId: string
  agentWorking: boolean
}

interface PendingAttachment {
  id: string
  name: string
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => {
      const result = reader.result as string
      // Strip the data URL prefix (e.g. "data:image/png;base64,")
      const base64 = result.split(',')[1] ?? result
      resolve(base64)
    }
    reader.onerror = reject
    reader.readAsDataURL(file)
  })
}

export default function InputBar({ sessionId }: InputBarProps) {
  const getDraft = useSessionsStore((s) => s.getDraft)
  const setDraft = useSessionsStore((s) => s.setDraft)

  const [text, setText] = useState(() => getDraft(sessionId))
  const [sending, setSending] = useState(false)
  const [attachments, setAttachments] = useState<PendingAttachment[]>([])
  const [uploading, setUploading] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)

  // Re-initialize draft when sessionId changes
  useEffect(() => {
    setText(getDraft(sessionId))
    setAttachments([])
  }, [sessionId, getDraft])

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
    const val = e.target.value
    setText(val)
    setDraft(sessionId, val)
  }

  const handleFileSelect = useCallback(
    async (e: ChangeEvent<HTMLInputElement>) => {
      const files = e.target.files
      if (!files || files.length === 0) return
      setUploading(true)
      try {
        for (const file of Array.from(files)) {
          const base64 = await fileToBase64(file)
          const res = await authedFetch(`/api/sessions/${sessionId}/attachments`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: file.name, data: base64, mime_type: file.type }),
          })
          if (res.ok) {
            const result = await res.json()
            setAttachments((prev) => [...prev, { id: result.id, name: file.name }])
          }
        }
      } finally {
        setUploading(false)
        // Reset input so the same file can be re-selected
        if (fileInputRef.current) fileInputRef.current.value = ''
      }
    },
    [sessionId],
  )

  const removeAttachment = useCallback((id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id))
  }, [])

  const handleSend = useCallback(async () => {
    const trimmed = text.trim()
    if ((!trimmed && attachments.length === 0) || sending) return
    setSending(true)
    try {
      const body: Record<string, unknown> = { text: trimmed }
      if (attachments.length > 0) {
        body.attachmentIds = attachments.map((a) => a.id)
      }
      await authedFetch(`/api/sessions/${sessionId}/message`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      setText('')
      setDraft(sessionId, '')
      setAttachments([])
    } finally {
      setSending(false)
    }
  }, [text, sending, sessionId, setDraft, attachments])

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    const isMobile = window.matchMedia('(pointer: coarse)').matches
    if (e.key === 'Enter' && !e.shiftKey && !isMobile) {
      e.preventDefault()
      handleSend()
    }
  }

  const canSend = (text.trim().length > 0 || attachments.length > 0) && !sending

  return (
    <div className="input-bar">
      <div className="input-bar-inner">
        <input
          ref={fileInputRef}
          type="file"
          multiple
          style={{ display: 'none' }}
          onChange={handleFileSelect}
        />
        <button
          className="upload-btn"
          onClick={() => fileInputRef.current?.click()}
          disabled={uploading || sending}
          type="button"
          title="Attach files"
        >
          <svg
            width="18"
            height="18"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" />
          </svg>
        </button>
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
          <button className="send-btn" onClick={handleSend} disabled={!canSend} type="button">
            Send
          </button>
        </div>
      </div>
      {attachments.length > 0 && (
        <div className="attachment-chips">
          {uploading && (
            <span className="attachment-chip attachment-chip-uploading">Uploading...</span>
          )}
          {attachments.map((a) => (
            <span key={a.id} className="attachment-chip">
              <span className="attachment-chip-icon">{'\u{1F4CE}'}</span>
              <span className="attachment-chip-name">{a.name}</span>
              <button
                className="attachment-chip-remove"
                onClick={() => removeAttachment(a.id)}
                type="button"
              >
                &times;
              </button>
            </span>
          ))}
        </div>
      )}
    </div>
  )
}
