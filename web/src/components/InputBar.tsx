import { useCallback, useRef, useState, useEffect } from 'react'
import type { KeyboardEvent, ChangeEvent, ClipboardEvent } from 'react'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'
import { useMentions, filterMentions, type MentionItem } from '../hooks/useMentions'
import { describeActionError } from '../utils/actionError'

interface InputBarProps {
  sessionId: string
  agentWorking: boolean
  /** True while a provider/account model-switch handover is generating its
   *  doc. Sending is blocked (the backend returns 409) until it lands. */
  handoverActive?: boolean
  /** Non-null ⇒ the session's provider drops attachments; the attach
   *  button is disabled with this text as its tooltip and pasted files
   *  are refused with an error chip. */
  attachDisabledReason?: string | null
}

interface PendingAttachment {
  id: string
  name: string
  mimeType: string
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

export default function InputBar({
  sessionId,
  handoverActive = false,
  attachDisabledReason = null,
}: InputBarProps) {
  const getDraft = useSessionsStore((s) => s.getDraft)
  const setDraft = useSessionsStore((s) => s.setDraft)
  const addPendingUserMessage = useSessionsStore((s) => s.addPendingUserMessage)
  const removePendingUserMessage = useSessionsStore((s) => s.removePendingUserMessage)
  // Pulled straight from the store (no prop drilling) so the handover
  // banner can offer a Cancel that interrupts the in-flight doc turn — the
  // backend then aborts the switch and keeps the current model + context.
  const interruptSession = useSessionsStore((s) => s.interruptSession)
  const allMentions = useMentions(sessionId)

  // The parent passes a `key={sessionId}` so this component remounts
  // per session — that handles the "reset on session change" case
  // without an effect that synchronously calls setState.
  const [text, setText] = useState(() => getDraft(sessionId))
  const [sending, setSending] = useState(false)
  const [attachments, setAttachments] = useState<PendingAttachment[]>([])
  const [uploading, setUploading] = useState(false)
  const [uploadErrors, setUploadErrors] = useState<string[]>([])
  // A failed send: why it failed, plus the exact payload so Retry
  // re-sends what the user actually wrote.
  const [sendError, setSendError] = useState<{
    message: string
    text: string
    attachments: PendingAttachment[]
  } | null>(null)
  const [suggestions, setSuggestions] = useState<MentionItem[]>([])
  const [showAutocomplete, setShowAutocomplete] = useState(false)
  // Keyboard cursor over the @-mention list. The list is a listbox the
  // textarea points at via `aria-activedescendant`, so focus never leaves
  // the composer while the user picks.
  const [mentionIndex, setMentionIndex] = useState(0)
  // Handover-cancel request in flight / its failure. The interrupt used
  // to be fire-and-forget: a refusal left the banner spinning with no
  // sign anything had gone wrong, and every click stacked another call.
  const [cancellingHandover, setCancellingHandover] = useState(false)
  const [handoverCancelError, setHandoverCancelError] = useState<string | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)

  const resizeTextarea = useCallback(() => {
    const ta = textareaRef.current
    if (!ta) return
    ta.style.height = 'auto'
    const lineHeight = 20
    const maxHeight = lineHeight * 10
    const needsScroll = ta.scrollHeight > maxHeight
    // Hide the scrollbar while we have room to grow — the textarea is
    // auto-sized and the scrollbar only appears at the cap. Otherwise
    // browsers reserve the gutter and the user sees a permanent vertical
    // scrollbar on a 1-line input.
    ta.style.overflowY = needsScroll ? 'auto' : 'hidden'
    ta.style.height = `${Math.min(ta.scrollHeight, maxHeight)}px`
  }, [])

  useEffect(() => {
    resizeTextarea()
  }, [text, resizeTextarea])

  // Keep the keyboard cursor visible in the (scrollable) mention list.
  useEffect(() => {
    if (!showAutocomplete) return
    document
      .getElementById(`mention-opt-${sessionId}-${mentionIndex}`)
      ?.scrollIntoView({ block: 'nearest' })
  }, [showAutocomplete, mentionIndex, sessionId])

