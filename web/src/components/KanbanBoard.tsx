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
  const [selectedCard, setSelectedCard] = useState<Card | null>(null)

  useEffect(() => {
    fetchCards(projectId)
  }, [projectId, fetchCards])

  const cardsByStep = (step: string) => cards.filter((c) => c.step === step)

  return (
    <div className="kanban-board">
      <div className="kanban-columns">
        {STEPS.map((step) => (
          <div key={step.key} className="kanban-column">
            <div className="kanban-column-header">
              <h3>{step.label}</h3>
              <span className="kanban-count">{cardsByStep(step.key).length}</span>
            </div>
            <div className="kanban-cards">
              {cardsByStep(step.key).map((card) => (
                <button
                  key={card.id}
                  className={`kanban-card ${card.blocked ? 'blocked' : ''}`}
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
        <div className="modal-backdrop" onClick={() => setSelectedCard(null)}>
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
            <button className="close-btn" onClick={() => setSelectedCard(null)}>
              Close
            </button>
          </div>
        </div>
      )}
    </div>
  )
}
