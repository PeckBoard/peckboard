import { useMemo, useState } from 'react'
import type { Card } from '../types/api'
import Modal from './Modal'

interface DependencyPickerModalProps {
  candidates: Card[]
  selectedIds: string[]
  onCancel: () => void
  onConfirm: (ids: string[]) => void
}

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

const stepLabel = (step: string) => {
  const normalized = normalizeStep(step)
  switch (normalized) {
    case 'backlog':
      return 'Backlog'
    case 'in_progress':
      return 'Running'
    case 'done':
      return 'Done'
    case 'wont_do':
      return "Won't Do"
    default:
      return normalized
  }
}

/**
 * Picker for the card-form "Depends On" field. By default it lists
 * backlog + running candidates (the cards a user is most likely to wait
 * on); typing in the search box widens the result set to every
 * candidate, including already-done cards, so the user can still pick
 * them when they want to.
 */
export default function DependencyPickerModal({
  candidates,
  selectedIds,
  onCancel,
  onConfirm,
}: DependencyPickerModalProps) {
  const [search, setSearch] = useState('')
  const [draft, setDraft] = useState<string[]>(selectedIds)

  const visible = useMemo(() => {
    const query = search.trim().toLowerCase()
    const selectedSet = new Set(draft)
    // Always include already-selected cards so the user can see (and
    // un-tick) them even if they no longer match the current filter.
    const matchesSearch = (c: Card) => (query === '' ? true : c.title.toLowerCase().includes(query))
    const inDefaultScope = (c: Card) => {
      const step = normalizeStep(c.step)
      return step === 'backlog' || step === 'in_progress'
    }
    const ranked = candidates.filter((c) => {
      if (selectedSet.has(c.id)) return true
      if (!matchesSearch(c)) return false
      // No search → only show backlog + running. With a search, widen
      // to every candidate that matches.
      return query !== '' || inDefaultScope(c)
    })
    // Selected first, then by step (backlog → running → done → wont_do),
    // then by title.
    const stepOrder = (c: Card) => {
      switch (normalizeStep(c.step)) {
        case 'backlog':
          return 0
        case 'in_progress':
          return 1
        case 'done':
          return 2
        case 'wont_do':
          return 3
        default:
          return 4
      }
    }
    return [...ranked].sort((a, b) => {
      const aSel = selectedSet.has(a.id) ? 0 : 1
      const bSel = selectedSet.has(b.id) ? 0 : 1
      if (aSel !== bSel) return aSel - bSel
      const stepDiff = stepOrder(a) - stepOrder(b)
      if (stepDiff !== 0) return stepDiff
      return a.title.localeCompare(b.title)
    })
  }, [candidates, draft, search])

  const toggle = (id: string) => {
    setDraft((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]))
  }

  return (
    <Modal
      onClose={onCancel}
      maxWidth={520}
      className="dependency-picker-modal"
      backdropClassName="dependency-picker-backdrop"
    >
      <h2>Select Dependencies</h2>
      <p className="form-hint" style={{ marginTop: 0, marginBottom: 10 }}>
        A worker only starts this card once every selected card is done.
      </p>
      <input
        className="form-input dependency-picker-search"
        placeholder="Search cards..."
        value={search}
        onChange={(e) => setSearch(e.target.value)}
        autoFocus
      />
      <div className="dependency-picker-list">
        {visible.length === 0 ? (
          <p className="form-hint dependency-picker-empty">
            {search.trim()
              ? 'No cards match your search.'
              : 'No backlog or running cards to depend on.'}
          </p>
        ) : (
          visible.map((c) => (
            <label key={c.id} className="dependency-picker-option">
              <input type="checkbox" checked={draft.includes(c.id)} onChange={() => toggle(c.id)} />
              <span className="dependency-picker-option-title">{c.title}</span>
              <span className={`dependency-picker-step step-${normalizeStep(c.step)}`}>
                {stepLabel(c.step)}
              </span>
            </label>
          ))
        )}
      </div>
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="btn-primary" onClick={() => onConfirm(draft)}>
          {draft.length === 0 ? 'Clear Dependencies' : `Save (${draft.length})`}
        </button>
      </div>
    </Modal>
  )
}
