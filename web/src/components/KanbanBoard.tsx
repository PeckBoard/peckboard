import { useEffect, useState } from 'react'
import { useProjectsStore } from '../store/projects'
import type { Card } from '../types/api'

const STEPS = [
  { key: 'backlog', label: 'Backlog' },
  { key: 'in_progress', label: 'In Progress' },
  { key: 'review', label: 'Review' },
  { key: 'done', label: 'Done' },
] as const

function priorityBadge(priority: number) {
  const map: Record<number, { label: string; className: string }> = {
    1: { label: 'High', className: 'priority-high' },
    2: { label: 'Medium', className: 'priority-medium' },
    3: { label: 'Low', className: 'priority-low' },
  }
  const info = map[priority] || { label: `P${priority}`, className: 'priority-low' }
  return <span className={`priority-badge ${info.className}`}>{info.label}</span>
}

interface KanbanBoardProps {
  projectId: string
}

export default function KanbanBoard({ projectId }: KanbanBoardProps) {
  const cards = useProjectsStore((s) => s.cards)
  const fetchCards = useProjectsStore((s) => s.fetchCards)
  const createCard = useProjectsStore((s) => s.createCard)
  const updateCard = useProjectsStore((s) => s.updateCard)
  const deleteCard = useProjectsStore((s) => s.deleteCard)
  const [selectedCard, setSelectedCard] = useState<Card | null>(null)
  const [showAddForm, setShowAddForm] = useState(false)
  const [addTitle, setAddTitle] = useState('')
  const [addDescription, setAddDescription] = useState('')
  const [addPriority, setAddPriority] = useState(2)
  const [addSubmitting, setAddSubmitting] = useState(false)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)
  const [draggingCardId, setDraggingCardId] = useState<string | null>(null)
  const [dragOverStep, setDragOverStep] = useState<string | null>(null)

  useEffect(() => {
    fetchCards(projectId)
  }, [projectId, fetchCards])

  const cardsByStep = (step: string) => cards.filter((c) => c.step === step)

  const handleAddCard = async () => {
    if (!addTitle.trim() || addSubmitting) return
    setAddSubmitting(true)
    try {
      await createCard(projectId, {
        title: addTitle.trim(),
        description: addDescription.trim(),
        step: 'backlog',
        priority: addPriority,
      } as Partial<Card>)
      setAddTitle('')
      setAddDescription('')
      setAddPriority(2)
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
    } catch {
      /* ignore */
    }
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
    // Only clear if we're actually leaving the column, not entering a child
    const related = e.relatedTarget as Node | null
    const current = e.currentTarget as Node
    if (!related || !current.contains(related)) {
      if (dragOverStep === stepKey) {
        setDragOverStep(null)
      }
    }
  }

  const handleDrop = async (e: React.DragEvent, targetStep: string) => {
    e.preventDefault()
    setDragOverStep(null)
    const cardId = e.dataTransfer.getData('cardId')
    const fromStep = e.dataTransfer.getData('fromStep')
    if (!cardId || fromStep === targetStep) return

    // Optimistic update: modify local state immediately
    useProjectsStore.setState((s) => ({
      cards: s.cards.map((c) =>
        c.id === cardId ? { ...c, step: targetStep } : c
      ),
    }))

    try {
      await updateCard(projectId, cardId, { step: targetStep })
    } catch {
      // Revert on failure
      fetchCards(projectId)
    }
  }

  return (
    <div className="kanban-board">
      <div className="kanban-board-header">
        <button className="btn-primary" onClick={() => setShowAddForm(!showAddForm)}>
          {showAddForm ? 'Cancel' : 'Add Card'}
        </button>
      </div>

      {showAddForm && (
        <div className="kanban-add-form">
          <input
            type="text"
            placeholder="Card title"
            value={addTitle}
            onChange={(e) => setAddTitle(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') handleAddCard() }}
            autoFocus
          />
          <textarea
            placeholder="Description"
            value={addDescription}
            onChange={(e) => setAddDescription(e.target.value)}
            rows={3}
          />
          <select value={addPriority} onChange={(e) => setAddPriority(Number(e.target.value))}>
            <option value={1}>High priority</option>
            <option value={2}>Medium priority</option>
            <option value={3}>Low priority</option>
          </select>
          <button className="btn-primary" onClick={handleAddCard} disabled={!addTitle.trim() || addSubmitting}>
            {addSubmitting ? 'Creating...' : 'Create Card'}
          </button>
        </div>
      )}

      <div className="kanban-columns">
        {STEPS.map((step) => (
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
                <button
                  key={card.id}
                  className={`kanban-card ${card.blocked ? 'blocked' : ''}${draggingCardId === card.id ? ' dragging' : ''}`}
                  draggable={true}
                  onDragStart={(e) => handleDragStart(e, card)}
                  onDragEnd={handleDragEnd}
                  onClick={() => setSelectedCard(card)}
                >
                  <div className="kanban-card-header">
                    <span className="kanban-card-title">{card.title}</span>
                    {priorityBadge(card.priority)}
                  </div>
                  {card.blocked && (
                    <div className="blocked-indicator">
                      Blocked{card.block_reason ? `: ${card.block_reason}` : ''}
                    </div>
                  )}
                </button>
              ))}
            </div>
          </div>
        ))}
      </div>

      {selectedCard && (
        <div className="modal-backdrop" onClick={() => { setSelectedCard(null); setConfirmDeleteId(null) }}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>{selectedCard.title}</h2>
            <div className="card-detail-grid">
              <div className="card-detail-row">
                <span className="card-detail-label">Step</span>
                <span>{selectedCard.step}</span>
              </div>
              <div className="card-detail-row">
                <span className="card-detail-label">Priority</span>
                {priorityBadge(selectedCard.priority)}
              </div>
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
              {selectedCard.workflow && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Workflow</span>
                  <span>{selectedCard.workflow}</span>
                </div>
              )}
              {selectedCard.description && (
                <div className="card-detail-row">
                  <span className="card-detail-label">Description</span>
                  <span>{selectedCard.description}</span>
                </div>
              )}
            </div>
            <div className="card-detail-actions">
              {selectedCard.worker_session_id && (
                <button
                  className="btn-secondary"
                  onClick={() => {
                    window.location.href = `/sessions/${selectedCard.worker_session_id}`
                  }}
                >
                  View Session
                </button>
              )}
              {confirmDeleteId === selectedCard.id ? (
                <div className="card-delete-confirm">
                  <span>Delete this card?</span>
                  <button className="btn-danger" onClick={() => handleDeleteCard(selectedCard.id)}>
                    Confirm Delete
                  </button>
                  <button className="btn-secondary" onClick={() => setConfirmDeleteId(null)}>
                    Cancel
                  </button>
                </div>
              ) : (
                <button className="btn-danger" onClick={() => setConfirmDeleteId(selectedCard.id)}>
                  Delete Card
                </button>
              )}
            </div>
            <button className="close-btn" onClick={() => { setSelectedCard(null); setConfirmDeleteId(null) }}>
              Close
            </button>
          </div>
        </div>
      )}
    </div>
  )
}