  const handleChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value
    // Any edit to the draft means the user has moved on from the failure.
    setSendError(null)
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
      setMentionIndex(0)
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
    setMentionIndex(0)
    textareaRef.current?.focus()
  }

  // Shared upload path for both the file picker and clipboard paste. A
  // pasted image often has a generic/empty `file.name` (e.g. "image.png"
  // or ""), so fall back to a stable name derived from the mime type.
  const uploadFiles = useCallback(
    async (files: File[]) => {
      if (files.length === 0) return
      if (attachDisabledReason) {
        // Reachable via paste or a programmatic file-input set while the
        // button is disabled — refuse loudly instead of uploading bytes
        // the provider would silently drop.
        setUploadErrors([attachDisabledReason])
        return
      }
      setUploading(true)
      setUploadErrors([])
      try {
        for (const file of files) {
          const name =
            file.name && file.name.trim().length > 0
              ? file.name
              : `pasted-${Date.now()}.${(file.type.split('/')[1] || 'bin').replace(/[^a-z0-9]/gi, '')}`
          // Per-file try/catch: one failed upload must not abort the rest
          // of the batch, and the user has to see WHICH file failed —
          // silently dropping it looks like the app ignored the pick.
          try {
            const base64 = await fileToBase64(file)
            const res = await authedFetch(`/api/sessions/${sessionId}/attachments`, {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ name, data: base64, mime_type: file.type }),
            })
            if (!res.ok) {
              const detail = (await res.json().catch(() => null))?.error
              throw new Error(typeof detail === 'string' ? detail : `upload failed (${res.status})`)
            }
            const result = await res.json()
            setAttachments((prev) => [...prev, { id: result.id, name, mimeType: file.type }])
          } catch (err) {
            const reason = err instanceof Error ? err.message : 'upload failed'
            setUploadErrors((prev) => [...prev, `${name}: ${reason}`])
          }
        }
      } finally {
        setUploading(false)
      }
    },
    [sessionId, attachDisabledReason],
  )

  const handleFileSelect = useCallback(
    async (e: ChangeEvent<HTMLInputElement>) => {
      const files = e.target.files
      if (!files || files.length === 0) return
      try {
        await uploadFiles(Array.from(files))
      } finally {
        // Reset input so the same file can be re-selected
        if (fileInputRef.current) fileInputRef.current.value = ''
      }
    },
    [uploadFiles],
  )

  // Paste an image (or any file) straight from the clipboard into the
  // composer — same upload path as the attach button. Screenshots come
  // through `clipboardData.items` as `kind: 'file'`; we pull those and
  // leave normal text paste alone (don't preventDefault unless we took a
  // file, or we'd swallow pasted text).
  const handlePaste = useCallback(
    (e: ClipboardEvent<HTMLTextAreaElement>) => {
      const items = e.clipboardData?.items
      if (!items) return
      const files: File[] = []
      for (const item of Array.from(items)) {
        if (item.kind === 'file') {
          const file = item.getAsFile()
          if (file) files.push(file)
        }
      }
      if (files.length === 0) return
      e.preventDefault()
      void uploadFiles(files)
    },
    [uploadFiles],
  )

  const removeAttachment = useCallback((id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id))
  }, [])

  // The send core, parameterised by its payload so Retry can re-send
  // exactly what failed instead of whatever the composer happens to
  // hold by then.
  const submitMessage = useCallback(
    async (trimmed: string, atts: PendingAttachment[]) => {
      if ((!trimmed && atts.length === 0) || sending || handoverActive) return
      const attachmentIds = atts.map((a) => a.id)
      const attachmentMeta = atts.map((a) => ({
        filename: a.name,
        mimeType: a.mimeType,
        id: a.id,
      }))
      // Clear the composer up-front: lets the user start typing the next
      // message immediately (matches Slack/Discord/iMessage), and avoids
      // clobbering anything they type during the in-flight request. Only
      // clear what we're actually sending — on a retry the composer may
      // already hold a different draft.
      setSending(true)
      setSendError(null)
      if ((textareaRef.current?.value ?? '').trim() === trimmed) {
        setText('')
        setDraft(sessionId, '')
      }
      setAttachments((prev) => prev.filter((a) => !attachmentIds.includes(a.id)))
      // Show the bubble optimistically. When the agent is mid-turn the
      // backend queues the message and the WS `user` event can lag a few
      // hundred ms behind the POST returning; without this the composer
      // empties but nothing visible happens until the round-trip
      // completes, which felt like the message had vanished. The real
      // event auto-clears the matching pending entry on arrival.
      const pendingId = addPendingUserMessage(sessionId, trimmed, attachmentMeta)
      try {
        const body: Record<string, unknown> = { text: trimmed }
        if (attachmentIds.length > 0) {
          body.attachmentIds = attachmentIds
        }
        const res = await authedFetch(`/api/sessions/${sessionId}/message`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        })
        if (!res.ok) {
          const detail = (await res.json().catch(() => null))?.error
          rollbackFailedSend(
            pendingId,
            describeActionError(
              typeof detail === 'string' ? new Error(detail) : null,
              `Couldn't send your message (${res.status}). It's back in the composer.`,
            ),
          )
        }
      } catch (e) {
        rollbackFailedSend(
          pendingId,
          describeActionError(
            e,
            "Couldn't reach the server. Your message is back in the composer.",
          ),
        )
      } finally {
        setSending(false)
      }

      // Rolling back a failed send: drop the optimistic bubble, and
      // restore the user's text only if the composer is still empty.
      // Stomping over text the user has started typing for the *next*
      // message would be exactly the clobber the up-front clear set
      // out to avoid (see comment at the top of this handler).
      // Attachments always come back — they're already uploaded and
      // still valid server-side, so dropping them would force the user
      // to re-pick every file. Merge rather than replace, in case more
      // were added while the request was in flight.
      function rollbackFailedSend(tempId: string, message: string) {
        removePendingUserMessage(sessionId, tempId)
        if (textareaRef.current?.value === '') {
          setText(trimmed)
          setDraft(sessionId, trimmed)
        }
        setAttachments((prev) => {
          const have = new Set(prev.map((a) => a.id))
          return [...atts.filter((a) => !have.has(a.id)), ...prev]
        })
        setSendError({ message, text: trimmed, attachments: atts })
      }
    },
    [sending, handoverActive, sessionId, setDraft, addPendingUserMessage, removePendingUserMessage],
  )

  const handleSend = useCallback(() => {
    void submitMessage(text.trim(), attachments)
  }, [submitMessage, text, attachments])

  const handleRetrySend = useCallback(() => {
    if (!sendError) return
    void submitMessage(sendError.text, sendError.attachments)
  }, [submitMessage, sendError])

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // The @-mention list owns the arrow / Enter / Escape keys while it is
    // open. This branch MUST come before the send branch below, or Enter
    // fires off the half-typed "@foo" instead of completing the mention.
    if (showAutocomplete && suggestions.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        setMentionIndex((i) => (i + 1) % suggestions.length)
        return
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault()
        setMentionIndex((i) => (i - 1 + suggestions.length) % suggestions.length)
        return
      }
      if (e.key === 'Enter' || e.key === 'Tab') {
        const item = suggestions[mentionIndex]
        if (item) {
          e.preventDefault()
          insertSuggestion(item)
          return
        }
      }
      if (e.key === 'Escape') {
        e.preventDefault()
        setShowAutocomplete(false)
        return
      }
    }
    const isMobile = window.matchMedia('(pointer: coarse)').matches
    if (e.key === 'Enter' && !e.shiftKey && !isMobile) {
      e.preventDefault()
      handleSend()
    }
  }
  const handleHandoverCancel = async () => {
    if (cancellingHandover) return
    setCancellingHandover(true)
    setHandoverCancelError(null)
    try {
      await interruptSession(sessionId)
    } catch (e) {
      setHandoverCancelError(
        describeActionError(e, "Couldn't cancel the switch. Please try again."),
      )
    } finally {
      setCancellingHandover(false)
    }
  }

  const canSend = (text.trim().length > 0 || attachments.length > 0) && !sending && !handoverActive

  return (
    <div className="input-bar">
      {handoverActive && (
        <div className="handover-banner" role="status" data-testid="handover-banner">
          <span className="handover-spinner" aria-hidden="true" />
          <span className="handover-banner-text">Handing over context to the new model…</span>
          <button
            type="button"
            className="handover-cancel-btn"
            data-testid="handover-cancel"
            onClick={() => void handleHandoverCancel()}
            disabled={cancellingHandover}
            aria-busy={cancellingHandover || undefined}
            title="Cancel the switch and keep the current model and context"
          >
            {cancellingHandover ? 'Cancelling…' : 'Cancel'}
          </button>
          {handoverCancelError && (
            <span
              className="handover-cancel-error"
              role="alert"
              data-testid="handover-cancel-error"
            >
              {handoverCancelError}
            </span>
          )}
        </div>
      )}
      {/* Autocomplete dropdown for @mentions. A listbox the composer points
          at with `aria-activedescendant`: focus stays in the textarea, so the
          user keeps typing while arrowing through the matches. */}
      {showAutocomplete && (
        <div className="autocomplete-dropdown">
          <div className="autocomplete-header">@ — reports &amp; sessions</div>
          <div id={`mention-list-${sessionId}`} role="listbox" aria-label="Mention suggestions">
            {suggestions.map((s, i) => (
              <button
                key={`${s.type}-${s.detail}-${i}`}
                id={`mention-opt-${sessionId}-${i}`}
                type="button"
                role="option"
                aria-selected={i === mentionIndex}
                tabIndex={-1}
                className={`autocomplete-item${i === mentionIndex ? ' is-active' : ''}`}
                onMouseEnter={() => setMentionIndex(i)}
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
        </div>
      )}
      {/* A failed send explains itself right above the composer, where
          the restored draft and attachment chips are. */}
      {sendError && (
        <div className="send-error" role="alert" data-testid="send-error">
          <span className="send-error-text">{sendError.message}</span>
          <button
            type="button"
            className="send-error-retry"
            data-testid="send-error-retry"
            onClick={handleRetrySend}
            disabled={sending}
          >
            Retry
          </button>
          <button
            type="button"
            className="send-error-dismiss"
            aria-label="Dismiss send error"
            onClick={() => setSendError(null)}
          >
            &times;
          </button>
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
          disabled={uploading || sending || !!attachDisabledReason}
          type="button"
          title={attachDisabledReason ?? 'Attach files'}
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
          placeholder={handoverActive ? 'Handover in progress…' : 'Send a message...'}
          value={text}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
          role="combobox"
          aria-autocomplete="list"
          aria-expanded={showAutocomplete}
          aria-controls={showAutocomplete ? `mention-list-${sessionId}` : undefined}
          aria-activedescendant={
            showAutocomplete ? `mention-opt-${sessionId}-${mentionIndex}` : undefined
          }
          onPaste={handlePaste}
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
            aria-label="Send message"
            title="Send"
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
              aria-hidden="true"
            >
              <path d="M22 2 11 13" />
              <path d="m22 2-7 20-4-9-9-4 20-7z" />
            </svg>
          </button>
        </div>
      </div>
      {uploadErrors.length > 0 && (
        <div className="attachment-chips" role="alert" data-testid="upload-error">
          {uploadErrors.map((msg) => (
            <span key={msg} className="attachment-chip attachment-chip-error">
              <span className="attachment-chip-name">{msg}</span>
            </span>
          ))}
          <button
            className="attachment-chip-remove"
            type="button"
            aria-label="Dismiss upload errors"
            onClick={() => setUploadErrors([])}
          >
            &times;
          </button>
        </div>
      )}
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
