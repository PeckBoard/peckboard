import { useCallback, useRef, useState, useEffect } from 'react'
import type { KeyboardEvent, ChangeEvent } from 'react'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'
import { useMentions, filterMentions, type MentionItem } from '../hooks/useMentions'

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
  const allMentions = useMentions(sessionId)

  // The parent passes a `key={sessionId}` so this component remounts
  // per session — that handles the "reset on session change" case
  // without an effect that synchronously calls setState.
  const [text, setText] = useState(() => getDraft(sessionId))
  const [sending, setSending] = useState(false)
  const [attachments, setAttachments] = useState<PendingAttachment[]>([])
  const [uploading, setUploading] = useState(false)
  const [suggestions, setSuggestions] = useState<MentionItem[]>([])
  const [showAutocomplete, setShowAutocomplete] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)

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

    // Check for @ autocomplete trigger
    const cursor = e.target.selectionStart ?? val.length
    const before = val.slice(0, cursor)
    const atMatch = before.match(/@(\S*)$/)
    if (atMatch) {
      const filtered = filterMentions(allMentions, atMatch[1])
      setSuggestions(filtered)
      setShowAutocomplete(filtered.length > 0)
    } else {
      setShowAutocomplete(false)
    }
  }

  const insertSuggestion = (item: MentionItem) => {
    const cursor = textareaRef.current?.selectionStart ?? text.length
    const before = text.slice(0, cursor)
    const after = text.slice(cursor)
    const atIdx = before.lastIndexOf('@')
    const newText = before.slice(0, atIdx) + item.ref + after
    setText(newText)
    setDraft(sessionId, newText)
    setShowAutocomplete(false)
    textareaRef.current?.focus()
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
    const attachmentIds = attachments.map((a) => a.id)
    // Clear the composer up-front: lets the user start typing the next
    // message immediately (matches Slack/Discord/iMessage), and avoids
    // clobbering anything they type during the in-flight request.
    setSending(true)
    setText('')
    setDraft(sessionId, '')
    setAttachments([])
    try {
      const body: Record<string, unknown> = { text: trimmed }
      if (attachmentIds.length > 0) {
        body.attachmentIds = attachmentIds
      }
      await authedFetch(`/api/sessions/${sessionId}/message`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
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
      {/* Autocomplete dropdown for @mentions */}
      {showAutocomplete && (
        <div className="autocomplete-dropdown">
          <div className="autocomplete-header">@ — reports &amp; sessions</div>
          {suggestions.map((s, i) => (
            <button
              key={`${s.type}-${s.detail}-${i}`}
              className="autocomplete-item"
              onMouseDown={(e) => {
                e.preventDefault()
                insertSuggestion(s)
              }}
            >
              <span className="autocomplete-item-title">
                <span className={`autocomplete-type-badge autocomplete-type-${s.type}`}>
                  {s.type}
                </span>
                {s.label}
              </span>
              <span className="autocomplete-item-path">{s.detail}</span>
            </button>
          ))}
        </div>
      )}
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
          // Intentionally not disabled while sending: on mobile, disabling
          // a focused textarea blurs it, which closes the soft keyboard
          // and shifts the layout. handleSend already guards re-entry.
        />
        <div className="input-buttons">
          <button
            className="send-btn"
            onClick={handleSend}
            // Prevent the textarea from blurring on tap: on mobile the
            // resulting soft-keyboard close shifts the input bar down and
            // the click lands on empty space, so the first tap is wasted.
            onPointerDown={(e) => {
              if (canSend) e.preventDefault()
            }}
            disabled={!canSend}
            type="button"
          >
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
