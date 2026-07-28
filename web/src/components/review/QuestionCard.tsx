import { useState } from 'react'

import type { QuestionItem } from '../chat/events'
import { authedFetch } from '../../store/auth'
import { type DocAnchor } from '../../lib/review'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

interface Props {
  /** The review's AI session — the question lives on its event log. */
  sessionId: string
  /** Event id of the `question` event; echoed back as `question_id`. */
  questionId: string
  /** ControlRequest correlation id, when the provider supplied one. */
  requestId?: string
  questions: QuestionItem[]
  /** `pinned` heads the document column; `inline` is the chat lane's echo. */
  variant?: 'pinned' | 'inline'
  /** The passage the question is about, when one could be resolved. */
  anchor?: DocAnchor | null
  /** Scroll the document to `anchor` and light the block up. */
  onJump?: (anchor: DocAnchor) => void
}

/** An option that means "none of the above" gets a free-text box, so the
 *  answer is the user's own words rather than the literal word "Other". */
function isOtherOption(label: string): boolean {
  return /^other\b/i.test(label.trim())
}

/**
 * The reviewer's clarifying question, rendered where the user can still see
 * the document it is about.
 *
 * Deliberately not the chat's question card: that one is a boxed panel with
 * a title bar, sized for a wide feed, and at the head of a document column
 * it reads as a modal that swallowed the page. This is one strip — the
 * question, the passage it refers to (click it and the document scrolls
 * there), and the options as single-line rows. Everything below the fold of
 * a question stays one click away rather than one scroll.
 *
 * The submission is identical to the main chat's: a `question-resolved`
 * event carrying `{question_id, request_id?, answers}` keyed by question
 * index. The backend's resolution hook walks the review from `needs input`
 * back to `running`.
 */
