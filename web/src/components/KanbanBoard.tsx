import { useCallback, useEffect, useState } from 'react'
import { useProjectsStore } from '../store/projects'
import { useWsStore } from '../store/ws'
import { authedFetch } from '../store/auth'
import type { Card, Event } from '../types/api'
import EditCardModal from './EditCardModal'

const STEPS = [
  { key: 'backlog', label: 'Backlog' },
  { key: 'in_progress', label: 'In Progress' },
  { key: 'review', label: 'Review' },
  { key: 'done', label: 'Done' },
  { key: 'wont_do', label: "Won't Do" },
] as const

interface WorkflowInfo {
  id: string
  name: string
}
interface ModelInfo {
  id: string
  display_name: string
}

const EFFORT_OPTIONS = [
  { value: '', label: 'Default' },
  { value: 'low', label: 'Low' },
  { value: 'medium', label: 'Medium' },
  { value: 'high', label: 'High' },
  { value: 'xhigh', label: 'Extra high' },
  { value: 'max', label: 'Max' },
]

interface PriorityInfo {
  label: string
  value: number
  description: string
}

function priorityBadge(priority: number, priorities: PriorityInfo[]) {
  const p = priorities.find((pr) => pr.value === priority)
  const label = p?.label ?? `P${priority}`
  const className =
    priority <= 0 ? 'priority-critical' :
    priority <= 1 ? 'priority-high' :
    priority <= 2 ? 'priority-medium' :
    'priority-low'
  return <span className={`priority-badge ${className}`}>{label}</span>
}

interface PendingQuestion {
  eventId: string
  sessionId: string
  ts: number
  questions: QuestionItem[]
  cardId: string | null
  cardTitle: string | null
  cardDescription: string | null
}

