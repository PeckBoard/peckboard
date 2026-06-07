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

interface ReportSuggestion {
  folder: string
  file: string
  title: string
}

export default function InputBar({ sessionId }: InputBarProps) {
  const getDraft = useSessionsStore((s) => s.getDraft)
  const setDraft = useSessionsStore((s) => s.setDraft)

  const [text, setText] = useState(() => getDraft(sessionId))
  const [sending, setSending] = useState(false)
  const [attachments, setAttachments] = useState<PendingAttachment[]>([])
  const [uploading, setUploading] = useState(false)
  const [reportSuggestions, setReportSuggestions] = useState<ReportSuggestion[]>([])
  const [showReportAutocomplete, setShowReportAutocomplete] = useState(false)
  const [allReports, setAllReports] = useState<ReportSuggestion[]>([])
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)

  // Fetch report list for autocomplete
  useEffect(() => {
    authedFetch('/api/reports')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        const list = Array.isArray(data) ? data : (data?.reports ?? [])
        setAllReports(list.map((r: ReportSuggestion & { date?: string }) => ({
          folder: r.folder,
          file: r.file,
          title: r.title || r.file,
        })))
      })
      .catch(() => {})
  }, [])

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

    // Check for @report autocomplete trigger
    const cursor = e.target.selectionStart ?? val.length
    const before = val.slice(0, cursor)
    const atMatch = before.match(/@(\S*)$/)
    if (atMatch) {
      const filter = atMatch[1].toLowerCase()
      const filtered = allReports.filter((r) =>
        r.title.toLowerCase().includes(filter) || r.file.toLowerCase().includes(filter)
      )
      setReportSuggestions(filtered.slice(0, 8))
      setShowReportAutocomplete(filtered.length > 0)
    } else {
      setShowReportAutocomplete(false)
    }
  }

  const insertReport = (report: ReportSuggestion) => {
    const cursor = textareaRef.current?.selectionStart ?? text.length
    const before = text.slice(0, cursor)
    const after = text.slice(cursor)
    const atIdx = before.lastIndexOf('@')
    const ref = `[report:${report.folder}/${report.file}]`
    const newText = before.slice(0, atIdx) + ref + after
    setText(newText)
    setDraft(sessionId, newText)
    setShowReportAutocomplete(false)
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
      {/* Report autocomplete dropdown */}
      {showReportAutocomplete && (
        <div className="autocomplete-dropdown">
          <div className="autocomplete-header">Reports — type to filter</div>
          {reportSuggestions.map((r) => (
            <button
              key={`${r.folder}/${r.file}`}
              className="autocomplete-item"
              onMouseDown={(e) => { e.preventDefault(); insertReport(r) }}
            >
              <span className="autocomplete-item-title">{r.title}</span>
              <span className="autocomplete-item-path">{r.folder}/{r.file}</span>
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
