import { useEffect, useRef, useState } from 'react'
import { useProjectsStore, type PendingQuestion } from '../store/projects'
import { useResourcesStore } from '../store/resources'
import { useWsStore } from '../store/ws'
import { authedFetch } from '../store/auth'
import { useMentions, filterMentions } from '../hooks/useMentions'
import type { Card, Event } from '../types/api'
import EditCardModal from './EditCardModal'
import WorkerComms from './WorkerComms'
import ProjectTodoSummary from './ProjectTodoSummary'
import { useProjectTodos } from '../hooks/useProjectTodos'
import {
  EFFORT_OPTIONS,
  EMPTY_QUESTIONS,
  EMPTY_REPORTS,
  STEPS,
  THOUGHT_BUBBLE_MS,
  type PriorityInfo,
  type ThoughtBubble,
  priorityBadge,
  summarizeEvent,
} from './kanban/utils'

interface KanbanBoardProps {
  projectId: string
}

export default function KanbanBoard({ projectId }: KanbanBoardProps) {
  const projects = useProjectsStore((s) => s.projects)
  const updateProject = useProjectsStore((s) => s.updateProject)
  const cards = useProjectsStore((s) => s.cards)
  const fetchCards = useProjectsStore((s) => s.fetchCards)
  const createCard = useProjectsStore((s) => s.createCard)
  const updateCard = useProjectsStore((s) => s.updateCard)
  const deleteCard = useProjectsStore((s) => s.deleteCard)

  const project = projects.find((p) => p.id === projectId)
  // cardId -> latest todo snapshot for that card's worker session.
  const todosByCard = useProjectTodos(cards)
  const [selectedCard, setSelectedCard] = useState<Card | null>(null)
  const [showAddForm, setShowAddForm] = useState(false)
  const [addTitle, setAddTitle] = useState('')
  const [addDescription, setAddDescription] = useState('')
  const [addPriority, setAddPriority] = useState(2)
  const [addWorkflow, setAddWorkflow] = useState('')
  const [addModel, setAddModel] = useState('')
  const [addEffort, setAddEffort] = useState('')
  const [addDependsOn, setAddDependsOn] = useState<string[]>([])
  const [addSubmitting, setAddSubmitting] = useState(false)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)
  const [draggingCardId, setDraggingCardId] = useState<string | null>(null)
  const [dragOverStep, setDragOverStep] = useState<string | null>(null)
  const [cardMenuId, setCardMenuId] = useState<string | null>(null)
  const [editingCard, setEditingCard] = useState<Card | null>(null)
  const [showComms, setShowComms] = useState(false)
  // One transient thought bubble per card, keyed by card id.
  const [bubbles, setBubbles] = useState<Record<string, ThoughtBubble>>({})
  const bubbleTimers = useRef<Record<string, ReturnType<typeof setTimeout>>>({})
  // Latest session→card map, read inside the (stable) event listener.
  const sessionToCardRef = useRef<Record<string, string>>({})
  const cardReports = useProjectsStore((s) =>
    selectedCard ? (s.cardReportsByCard[selectedCard.id] ?? EMPTY_REPORTS) : EMPTY_REPORTS,
  )
  const fetchCardReports = useProjectsStore((s) => s.fetchCardReports)

  // Fetch reports when card detail is opened
  useEffect(() => {
    if (!selectedCard) return
    fetchCardReports(projectId, selectedCard.id)
  }, [selectedCard, projectId, fetchCardReports])

  const workflows = useResourcesStore((s) => s.workflows)
  const models = useResourcesStore((s) => s.models)
  const allMentions = useMentions()
  const [mentionAutocomplete, setMentionAutocomplete] = useState<{
    eventId: string
    idx: number
    suggestions: ReturnType<typeof filterMentions>
  } | null>(null)
  const [priorities, setPriorities] = useState<PriorityInfo[]>([
    { label: 'Critical', value: 0, description: 'Blocks everything' },
    { label: 'High', value: 1, description: 'Important' },
    { label: 'Medium', value: 2, description: 'Normal' },
    { label: 'Low', value: 3, description: 'Nice to have' },
    { label: 'Backlog', value: 4, description: 'Someday' },
  ])
  const pendingQuestions = useProjectsStore(
    (s) => s.pendingQuestionsByProject[projectId] ?? EMPTY_QUESTIONS,
  )
  const fetchPendingQuestions = useProjectsStore((s) => s.fetchPendingQuestions)
  const [questionAnswers, setQuestionAnswers] = useState<Record<string, Record<number, string>>>({})
  const [submittingQuestion, setSubmittingQuestion] = useState<string | null>(null)
  const [questionDialogOpen, setQuestionDialogOpen] = useState(false)

  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)
  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)

  // Fetch pending questions on mount and when events arrive
  useEffect(() => {
    fetchPendingQuestions(projectId)
  }, [fetchPendingQuestions, projectId])

  // Listen for WebSocket events to refresh pending questions
  useEffect(() => {
    const listener = (event: Event) => {
      if (event.kind === 'question' || event.kind === 'question-resolved') {
        fetchPendingQuestions(projectId)
      }
    }
    addEventListener(listener)
    return () => removeEventListener(listener)
  }, [addEventListener, removeEventListener, fetchPendingQuestions, projectId])

  // Listen for worker-question events to refresh pending questions live
  useEffect(() => {
    const handler = (e: globalThis.Event) => {
      const detail = (e as CustomEvent).detail
      const pid = detail?.data?.projectId as string | undefined
      if (pid === projectId) {
        fetchPendingQuestions(projectId)
      }
    }
    window.addEventListener('peckboard:worker-question', handler)
    return () => window.removeEventListener('peckboard:worker-question', handler)
  }, [projectId, fetchPendingQuestions])

  // Listen for card-update WebSocket events for live kanban updates
  useEffect(() => {
    const handler = (e: globalThis.Event) => {
      const detail = (e as CustomEvent).detail
      const card = detail?.data?.card as Card | undefined
      if (!card || card.project_id !== projectId) return
      // Update the card in place in the store
      useProjectsStore.setState((s) => {
        const exists = s.cards.some((c) => c.id === card.id)
        if (exists) {
          return { cards: s.cards.map((c) => (c.id === card.id ? card : c)) }
        } else {
          return { cards: [...s.cards, card] }
        }
      })
    }
    window.addEventListener('peckboard:card-update', handler)
    return () => window.removeEventListener('peckboard:card-update', handler)
  }, [projectId])

  // Listen for card-delete WebSocket events
  useEffect(() => {
    const handler = (e: globalThis.Event) => {
      const detail = (e as CustomEvent).detail
      const cardId = detail?.data?.cardId as string | undefined
      const pid = detail?.data?.projectId as string | undefined
      if (!cardId || pid !== projectId) return
      useProjectsStore.setState((s) => ({
        cards: s.cards.filter((c) => c.id !== cardId),
      }))
    }
    window.addEventListener('peckboard:card-delete', handler)
    return () => window.removeEventListener('peckboard:card-delete', handler)
  }, [projectId])

  // Keep a live session→card lookup for the thought-bubble listener.
  useEffect(() => {
    const map: Record<string, string> = {}
    for (const c of cards) {
      if (c.worker_session_id) map[c.worker_session_id] = c.id
    }
    sessionToCardRef.current = map
  }, [cards])

  // Subscribe to every worker session so its events stream in even when no one
  // has the session's chat open. Include `last_worker_session_id` so the todo
  // aggregate keeps receiving live `todo` events after a worker finishes a
  // chunk (the orchestrator clears `worker_session_id` between dispatches). The
  // key is the sorted, deduped set of session ids, so this only re-runs when a
  // worker is added or removed.
  const workerSessionKey = [
    ...new Set(
      cards
        .flatMap((c) => [c.worker_session_id, c.last_worker_session_id])
        .filter((id): id is string => Boolean(id)),
    ),
  ]
    .sort()
    .join(',')
  useEffect(() => {
    const ids = workerSessionKey ? workerSessionKey.split(',') : []
    ids.forEach((id) => subscribe(id))
    return () => ids.forEach((id) => unsubscribe(id))
  }, [workerSessionKey, subscribe, unsubscribe])

  // Show a transient thought bubble on a card whenever its worker emits an
  // event. Only one bubble per card; a newer event replaces the current one
  // and resets the 5s dismissal timer.
  useEffect(() => {
    const timers = bubbleTimers.current
    const listener = (event: Event) => {
      const cardId = sessionToCardRef.current[event.session_id]
      if (!cardId) return
      const text = summarizeEvent(event)
      if (!text) return
      setBubbles((prev) => ({ ...prev, [cardId]: { text, key: event.seq } }))
      if (timers[cardId]) clearTimeout(timers[cardId])
      timers[cardId] = setTimeout(() => {
        delete timers[cardId]
        setBubbles((prev) => {
          if (!(cardId in prev)) return prev
          const next = { ...prev }
          delete next[cardId]
          return next
        })
      }, THOUGHT_BUBBLE_MS)
    }
    addEventListener(listener)
    return () => {
      removeEventListener(listener)
      Object.values(timers).forEach(clearTimeout)
      bubbleTimers.current = {}
    }
  }, [addEventListener, removeEventListener])

  const handleAnswerQuestion = async (pq: PendingQuestion) => {
    const answers = questionAnswers[pq.eventId] ?? {}
    const hasAnswers = pq.questions.some((_, idx) => (answers[idx] ?? '').trim().length > 0)
    if (!hasAnswers || submittingQuestion) return

    setSubmittingQuestion(pq.eventId)
    try {
      const answerMap: Record<string, string> = {}
      pq.questions.forEach((_, idx) => {
        const val = (answers[idx] ?? '').trim()
        if (val) answerMap[String(idx)] = val
      })
      await authedFetch(`/api/sessions/${pq.sessionId}/events`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          kind: 'question-resolved',
          data: { question_id: pq.eventId, answers: answerMap },
        }),
      })
      // Clear answers and refresh
      setQuestionAnswers((prev) => {
        const next = { ...prev }
        delete next[pq.eventId]
        return next
      })
      fetchPendingQuestions(projectId)
    } finally {
      setSubmittingQuestion(null)
    }
  }

  const handleDismissQuestion = async (pq: PendingQuestion) => {
    if (submittingQuestion) return
    setSubmittingQuestion(pq.eventId)
    try {
      await authedFetch(`/api/sessions/${pq.sessionId}/events`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          kind: 'question-resolved',
          data: { question_id: pq.eventId, rejected: true },
        }),
      })
      fetchPendingQuestions(projectId)
    } finally {
      setSubmittingQuestion(null)
    }
  }

  const setQuestionAnswer = (eventId: string, idx: number, value: string) => {
    setQuestionAnswers((prev) => ({
      ...prev,
      [eventId]: { ...(prev[eventId] ?? {}), [idx]: value },
    }))

    // Check for @ autocomplete trigger
    const atMatch = value.match(/@(\S*)$/)
    if (atMatch && allMentions.length > 0) {
      const filtered = filterMentions(allMentions, atMatch[1])
      if (filtered.length > 0) {
        setMentionAutocomplete({ eventId, idx, suggestions: filtered })
      } else {
        setMentionAutocomplete(null)
      }
    } else {
      setMentionAutocomplete(null)
    }
  }

  const insertMentionInAnswer = (eventId: string, idx: number, mention: { ref: string }) => {
    const current = questionAnswers[eventId]?.[idx] ?? ''
    const atIdx = current.lastIndexOf('@')
    const newVal = current.slice(0, atIdx) + mention.ref
    setQuestionAnswers((prev) => ({
      ...prev,
      [eventId]: { ...(prev[eventId] ?? {}), [idx]: newVal },
    }))
    setMentionAutocomplete(null)
  }

  const toggleQuestionMulti = (eventId: string, idx: number, option: string) => {
    setQuestionAnswers((prev) => {
      const current = (prev[eventId] ?? {})[idx] ?? ''
      const selected = current ? current.split(',') : []
      const next = selected.includes(option)
        ? selected.filter((s) => s !== option)
        : [...selected, option]
      return { ...prev, [eventId]: { ...(prev[eventId] ?? {}), [idx]: next.join(',') } }
    })
  }

  useEffect(() => {
    fetchCards(projectId)
  }, [projectId, fetchCards])

  const fetchWorkflows = useResourcesStore((s) => s.fetchWorkflows)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  useEffect(() => {
    fetchWorkflows()
    fetchModels()
    authedFetch('/api/priorities')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.priorities) setPriorities(data.priorities)
      })
      .catch(() => {})
  }, [fetchWorkflows, fetchModels])

  // Map step aliases to canonical step names for display
  const normalizeStep = (step: string) => {
    switch (step) {
      case 'todo':
        return 'backlog'
      case 'in-progress':
        return 'in_progress'
      case 'wont-do':
        return 'wont_do'
      default:
        return step
    }
  }
  const cardsByStep = (step: string) => cards.filter((c) => normalizeStep(c.step) === step)

  // Dependency ids resolve to titles/steps via this lookup so cards can
  // show which prerequisites are still outstanding.
  const cardById = new Map(cards.map((c) => [c.id, c]))
  // A dependency is satisfied only when the prerequisite card is `done`
  // (matches the backend gate). Deleted prerequisites resolve to undefined
  // and no longer block.
  const unmetDeps = (card: Card): Card[] =>
    (card.depends_on ?? [])
      .map((id) => cardById.get(id))
      .filter((c): c is Card => c != null && normalizeStep(c.step) !== 'done')

  const handleAddCard = async () => {
    if (!addTitle.trim() || addSubmitting) return
    setAddSubmitting(true)
    try {
      await createCard(projectId, {
        title: addTitle.trim(),
        description: addDescription.trim(),
        step: 'backlog',
        priority: addPriority,
        workflow: addWorkflow || undefined,
        model: addModel || undefined,
        effort: addEffort || undefined,
        depends_on: addDependsOn.length > 0 ? addDependsOn : undefined,
      } as Partial<Card>)
      setAddTitle('')
      setAddDescription('')
      setAddPriority(2)
      setAddWorkflow('')
      setAddModel('')
      setAddEffort('')
      setAddDependsOn([])
      setShowAddForm(false)
    } catch {
      /* ignore */
    } finally {
      setAddSubmitting(false)
    }
  }

  const handleDeleteCard = async (cardId: string) => {
    try {
      await deleteCard(projectId, cardId)
      setSelectedCard(null)
      setConfirmDeleteId(null)
      setCardMenuId(null)
    } catch {
      /* ignore */
    }
  }

  const handleViewSession = (sessionId: string) => {
    setCardMenuId(null)
    setSelectedCard(null)
    // location.assign mutates via a method call (not a property write),
    // which the immutability lint accepts.
    window.location.assign(`/sessions/${sessionId}`)
  }

  const handleStopWorker = async (card: Card) => {
    setCardMenuId(null)
    await authedFetch(`/api/projects/${projectId}/cards/${card.id}/stop`, { method: 'POST' })
    fetchCards(projectId)
  }

  const handleRestartWorker = async (card: Card) => {
    setCardMenuId(null)
    await authedFetch(`/api/projects/${projectId}/cards/${card.id}/restart`, { method: 'POST' })
    fetchCards(projectId)
  }

  const handleCancelWontDo = async (card: Card) => {
    setCardMenuId(null)
    await authedFetch(`/api/projects/${projectId}/cards/${card.id}/cancel-wont-do`, {
      method: 'POST',
    })
    fetchCards(projectId)
  }

  const handleDragStart = (e: React.DragEvent, card: Card) => {
    e.dataTransfer.setData('cardId', card.id)
    e.dataTransfer.setData('fromStep', card.step)
    e.dataTransfer.effectAllowed = 'move'
    setDraggingCardId(card.id)
  }

  const handleDragEnd = () => {
    setDraggingCardId(null)
    setDragOverStep(null)
  }

  const handleDragOver = (e: React.DragEvent, stepKey: string) => {
    e.preventDefault()
    e.dataTransfer.dropEffect = 'move'
    setDragOverStep(stepKey)
  }

  const handleDragLeave = (e: React.DragEvent, stepKey: string) => {
    const related = e.relatedTarget as Node | null
    const current = e.currentTarget as Node
    if (!related || !current.contains(related)) {
      if (dragOverStep === stepKey) setDragOverStep(null)
    }
  }

  const handleDrop = async (e: React.DragEvent, targetStep: string) => {
    e.preventDefault()
    setDragOverStep(null)
    const cardId = e.dataTransfer.getData('cardId')
    const fromStep = e.dataTransfer.getData('fromStep')
    if (!cardId || fromStep === targetStep) return

    useProjectsStore.setState((s) => ({
      cards: s.cards.map((c) => (c.id === cardId ? { ...c, step: targetStep } : c)),
    }))

    try {
      await updateCard(projectId, cardId, { step: targetStep })
    } catch {
      fetchCards(projectId)
    }
  }

  // Only show wont_do column if there are cards in it
  const visibleSteps = STEPS.filter((s) => s.key !== 'wont_do' || cardsByStep('wont_do').length > 0)

  if (showComms) {
    return <WorkerComms projectId={projectId} onClose={() => setShowComms(false)} />
  }

  return (
    <div className="kanban-board">
      <div className="kanban-board-header">
        {project && (
          <div className="kanban-project-info">
            <h2 className="kanban-project-name">{project.name}</h2>
            <span className={`status-badge status-${project.status}`}>{project.status}</span>
            <button
              className={
                project.status === 'paused' ? 'btn-primary btn-sm' : 'btn-secondary btn-sm'
              }
              onClick={() =>
                updateProject(projectId, {
                  status: project.status === 'paused' ? 'active' : 'paused',
                } as Record<string, unknown>)
              }
            >
              {project.status === 'paused' ? 'Resume' : 'Pause'}
            </button>
          </div>
        )}
        <button
          className="btn-secondary"
          onClick={() => setShowComms(true)}
          title="View worker communications"
        >
          Comms
        </button>
        <button className="btn-primary" onClick={() => setShowAddForm(!showAddForm)}>
          {showAddForm ? 'Cancel' : 'Add Card'}
        </button>
      </div>

      {/* Project-level todo roll-up across every card's worker session. */}
      <ProjectTodoSummary todosByCard={todosByCard} />

      {/* Pending worker questions — trigger button */}
      {pendingQuestions.length > 0 && !questionDialogOpen && (
        <button className="worker-questions-trigger" onClick={() => setQuestionDialogOpen(true)}>
          <span className="worker-questions-icon">&#x2753;</span>
          <span>
            {pendingQuestions.length} worker question{pendingQuestions.length > 1 ? 's' : ''} need
            your answer
          </span>
        </button>
      )}

      {/* Question dialog — shows first pending question */}
      {questionDialogOpen &&
        pendingQuestions.length > 0 &&
        (() => {
          const pq = pendingQuestions[0]
          const answers = questionAnswers[pq.eventId] ?? {}
          const isSubmitting = submittingQuestion === pq.eventId
          const hasAnswers = pq.questions.some((_, idx) => (answers[idx] ?? '').trim().length > 0)
          const remaining = pendingQuestions.length - 1

          return (
            <div className="modal-backdrop" onClick={() => setQuestionDialogOpen(false)}>
              <div
                className="modal question-dialog"
                onClick={(e) => e.stopPropagation()}
                style={{ maxWidth: 520 }}
              >
                {/* Header with context */}
                <div className="question-dialog-header">
                  <div className="question-dialog-counter">
                    {remaining > 0
                      ? `${pendingQuestions.length} questions remaining`
                      : 'Last question'}
                  </div>
                  {pq.cardTitle && (
                    <div className="question-dialog-context">
                      <span className="question-dialog-card-label">Card:</span>
                      <span className="question-dialog-card-title">{pq.cardTitle}</span>
                    </div>
                  )}
                  {pq.cardDescription && (
                    <div className="question-dialog-card-desc">{pq.cardDescription}</div>
                  )}
                </div>

                {/* Question content */}
                <div className="question-dialog-body">
                  {pq.questions.map((q, idx) => (
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
                                    checked={(answers[idx] ?? '').split(',').includes(opt)}
                                    onChange={() => toggleQuestionMulti(pq.eventId, idx, opt)}
                                    disabled={isSubmitting}
                                  />
                                ) : (
                                  <input
                                    type="radio"
                                    name={`wq-${pq.eventId}-${idx}`}
                                    checked={answers[idx] === opt}
                                    onChange={() => setQuestionAnswer(pq.eventId, idx, opt)}
                                    disabled={isSubmitting}
                                  />
                                )}
                                <span className="question-option-text">
                                  <span>{opt}</span>
                                  {optObj?.description && (
                                    <span className="question-option-desc">
                                      {optObj.description}
                                    </span>
                                  )}
                                </span>
                              </label>
                            )
                          })}
                        </div>
                      ) : (
                        <div className="question-input-wrapper">
                          <input
                            className="question-input"
                            type="text"
                            placeholder="Type your answer... (@ to reference a report)"
                            value={answers[idx] ?? ''}
                            onChange={(e) => setQuestionAnswer(pq.eventId, idx, e.target.value)}
                            onKeyDown={(e) => {
                              if (
                                e.key === 'Enter' &&
                                pq.questions.length === 1 &&
                                hasAnswers &&
                                !mentionAutocomplete
                              )
                                handleAnswerQuestion(pq)
                            }}
                            onBlur={() => setTimeout(() => setMentionAutocomplete(null), 200)}
                            disabled={isSubmitting}
                            autoFocus
                          />
                          {mentionAutocomplete &&
                            mentionAutocomplete.eventId === pq.eventId &&
                            mentionAutocomplete.idx === idx && (
                              <div className="autocomplete-dropdown autocomplete-inline">
                                <div className="autocomplete-header">
                                  @ — reports &amp; sessions
                                </div>
                                {mentionAutocomplete.suggestions.map((m, i) => (
                                  <button
                                    key={`${m.type}-${m.detail}-${i}`}
                                    className="autocomplete-item"
                                    onMouseDown={(e) => {
                                      e.preventDefault()
                                      insertMentionInAnswer(pq.eventId, idx, m)
                                    }}
                                  >
                                    <span className="autocomplete-item-title">
                                      <span
                                        className={`autocomplete-type-badge autocomplete-type-${m.type}`}
                                      >
                                        {m.type}
                                      </span>
                                      {m.label}
                                    </span>
                                    <span className="autocomplete-item-path">{m.detail}</span>
                                  </button>
                                ))}
                              </div>
                            )}
                        </div>
                      )}
                    </div>
                  ))}
                </div>

                {/* Footer */}
                <div className="question-dialog-footer">
                  <div className="question-dialog-left-actions">
                    <button className="btn-secondary" onClick={() => setQuestionDialogOpen(false)}>
                      Answer Later
                    </button>
                    <button
                      className="btn-secondary btn-danger-text"
                      onClick={async () => {
                        await handleDismissQuestion(pq)
                        if (pendingQuestions.length <= 1) setQuestionDialogOpen(false)
                      }}
                      disabled={isSubmitting}
                    >
                      Dismiss
                    </button>
                  </div>
                  <button
                    className="btn-primary"
                    onClick={async () => {
                      await handleAnswerQuestion(pq)
                      if (pendingQuestions.length <= 1) setQuestionDialogOpen(false)
                    }}
                    disabled={!hasAnswers || isSubmitting}
                  >
                    {isSubmitting ? 'Submitting...' : 'Submit'}
                  </button>
                </div>
              </div>
            </div>
          )
        })()}

      {showAddForm && (
        <div className="kanban-add-form">
          <input
            type="text"
            placeholder="Card title"
            value={addTitle}
            onChange={(e) => setAddTitle(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) handleAddCard()
            }}
            autoFocus
          />
          <textarea
            placeholder="Description"
            value={addDescription}
            onChange={(e) => setAddDescription(e.target.value)}
            rows={2}
          />
          <div className="kanban-add-row">
            <select value={addPriority} onChange={(e) => setAddPriority(Number(e.target.value))}>
              {priorities.map((p) => (
                <option key={p.value} value={p.value}>
                  {p.label}
                </option>
              ))}
            </select>
            <select value={addWorkflow} onChange={(e) => setAddWorkflow(e.target.value)}>
              <option value="">Default workflow</option>
              {workflows.map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
            <select value={addModel} onChange={(e) => setAddModel(e.target.value)}>
              <option value="">Default model</option>
              {models.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.display_name}
                </option>
              ))}
            </select>
            <select value={addEffort} onChange={(e) => setAddEffort(e.target.value)}>
              {EFFORT_OPTIONS.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label} effort
                </option>
              ))}
            </select>
          </div>
          {cards.length > 0 && (
            <div className="kanban-add-deps">
              <span className="kanban-add-deps-label">
                Depends on (starts after these are done):
              </span>
              <div className="kanban-deps-options">
                {cards.map((c) => (
                  <label key={c.id} className="kanban-dep-option">
                    <input
                      type="checkbox"
                      checked={addDependsOn.includes(c.id)}
                      onChange={(e) =>
                        setAddDependsOn((prev) =>
                          e.target.checked ? [...prev, c.id] : prev.filter((id) => id !== c.id),
                        )
                      }
                    />
                    <span>{c.title}</span>
                  </label>
                ))}
              </div>
            </div>
          )}
          <button
            className="btn-primary"
            onClick={handleAddCard}
            disabled={!addTitle.trim() || addSubmitting}
          >
            {addSubmitting ? 'Creating...' : 'Create Card'}
          </button>
        </div>
      )}

      <div className="kanban-columns">
        {visibleSteps.map((step) => (
          <div
            key={step.key}
            className={`kanban-column${dragOverStep === step.key ? ' drag-over' : ''}`}
            onDragOver={(e) => handleDragOver(e, step.key)}
            onDragLeave={(e) => handleDragLeave(e, step.key)}
            onDrop={(e) => handleDrop(e, step.key)}
          >
            <div className="kanban-column-header">
              <h3>{step.label}</h3>
              <span className="kanban-count">{cardsByStep(step.key).length}</span>
            </div>
            <div className="kanban-cards">
              {cardsByStep(step.key).map((card) => (
                <div
                  key={`${card.id}-${card.step}`}
                  className={`kanban-card ${card.blocked ? 'blocked' : ''}${draggingCardId === card.id ? ' dragging' : ''}`}
                  draggable={true}
                  onDragStart={(e) => handleDragStart(e, card)}
                  onDragEnd={handleDragEnd}
                >
                  {bubbles[card.id] && (
                    <div
                      key={bubbles[card.id].key}
                      className="card-thought-bubble"
                      title={bubbles[card.id].text}
                    >
                      {bubbles[card.id].text}
                    </div>
                  )}
                  <div className="kanban-card-top" onClick={() => setSelectedCard(card)}>
                    <div className="kanban-card-header">
                      <span className="kanban-card-title">{card.title}</span>
                      {(() => {
                        const todos = todosByCard[card.id]
                        if (!todos || todos.length === 0) return null
                        const done = todos.filter((t) => t.status === 'done').length
                        return (
                          <span
                            className="card-todo-badge"
                            data-testid="card-todo-badge"
                            title={`${done} of ${todos.length} tasks done`}
                          >
                            {done}/{todos.length}
                          </span>
                        )
                      })()}
                      {priorityBadge(card.priority, priorities)}
                    </div>
                    {card.blocked && (
                      <div className="blocked-indicator">
                        Blocked{card.block_reason ? `: ${card.block_reason}` : ''}
                      </div>
                    )}
                    {!card.worker_session_id &&
                      card.step !== 'done' &&
                      card.step !== 'wont_do' &&
                      (() => {
                        const pending = unmetDeps(card)
                        if (pending.length === 0) return null
                        return (
                          <div
                            className="waiting-indicator"
                            title={`Waiting on: ${pending.map((c) => c.title).join(', ')}`}
                          >
                            Waiting on {pending.length}{' '}
                            {pending.length === 1 ? 'dependency' : 'dependencies'}
                          </div>
                        )
                      })()}
                    {card.worker_session_id && (
                      <div className="kanban-card-worker">Worker active</div>
                    )}
                  </div>
                  <div className="kanban-card-actions">
                    <button
                      className="kanban-card-menu-btn"
                      onClick={(e) => {
                        e.stopPropagation()
                        setCardMenuId(cardMenuId === card.id ? null : card.id)
                      }}
                    >
                      ...
                    </button>
                    {cardMenuId === card.id && (
                      <div className="kanban-card-menu">
                        {(card.worker_session_id || card.last_worker_session_id) && (
                          <button
                            onClick={() =>
                              handleViewSession(
                                (card.worker_session_id || card.last_worker_session_id)!,
                              )
                            }
                          >
                            View Session
                          </button>
                        )}
                        <button
                          onClick={() => {
                            setCardMenuId(null)
                            setEditingCard(card)
                          }}
                        >
                          Edit
                        </button>
                        <button
                          onClick={() => {
                            setCardMenuId(null)
                            setSelectedCard(card)
                          }}
                        >
                          Details
                        </button>
                        {card.worker_session_id && (
                          <button onClick={() => handleStopWorker(card)}>Stop Worker</button>
                        )}
                        {!card.worker_session_id &&
                          card.step !== 'done' &&
                          card.step !== 'wont_do' && (
                            <button onClick={() => handleRestartWorker(card)}>
                              Restart Worker
                            </button>
                          )}
                        {card.step !== 'done' && card.step !== 'wont_do' && (
                          <button className="danger" onClick={() => handleCancelWontDo(card)}>
                            Cancel as Won't Do
                          </button>
                        )}
                        <button
                          className="danger"
                          onClick={() => {
                            setCardMenuId(null)
                            setConfirmDeleteId(card.id)
                          }}
                        >
                          Delete
                        </button>
                      </div>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>

      {confirmDeleteId && (
        <div className="modal-backdrop" onClick={() => setConfirmDeleteId(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 360 }}>
            <h2>Delete card?</h2>
            <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text2)' }}>
              This will stop any active worker and permanently delete the card.
            </p>
            <div className="form-actions" style={{ marginTop: 16 }}>
              <button className="btn-secondary" onClick={() => setConfirmDeleteId(null)}>
                Cancel
              </button>
              <button className="btn-danger" onClick={() => handleDeleteCard(confirmDeleteId)}>
                Delete
              </button>
            </div>
          </div>
        </div>
      )}

      {selectedCard && (
        <div className="modal-backdrop" onClick={() => setSelectedCard(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>{selectedCard.title}</h2>
            <div className="card-detail-grid">
              <div className="card-detail-row">
                <span className="card-detail-label">Step</span>
                <span>
                  {STEPS.find((s) => s.key === selectedCard.step)?.label ?? selectedCard.step}
                </span>
              </div>
              <div className="card-detail-row">
                <span className="card-detail-label">Priority</span>
                {priorityBadge(selectedCard.priority, priorities)}
              </div>
              {selectedCard.workflow && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Workflow</span>
                  <span>{selectedCard.workflow}</span>
                </div>
              )}
              {selectedCard.model && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Model</span>
                  <span>{selectedCard.model}</span>
                </div>
              )}
              {selectedCard.effort && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Effort</span>
                  <span>{selectedCard.effort}</span>
                </div>
              )}
              <div className="card-detail-row">
                <span className="card-detail-label">Blocked</span>
                <span>{selectedCard.blocked ? 'Yes' : 'No'}</span>
              </div>
              {selectedCard.block_reason && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Block Reason</span>
                  <span>{selectedCard.block_reason}</span>
                </div>
              )}
              {(selectedCard.depends_on?.length ?? 0) > 0 && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Depends On</span>
                  <span>
                    {selectedCard.depends_on!.map((id) => {
                      const dep = cardById.get(id)
                      if (!dep) return null
                      const done = normalizeStep(dep.step) === 'done'
                      return (
                        <span key={id} className={done ? 'dep-done' : 'dep-pending'}>
                          {dep.title}
                          {done ? ' ✓' : ' (pending)'}
                          {'  '}
                        </span>
                      )
                    })}
                  </span>
                </div>
              )}
              {selectedCard.description && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Description</span>
                  <span>{selectedCard.description}</span>
                </div>
              )}
              {selectedCard.handoff_context && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Handoff Context</span>
                  <span>{selectedCard.handoff_context}</span>
                </div>
              )}
              {cardReports.length > 0 && (
                <div className="card-detail-row" style={{ flexDirection: 'column', gap: 6 }}>
                  <span className="card-detail-label">Reports ({cardReports.length})</span>
                  <div className="card-reports-list">
                    {cardReports.map((r) => (
                      <button
                        key={`${r.folder}/${r.file}`}
                        className="card-report-link"
                        onClick={() => {
                          window.location.assign('/reports')
                        }}
                      >
                        <span className="card-report-title">{r.title}</span>
                        <span className="card-report-date">
                          {r.date?.split('T')[0] ?? r.folder}
                        </span>
                      </button>
                    ))}
                  </div>
                </div>
              )}
            </div>
            <div className="card-detail-actions">
              {(selectedCard.worker_session_id || selectedCard.last_worker_session_id) && (
                <button
                  className="btn-secondary"
                  onClick={() =>
                    handleViewSession(
                      (selectedCard.worker_session_id || selectedCard.last_worker_session_id)!,
                    )
                  }
                >
                  View Session
                </button>
              )}
              <button className="btn-secondary" onClick={() => setSelectedCard(null)}>
                Close
              </button>
            </div>
          </div>
        </div>
      )}

      {editingCard && (
        <EditCardModal
          projectId={projectId}
          card={editingCard}
          onClose={() => {
            setEditingCard(null)
            fetchCards(projectId)
          }}
        />
      )}
    </div>
  )
}