export default function QuestionCard({
  sessionId,
  questionId,
  requestId,
  questions,
  variant = 'pinned',
  anchor = null,
  onJump,
}: Props) {
  const [answers, setAnswers] = useState<Record<number, string>>({})
  /** Free text typed against an "Other" option, per question. */
  const [otherText, setOtherText] = useState<Record<number, string>>({})
  const [submitting, setSubmitting] = useState(false)
  const [submitted, setSubmitted] = useState<Record<string, string> | null>(null)
  const [error, setError] = useState<string | null>(null)

  const setAnswer = (idx: number, value: string) => setAnswers((p) => ({ ...p, [idx]: value }))

  const toggleMulti = (idx: number, option: string) => {
    setAnswers((prev) => {
      const selected = prev[idx] ? prev[idx].split(',') : []
      const next = selected.includes(option)
        ? selected.filter((s) => s !== option)
        : [...selected, option]
      return { ...prev, [idx]: next.join(',') }
    })
  }

  /** The selection with any "Other" label swapped for what was typed. */
  const resolveAnswer = (idx: number): string => {
    const raw = (answers[idx] ?? '').trim()
    if (!raw) return ''
    const typed = (otherText[idx] ?? '').trim()
    if (!typed) return raw
    return raw
      .split(',')
      .map((part) => (isOtherOption(part) ? typed : part))
      .join(', ')
  }

  const answered = questions.some((_, idx) => resolveAnswer(idx).length > 0)

  const post = async (data: Record<string, unknown>) => {
    const res = await authedFetch(`/api/sessions/${encodeURIComponent(sessionId)}/events`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ kind: 'question-resolved', data }),
    })
    if (!res.ok) {
      const body = (await res.json().catch(() => null)) as { error?: string } | null
      throw new Error(body?.error || `HTTP ${res.status}`)
    }
  }

  const submit = () => {
    if (!answered || submitting) return
    setSubmitting(true)
    setError(null)
    const answerMap: Record<string, string> = {}
    questions.forEach((_, idx) => {
      const val = resolveAnswer(idx)
      if (val) answerMap[String(idx)] = val
    })
    post({
      question_id: questionId,
      ...(requestId ? { request_id: requestId } : {}),
      answers: answerMap,
    })
      // The resolved event arrives over the WS and replaces this card in the
      // chat lane; the pinned copy collapses to the summary below until the
      // refetch drops it.
      .then(() => setSubmitted(answerMap))
      .catch((e: unknown) => setError(describeActionError(e, "Couldn't send that answer.")))
      .finally(() => setSubmitting(false))
  }

  const dismiss = () => {
    if (submitting) return
    setSubmitting(true)
    setError(null)
    post({
      question_id: questionId,
      ...(requestId ? { request_id: requestId } : {}),
      rejected: true,
    })
      .then(() => setSubmitted({}))
      .catch((e: unknown) => setError(describeActionError(e, "Couldn't dismiss the question.")))
      .finally(() => setSubmitting(false))
  }

  if (submitted) {
    return (
      <section
        className={`review-ask review-ask--answered review-ask--${variant}`}
        data-testid="review-question-answered"
      >
        <span className="review-ask__mark review-ask__mark--done" aria-hidden="true">
          ✓
        </span>
        <p className="review-ask__answer">
          {Object.values(submitted).join(' · ') || 'Dismissed without answering.'}
        </p>
      </section>
    )
  }

  const lineLabel = anchor
    ? `L${anchor.start}${anchor.end !== anchor.start ? `–${anchor.end}` : ''}`
    : ''

  return (
    <section
      className={`review-ask review-ask--${variant}`}
      data-testid="review-question-card"
      aria-label="The reviewer needs an answer"
    >
      {questions.map((q, idx) => {
        const selected = (answers[idx] ?? '').split(',')
        const otherSelected = selected.some(isOtherOption)
        return (
          <div key={idx} className="review-ask__item">
            <div className="review-ask__head">
              <span className="review-ask__mark" aria-hidden="true">
                ?
              </span>
              <div className="review-ask__text">
                {q.header && <span className="review-ask__eyebrow">{q.header}</span>}
                <p className="review-ask__question">{q.question}</p>
              </div>
              {/* The passage is the whole point: a question about a document
                  you can't see is a question you answer by guessing. */}
              {idx === 0 && anchor && onJump && (
                <button
                  type="button"
                  className="review-ask__anchor"
                  data-testid="review-question-anchor"
                  title={anchor.quote}
                  onClick={() => onJump(anchor)}
                >
                  <span className="review-ask__anchor-lines">{lineLabel}</span>
                  <span className="review-ask__anchor-quote">{anchor.quote}</span>
                </button>
              )}
            </div>

            {q.options && q.options.length > 0 ? (
              <div className="review-ask__options" role={q.multiSelect ? 'group' : 'radiogroup'}>
                {q.options.map((opt, optIdx) => {
                  const desc = q.optionObjects?.[optIdx]?.description
                  const picked = q.multiSelect ? selected.includes(opt) : answers[idx] === opt
                  return (
                    <button
                      key={opt}
                      type="button"
                      role={q.multiSelect ? 'checkbox' : 'radio'}
                      aria-checked={picked}
                      className={`review-ask__option${picked ? ' review-ask__option--picked' : ''}`}
                      disabled={submitting}
                      // Descriptions stay one line: the full text is on hover
                      // and expands under the option once it's the choice.
                      title={desc}
                      onClick={() => (q.multiSelect ? toggleMulti(idx, opt) : setAnswer(idx, opt))}
                    >
                      <span className="review-ask__tick" aria-hidden="true" />
                      <span className="review-ask__option-label">{opt}</span>
                      {desc && <span className="review-ask__option-desc">{desc}</span>}
                    </button>
                  )
                })}
              </div>
            ) : (
              <input
                className="review-ask__input"
                type="text"
                placeholder="Type your answer…"
                aria-label={q.question}
                data-testid="review-question-input"
                value={answers[idx] ?? ''}
                disabled={submitting}
                onChange={(e) => setAnswer(idx, e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && questions.length === 1) submit()
                }}
              />
            )}

            {otherSelected && (
              <input
                className="review-ask__input"
                type="text"
                placeholder="Say what you'd rather…"
                aria-label="Other answer"
                data-testid="review-question-other"
                autoFocus
                value={otherText[idx] ?? ''}
                disabled={submitting}
                onChange={(e) => setOtherText((p) => ({ ...p, [idx]: e.target.value }))}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && questions.length === 1) submit()
                }}
              />
            )}
          </div>
        )
      })}

      {error && (
        <p className="form-error review-ask__error" role="alert">
          {error}
        </p>
      )}

      <div className="review-ask__actions">
        <button
          type="button"
          className="review-ask__dismiss"
          disabled={submitting}
          onClick={dismiss}
        >
          Dismiss
        </button>
        <button
          type="button"
          className="btn-primary review-ask__send"
          data-testid="review-question-submit"
          disabled={!answered || submitting}
          onClick={submit}
        >
          {submitting ? 'Sending…' : 'Answer'}
        </button>
      </div>
    </section>
  )
}
