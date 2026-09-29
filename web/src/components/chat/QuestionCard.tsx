import Modal from '../Modal'
import { selectedOptions } from '../../lib/questionAnswers'
import { type QuestionTarget, useQuestionAnswer } from '../../hooks/useQuestionAnswer'
import type { QuestionItem } from './events'

/**
 * The chat's `ask_user` / AskUserQuestion surfaces.
 *
 * `useQuestionAnswer` owns the answer state and the `question-resolved`
 * POST, and `QuestionForm` owns the fields, so the centered modal and the
 * inline card render the same thing. The modal is the default for the
 * session being viewed; the inline card is for read-only echoes (the
 * native subagent pane) where a modal would be wrong.
 */

/** Title bar + fields + Submit / Dismiss. Shared by the modal and the inline card. */
function QuestionForm(target: QuestionTarget) {
  const { questionId, questions } = target
  const { answers, setAnswer, toggleMulti, hasAnswers, submitting, submit, dismiss } =
    useQuestionAnswer(target)

  return (
    <>
      <div className="question-card-title-bar">
        <span className="question-card-icon">&#x2753;</span>
        <span className="question-card-title-text" data-dialog-title>
          Input needed
        </span>
      </div>
      {questions.map((q, idx) => (
        <div key={idx} className="question-item">
          {q.header && <div className="question-header">{q.header}</div>}
          <div className="question-card-text">{q.question}</div>
          {q.options && q.options.length > 0 ? (
            <div className="question-options">
              {q.options.map((opt, optIdx) => {
                const optObj = q.optionObjects?.[optIdx]
                return (
                  <label key={opt} className="question-option-label">
                    {q.multiSelect ? (
                      <input
                        type="checkbox"
                        checked={selectedOptions(answers[idx]).includes(opt)}
                        onChange={() => toggleMulti(idx, opt)}
                        disabled={submitting}
                      />
                    ) : (
                      <input
                        type="radio"
                        name={`question-${questionId}-${idx}`}
                        checked={answers[idx] === opt}
                        onChange={() => setAnswer(idx, opt)}
                        disabled={submitting}
                      />
                    )}
                    <span className="question-option-text">
                      <span className="question-option-label-text">{opt}</span>
                      {optObj?.description && (
                        <span className="question-option-desc">{optObj.description}</span>
                      )}
                    </span>
                  </label>
                )
              })}
            </div>
          ) : (
            <input
              className="question-input"
              type="text"
              placeholder="Type your answer..."
              value={typeof answers[idx] === 'string' ? answers[idx] : ''}
              onChange={(e) => setAnswer(idx, e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && questions.length === 1) void submit()
              }}
              disabled={submitting}
            />
          )}
        </div>
      ))}
      <div className="question-actions">
        <button
          className="btn-primary"
          onClick={() => void submit()}
          disabled={!hasAnswers || submitting}
        >
          Submit
        </button>
        <button className="btn-secondary" onClick={() => void dismiss()} disabled={submitting}>
          Dismiss
        </button>
      </div>
    </>
  )
}

/** The question inline in a feed (read-only echoes such as subagent panes). */
export function QuestionCard(target: QuestionTarget) {
  return (
    <div className="question-card question-active">
      <QuestionForm {...target} />
    </div>
  )
}

/**
 * The centered take on the same card. `onHide` is Escape / backdrop: it
 * only puts the modal away — the question stays open and the feed keeps
 * an "Answer" row to bring it back. Only "Dismiss" rejects.
 */
export function QuestionModal({ onHide, ...target }: QuestionTarget & { onHide: () => void }) {
  return (
    <Modal
      onClose={onHide}
      role="alertdialog"
      maxWidth={520}
      className="question-modal"
      data-testid="question-modal"
    >
      {/* Keyed on the question so a new question never inherits the
          half-typed answer of the one it replaced. */}
      <div key={target.questionId} className="question-card question-active">
        <QuestionForm {...target} />
      </div>
    </Modal>
  )
}

/** The feed's placeholder while the modal is hidden or another pane has it. */
export function PendingQuestionRow({
  questions,
  onAnswer,
}: {
  questions: QuestionItem[]
  onAnswer: () => void
}) {
  const preview = questions[0]?.question ?? ''
  return (
    <div className="question-pending-row" data-testid="question-pending-row">
      <span className="question-card-icon" aria-hidden="true">
        &#x2753;
      </span>
      <span className="question-pending-title">Input needed</span>
      {preview && (
        <span className="question-pending-preview" title={preview}>
          {preview}
        </span>
      )}
      <button type="button" className="btn-primary question-pending-answer" onClick={onAnswer}>
        Answer
      </button>
    </div>
  )
}

export function ResolvedQuestionCard({
  questions,
  answers,
}: {
  questions: QuestionItem[]
  answers: Record<string, unknown>
}) {
  return (
    <div className="question-card question-resolved">
      <div className="question-card-title-bar">
        <span className="question-card-icon">&#x2611;&#xFE0F;</span>
        <span className="question-card-title-text">Question answered</span>
      </div>
      {questions.map((q, idx) => {
        const answer = String(
          answers[idx] ?? answers[String(idx)] ?? answers[q.question] ?? '(no answer)',
        )
        return (
          <div key={idx} className="question-item">
            {q.header && <div className="question-header">{q.header}</div>}
            <div className="question-card-text">{q.question}</div>
            <div className="question-answer-display">{answer}</div>
          </div>
        )
      })}
    </div>
  )
}