interface QuestionItem {
  question: string
  header?: string
  multiSelect?: boolean
  options?: string[]
  optionObjects?: { label: string; description?: string }[]
}

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
  const [selectedCard, setSelectedCard] = useState<Card | null>(null)
  const [showAddForm, setShowAddForm] = useState(false)
  const [addTitle, setAddTitle] = useState('')
  const [addDescription, setAddDescription] = useState('')
  const [addPriority, setAddPriority] = useState(2)
  const [addWorkflow, setAddWorkflow] = useState('')
  const [addModel, setAddModel] = useState('')
  const [addEffort, setAddEffort] = useState('')
  const [addSubmitting, setAddSubmitting] = useState(false)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)
  const [draggingCardId, setDraggingCardId] = useState<string | null>(null)
  const [dragOverStep, setDragOverStep] = useState<string | null>(null)
  const [cardMenuId, setCardMenuId] = useState<string | null>(null)
  const [editingCard, setEditingCard] = useState<Card | null>(null)

  const [workflows, setWorkflows] = useState<WorkflowInfo[]>([])
  const [models, setModels] = useState<ModelInfo[]>([])
  const [priorities, setPriorities] = useState<PriorityInfo[]>([
    { label: 'Critical', value: 0, description: 'Blocks everything' },
    { label: 'High', value: 1, description: 'Important' },
    { label: 'Medium', value: 2, description: 'Normal' },
    { label: 'Low', value: 3, description: 'Nice to have' },
    { label: 'Backlog', value: 4, description: 'Someday' },
  ])
  const [pendingQuestions, setPendingQuestions] = useState<PendingQuestion[]>([])
  const [questionAnswers, setQuestionAnswers] = useState<Record<string, Record<number, string>>>({})
  const [submittingQuestion, setSubmittingQuestion] = useState<string | null>(null)
  const [questionDialogOpen, setQuestionDialogOpen] = useState(false)

  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)

  const fetchPendingQuestions = useCallback(async () => {
    const res = await authedFetch(`/api/projects/${projectId}/pending-questions`)
    if (res.ok) {
      const data = await res.json()
      setPendingQuestions(data.questions ?? [])
    }
  }, [projectId])

  // Fetch pending questions on mount and when events arrive
  useEffect(() => {
    fetchPendingQuestions()
  }, [fetchPendingQuestions])

  // Listen for WebSocket events to refresh pending questions
  useEffect(() => {
    const listener = (event: Event) => {
      if (event.kind === 'question' || event.kind === 'question-resolved') {
        fetchPendingQuestions()
      }
    }
    addEventListener(listener)
    return () => removeEventListener(listener)
  }, [addEventListener, removeEventListener, fetchPendingQuestions])

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
      setQuestionAnswers((prev) => { const next = { ...prev }; delete next[pq.eventId]; return next })
      fetchPendingQuestions()
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
      fetchPendingQuestions()
    } finally {
      setSubmittingQuestion(null)
    }
  }

  const setQuestionAnswer = (eventId: string, idx: number, value: string) => {
    setQuestionAnswers((prev) => ({
      ...prev,
      [eventId]: { ...(prev[eventId] ?? {}), [idx]: value },
    }))
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

  useEffect(() => {
    authedFetch('/api/workflows')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.workflows) setWorkflows(data.workflows)
      })
      .catch(() => {})
    authedFetch('/api/models')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.models) setModels(data.models)
      })
      .catch(() => {})
    authedFetch('/api/priorities')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.priorities) setPriorities(data.priorities)
      })
      .catch(() => {})
  }, [])

  // Map step aliases to canonical step names for display
  const normalizeStep = (step: string) => {
    switch (step) {
      case 'todo': return 'backlog'
      case 'in-progress': return 'in_progress'
      case 'wont-do': return 'wont_do'
      default: return step
    }
  }
  const cardsByStep = (step: string) => cards.filter((c) => normalizeStep(c.step) === step)

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
      } as Partial<Card>)
      setAddTitle('')
      setAddDescription('')
      setAddPriority(2)
      setAddWorkflow('')
      setAddModel('')
      setAddEffort('')
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
    window.location.href = `/sessions/${sessionId}`
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
        <button className="btn-primary" onClick={() => setShowAddForm(!showAddForm)}>
          {showAddForm ? 'Cancel' : 'Add Card'}
        </button>
      </div>

      {/* Pending worker questions — trigger button */}
      {pendingQuestions.length > 0 && !questionDialogOpen && (
        <button
          className="worker-questions-trigger"
          onClick={() => setQuestionDialogOpen(true)}
        >
          <span className="worker-questions-icon">&#x2753;</span>
          <span>{pendingQuestions.length} worker question{pendingQuestions.length > 1 ? 's' : ''} need your answer</span>
        </button>
      )}

      {/* Question dialog — shows first pending question */}
      {questionDialogOpen && pendingQuestions.length > 0 && (() => {
        const pq = pendingQuestions[0]
        const answers = questionAnswers[pq.eventId] ?? {}
        const isSubmitting = submittingQuestion === pq.eventId
        const hasAnswers = pq.questions.some((_, idx) => (answers[idx] ?? '').trim().length > 0)
        const remaining = pendingQuestions.length - 1

        return (
          <div className="modal-backdrop" onClick={() => setQuestionDialogOpen(false)}>
            <div className="modal question-dialog" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 520 }}>
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
                                {optObj?.description && <span className="question-option-desc">{optObj.description}</span>}
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
                        value={answers[idx] ?? ''}
                        onChange={(e) => setQuestionAnswer(pq.eventId, idx, e.target.value)}
                        onKeyDown={(e) => { if (e.key === 'Enter' && pq.questions.length === 1 && hasAnswers) handleAnswerQuestion(pq) }}
                        disabled={isSubmitting}
                        autoFocus
                      />
                    )}
                  </div>
                ))}
              </div>

              {/* Footer — submit or dismiss only */}
              <div className="question-dialog-footer">
                <button className="btn-secondary" onClick={async () => {
                  await handleDismissQuestion(pq)
                  if (pendingQuestions.length <= 1) setQuestionDialogOpen(false)
                }} disabled={isSubmitting}>Dismiss</button>
                <button className="btn-primary" onClick={async () => {
                  await handleAnswerQuestion(pq)
                  if (pendingQuestions.length <= 1) setQuestionDialogOpen(false)
                }} disabled={!hasAnswers || isSubmitting}>
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
                <option key={p.value} value={p.value}>{p.label}</option>
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
                  <div className="kanban-card-top" onClick={() => setSelectedCard(card)}>
                    <div className="kanban-card-header">
                      <span className="kanban-card-title">{card.title}</span>
                      {priorityBadge(card.priority, priorities)}
                    </div>
                    {card.blocked && (
                      <div className="blocked-indicator">
                        Blocked{card.block_reason ? `: ${card.block_reason}` : ''}
                      </div>
                    )}
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
