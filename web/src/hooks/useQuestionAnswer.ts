import { useState } from 'react'
import { authedFetch } from '../store/auth'
import { type AnswerValue, answerText, toggleOption } from '../lib/questionAnswers'
import type { QuestionItem } from '../components/chat/events'

/** The open question a card or modal answers, and where the answer goes. */
export interface QuestionTarget {
  sessionId: string
  /** Event id of the `question` event; echoed back as `question_id`. */
  questionId: string
  /** ControlRequest correlation id, echoed back as `request_id`. */
  requestId?: string
  questions: QuestionItem[]
}

/**
 * Answer state and the `question-resolved` POST for an `ask_user` /
 * AskUserQuestion prompt. Shared by the chat's centered modal and the
 * inline card so the two can never drift in what they send.
 */
export function useQuestionAnswer({ sessionId, questionId, requestId, questions }: QuestionTarget) {
  const [answers, setAnswers] = useState<Record<number, AnswerValue>>({})
  const [submitting, setSubmitting] = useState(false)

  const setAnswer = (idx: number, value: string) => {
    setAnswers((prev) => ({ ...prev, [idx]: value }))
  }

  const toggleMulti = (idx: number, option: string) =>
    setAnswers((prev) => ({ ...prev, [idx]: toggleOption(prev[idx], option) }))

  const hasAnswers = questions.some((_, idx) => answerText(answers[idx]).length > 0)

  const post = async (data: Record<string, unknown>) => {
    setSubmitting(true)
    try {
      await authedFetch(`/api/sessions/${sessionId}/events`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          kind: 'question-resolved',
          data: {
            question_id: questionId,
            ...(requestId ? { request_id: requestId } : {}),
            ...data,
          },
        }),
      })
    } finally {
      setSubmitting(false)
    }
  }

  const submit = async () => {
    if (!hasAnswers || submitting) return
    const answerMap: Record<string, string> = {}
    questions.forEach((_, idx) => {
      const val = answerText(answers[idx])
      if (val) answerMap[String(idx)] = val
    })
    await post({ answers: answerMap })
  }

  /** Reject the question outright — the agent gets `rejected: true`. */
  const dismiss = async () => {
    if (submitting) return
    await post({ rejected: true })
  }

  return { answers, setAnswer, toggleMulti, hasAnswers, submitting, submit, dismiss }
}
