import { useState, type KeyboardEvent } from 'react'
import SafeMarkdown from '../../SafeMarkdown'
import { NOTE_MAX } from '../../../store/views'
import WidgetFrame from '../WidgetFrame'
import type { InfoWidgetProps } from './types'
import '../../../styles/dashboard-info.css'

/** A note's title: its first markdown heading, else "Note". */
function noteTitle(body: string): string {
  const m = /^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$/m.exec(body)
  return m ? m[1].slice(0, 80) : 'Note'
}

/** Free-form markdown note. Renders read-only; Edit (menu, button, or a
 *  double-click) swaps in a textarea that saves on blur or Ctrl/⌘+Enter
 *  and discards on Escape. */
export default function NoteWidget({ widget, ctx, menuItems, onChange }: InfoWidgetProps) {
  const body = widget.body ?? ''
  const [draft, setDraft] = useState<string | null>(null)
  const editing = draft !== null
  const startEdit = () => setDraft(body)
  const commit = () => {
    if (draft === null) return
    const next = draft.slice(0, NOTE_MAX)
    setDraft(null)
    if (next !== body) onChange({ body: next.trim() ? next : null })
  }
  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault()
      commit()
    } else if (e.key === 'Escape') {
      e.preventDefault()
      setDraft(null)
    }
  }

  return (
    <WidgetFrame
      kind="note"
      widgetId={widget.id}
      title={noteTitle(body)}
      menuItems={[
        { label: 'Edit note', testId: 'dash-note-edit', hidden: editing, onSelect: startEdit },
        ...menuItems,
      ]}
      ctx={ctx}
    >
      {editing ? (
        <div className="dash-note-editor">
          <textarea
            className="form-input dash-note-textarea"
            value={draft}
            maxLength={NOTE_MAX}
            autoFocus
            placeholder="Markdown — **bold**, lists, links, `code`…"
            aria-label="Note text"
            onChange={(e) => setDraft(e.target.value)}
            onBlur={commit}
            onKeyDown={onKey}
            data-testid="dash-note-textarea"
          />
          <div className="dash-note-foot">
            <span>Ctrl+Enter to save · Esc to cancel</span>
            <span className={draft.length >= NOTE_MAX ? 'dash-note-limit' : undefined}>
              {draft.length.toLocaleString()} / {NOTE_MAX.toLocaleString()}
            </span>
          </div>
        </div>
      ) : body.trim() ? (
        <div
          className="dash-scroll"
          onDoubleClick={startEdit}
          title="Double-click to edit"
          data-testid="dash-note-body"
        >
          <SafeMarkdown className="report-content dash-markdown">{body}</SafeMarkdown>
        </div>
      ) : (
        <div className="dash-empty dash-note-empty" data-testid="dash-note-empty">
          <p>Empty note</p>
          <button
            type="button"
            className="btn-secondary btn-sm"
            onClick={startEdit}
            data-testid="dash-note-start"
          >
            Write a note
          </button>
        </div>
      )}
    </WidgetFrame>
  )
}
