import { useState } from 'react'

import type { QuestionItem } from '../chat/events'
import { authedFetch } from '../../store/auth'
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
  /** `pinned` sits above the document; `inline` is the chat lane's echo. */
  variant?: 'pinned' | 'inline'
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
 * The same card is pinned above the document and echoed in the chat lane —
 * one component, so an answer given in either place submits the identical
 * `question-resolved` event the main chat posts (`{question_id, request_id?,
 * answers}` keyed by question index). The backend's resolution hook is what
 * walks the review from `needs input` back to `running`; this only has to
 * get the payload right.
 */
export default function QuestionCard({
  sessionId,
  questionId,
  requestId,
  questions,
  variant = 'pinned',
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
      <div
        className={`question-card question-resolved review-question review-question--${variant}`}
        data-testid="review-question-answered"
      >
        <div className="question-card-title-bar">
          <span className="question-card-icon">&#x2611;&#xFE0F;</span>
          <span className="question-card-title-text">Answer sent</span>
        </div>
        {questions.map((q, idx) => (
          <div key={idx} className="question-item">
            <div className="question-card-text">{q.question}</div>
            <div className="question-answer-display">{submitted[String(idx)] ?? 'dismissed'}</div>
          </div>
        ))}
      </div>
    )
  }

  return (
    <div
      className={`question-card question-active review-question review-question--${variant}`}
      data-testid="review-question-card"
    >
      <div className="question-card-title-bar">
        <span className="question-card-icon">&#x2753;</span>
        <span className="question-card-title-text">The reviewer needs an answer</span>
      </div>

      {questions.map((q, idx) => {
        const selected = (answers[idx] ?? '').split(',')
        const otherSelected = selected.some(isOtherOption)
        return (
          <div key={idx} className="question-item">
            {q.header && <div className="question-header">{q.header}</div>}
            <div className="question-card-text">{q.question}</div>
            {q.options && q.options.length > 0 ? (
              <div className="question-options">
                {q.options.map((opt, optIdx) => {
                  const desc = q.optionObjects?.[optIdx]?.description
                  return (
                    <label key={opt} className="question-option-label">
                      {q.multiSelect ? (
                        <input
                          type="checkbox"
                          checked={selected.includes(opt)}
                          onChange={() => toggleMulti(idx, opt)}
                          disabled={submitting}
                        />
                      ) : (
                        <input
                          type="radio"
                          name={`review-question-${questionId}-${idx}`}
                          checked={answers[idx] === opt}
                          onChange={() => setAnswer(idx, opt)}
                          disabled={submitting}
                        />
                      )}
                      <span className="question-option-text">
                        <span className="question-option-label-text">{opt}</span>
                        {desc && <span className="question-option-desc">{desc}</span>}
                      </span>
                    </label>
                  )
                })}
                {otherSelected && (
                  <input
                    className="question-input"
                    type="text"
                    placeholder="Say what you'd rather…"
                    aria-label="Other answer"
                    data-testid="review-question-other"
                    value={otherText[idx] ?? ''}
                    disabled={submitting}
                    onChange={(e) => setOtherText((p) => ({ ...p, [idx]: e.target.value }))}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' && questions.length === 1) submit()
                    }}
                  />
                )}
              </div>
            ) : (
              <input
                className="question-input"
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
          </div>
        )
      })}

      {error && (
        <p className="form-error review-question__error" role="alert">
          {error}
        </p>
      )}

      <div className="question-actions">
        <button
          type="button"
          className="btn-primary"
          data-testid="review-question-submit"
          disabled={!answered || submitting}
          onClick={submit}
        >
          {submitting ? 'Sending…' : 'Answer'}
        </button>
        <button type="button" className="btn-secondary" disabled={submitting} onClick={dismiss}>
          Dismiss
        </button>
      </div>
    </div>
  )
}
