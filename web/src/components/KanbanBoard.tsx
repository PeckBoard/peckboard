import { useEffect, useRef, useState } from 'react'
import { useProjectsStore, type PendingQuestion } from '../store/projects'
import { useResourcesStore } from '../store/resources'
import { useWsStore } from '../store/ws'
import { authedFetch } from '../store/auth'
import { useMentions, filterMentions } from '../hooks/useMentions'
import type { Card, Event, Project } from '../types/api'
import CardFormModal from './CardFormModal'
import EditProjectModal from './EditProjectModal'
import { MenuButton, type MenuItem } from './Dropdown'
import Modal from './Modal'
import WorkerComms from './WorkerComms'
import ProjectTodoSummary from './ProjectTodoSummary'
import SafeMarkdown from './SafeMarkdown'
import { fetchPlanId, openPlan } from '../lib/plan'
import { useProjectTodos } from '../hooks/useProjectTodos'
import {
  EMPTY_QUESTIONS,
  EMPTY_REPORTS,
  STEPS,
  THOUGHT_BUBBLE_MS,
  type PriorityInfo,
  type ThoughtBubble,
  priorityAtInsertIdx,
  priorityBadge,
  summarizeEvent,
} from './kanban/utils'
import PriorityChevron from './kanban/PriorityChevron'

interface KanbanBoardProps {
  projectId: string
  /** Navigate to the dedicated project-todos view. */
  onOpenTodos?: () => void
  /** Plugin project-page entries to surface as toolbar buttons. */
  pluginItems?: { plugin: string; id: string; label: string }[]
  /** Open a plugin entry's full-page view by its item id. */
  onOpenPlugin?: (itemId: string) => void
}

export default function KanbanBoard({
  projectId,
  onOpenTodos,
  pluginItems,
  onOpenPlugin,
}: KanbanBoardProps) {
  // The board renders the same classic vertical-columns kanban on every
  // viewport: columns side by side, cards stacked top-to-bottom inside
  // each column. On a narrow phone the columns get narrow but the
  // shape stays consistent. DnD reorder uses a vertical midpoint test
  // against the dragged-over card.

  const projects = useProjectsStore((s) => s.projects)
  const updateProject = useProjectsStore((s) => s.updateProject)
  const cards = useProjectsStore((s) => s.cards)
  const fetchCards = useProjectsStore((s) => s.fetchCards)
  const updateCard = useProjectsStore((s) => s.updateCard)
  const deleteCard = useProjectsStore((s) => s.deleteCard)

  const project = projects.find((p) => p.id === projectId)
  // cardId -> latest todo snapshot for that card's worker session.
  const todosByCard = useProjectTodos(cards)
  const [selectedCard, setSelectedCard] = useState<Card | null>(null)
  const [showAddForm, setShowAddForm] = useState(false)
  const [editingProject, setEditingProject] = useState(false)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)
  const [draggingCardId, setDraggingCardId] = useState<string | null>(null)
  // Active drop target. `insertIdx` is set only for an in-row reorder hover
  // (mouse is over a sibling card in the same row); a null `insertIdx` means
  // a cross-row step-move hover (the destination row shows an accept band).
  const [dragOver, setDragOver] = useState<{ step: string; insertIdx: number | null } | null>(null)
  const [cardMenuId, setCardMenuId] = useState<string | null>(null)
  // Trigger-button rect captured when the "..." menu opens, used to
  // position the fixed dropdown so it can escape any clipping ancestor
  // (modal, scroll container) and align under the trigger.
  const [cardMenuRect, setCardMenuRect] = useState<DOMRect | null>(null)
  const [cardMenuPlanId, setCardMenuPlanId] = useState<string | null>(null)
  const closeCardMenu = () => {
    setCardMenuId(null)
    setCardMenuRect(null)
    setCardMenuPlanId(null)
  }
  const openCardMenu = (cardId: string, btn: HTMLElement) => {
    if (cardMenuId === cardId) {
      closeCardMenu()
    } else {
      setCardMenuId(cardId)
      setCardMenuRect(btn.getBoundingClientRect())
      setCardMenuPlanId(null)
      void fetchPlanId({ cardId }).then(setCardMenuPlanId)
    }
  }
  const [editingCard, setEditingCard] = useState<Card | null>(null)
  const [showComms, setShowComms] = useState(false)
  // Cards are collapsed by default — header only. Tapping the card toggles
  // its entry in this set. Independent per card so opening one doesn't
  // collapse the others.
  const [expandedCardIds, setExpandedCardIds] = useState<Set<string>>(new Set())
  const toggleCardExpanded = (cardId: string) => {
    setExpandedCardIds((prev) => {
      const next = new Set(prev)
      if (next.has(cardId)) next.delete(cardId)
      else next.add(cardId)
      return next
    })
  }
  // One transient thought bubble per card, keyed by card id.
  const [bubbles, setBubbles] = useState<Record<string, ThoughtBubble>>({})
  const bubbleTimers = useRef<Record<string, ReturnType<typeof setTimeout>>>({})
  // Latest session→card map, read inside the (stable) event listener.
  const sessionToCardRef = useRef<Record<string, string>>({})
  // cardId -> latest worker context-window occupancy, live from streamed
  // `agent-usage` events; seeded per card by `context_tokens` on the cards
  // fetch (the badge falls back to the seed until a live event arrives).
  const [ctxByCard, setCtxByCard] = useState<Record<string, number>>({})
  // Same session→card idea as sessionToCardRef, but including
  // `last_worker_session_id` so the badge keeps updating between chunk
  // dispatches (e.g. during an auto-compaction doc turn).
  const ctxSessionToCardRef = useRef<Record<string, string>>({})
  const cardReports = useProjectsStore((s) =>
    selectedCard ? (s.cardReportsByCard[selectedCard.id] ?? EMPTY_REPORTS) : EMPTY_REPORTS,
  )
  const fetchCardReports = useProjectsStore((s) => s.fetchCardReports)

  // Fetch reports when card detail is opened
  useEffect(() => {
    if (!selectedCard) return
    fetchCardReports(projectId, selectedCard.id)
  }, [selectedCard, projectId, fetchCardReports])

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

  // Listen for project-update broadcasts — currently the auto-pause path
  // is the only emitter, so this is how the "project paused, here's why"
  // banner appears without the user having to refresh.
  useEffect(() => {
    const handler = (e: globalThis.Event) => {
      const detail = (e as CustomEvent).detail
      const project = detail?.data?.project as Project | undefined
      if (!project || project.id !== projectId) return
      useProjectsStore.setState((s) => ({
        projects: s.projects.map((p) => (p.id === project.id ? project : p)),
      }))
    }
    window.addEventListener('peckboard:project-update', handler)
    return () => window.removeEventListener('peckboard:project-update', handler)
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

  // Keep a live session→card lookup for the thought-bubble listener, plus a
  // wider one (including resumable last-sessions) for the context badge.
  useEffect(() => {
    const map: Record<string, string> = {}
    const ctxMap: Record<string, string> = {}
    for (const c of cards) {
      if (c.worker_session_id) {
        map[c.worker_session_id] = c.id
        ctxMap[c.worker_session_id] = c.id
      }
      if (c.last_worker_session_id) ctxMap[c.last_worker_session_id] = c.id
    }
    sessionToCardRef.current = map
    ctxSessionToCardRef.current = ctxMap
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
      // Context badge: any subscribed worker session's usage event updates
      // its card's occupancy, mirroring the chat toolbar's live badge.
      if (event.kind === 'agent-usage') {
        const ctxCard = ctxSessionToCardRef.current[event.session_id]
        const ctx = (event.data?.contextTokens as number) ?? 0
        if (ctxCard && ctx > 0) setCtxByCard((prev) => ({ ...prev, [ctxCard]: ctx }))
        return
      }
      // A `handover` event (worker auto-compaction) restarts the worker's
      // conversation — drop the card's live occupancy so the badge doesn't
      // keep showing the pre-compaction window until the next turn reports.
      if (event.kind === 'handover') {
        const ctxCard = ctxSessionToCardRef.current[event.session_id]
        if (ctxCard) setCtxByCard((prev) => ({ ...prev, [ctxCard]: 0 }))
      }
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

  const cardsByStep = (step: string) => {
    const rows = cards.filter((c) => normalizeStep(c.step) === step)
    // Done column shows most-recently-finished first so a freshly
    // completed card jumps to the top instead of being buried behind
    // older completions in priority order. Backend list is sorted by
    // priority ASC; we re-order Done client-side because it's the only
    // step with a non-priority rule and we want the live WS update
    // (which just patches the cards array) to re-order in place.
    if (step === 'done') {
      return [...rows].sort((a, b) => {
        const aTs = a.completed_at ?? ''
        const bTs = b.completed_at ?? ''
        if (aTs === bTs) return 0
        if (!aTs) return 1
        if (!bTs) return -1
        return bTs.localeCompare(aTs)
      })
    }
    // Other columns show pickup order: cards already in-flight first,
    // then ready-to-pick (deps met, unblocked), then waiting (blocked or
    // deps unmet). Within each group, priority ASC then created_at ASC
    // matches the orchestrator's pickup order — older same-priority
    // cards queue ahead of new ones.
    const pickupBucket = (c: Card): number => {
      if (c.worker_session_id) return 0
      if (c.blocked) return 2
      if (unmetDeps(c).length > 0) return 2
      return 1
    }
    return [...rows].sort((a, b) => {
      const ba = pickupBucket(a)
      const bb = pickupBucket(b)
      if (ba !== bb) return ba - bb
      if (a.priority !== b.priority) return a.priority - b.priority
      const aTs = a.created_at ?? ''
      const bTs = b.created_at ?? ''
      return aTs.localeCompare(bTs)
    })
  }

  const handleDeleteCard = async (cardId: string) => {
    try {
      await deleteCard(projectId, cardId)
      setSelectedCard(null)
      setConfirmDeleteId(null)
      closeCardMenu()
    } catch {
      /* ignore */
    }
  }

  const handleViewSession = (sessionId: string) => {
    closeCardMenu()
    setSelectedCard(null)
    // location.assign mutates via a method call (not a property write),
    // which the immutability lint accepts.
    window.location.assign(`/sessions/${sessionId}`)
  }

  const handleStopWorker = async (card: Card) => {
    closeCardMenu()
    await authedFetch(`/api/projects/${projectId}/cards/${card.id}/stop`, { method: 'POST' })
    fetchCards(projectId)
  }

  const handleRestartWorker = async (card: Card) => {
    closeCardMenu()
    await authedFetch(`/api/projects/${projectId}/cards/${card.id}/restart`, { method: 'POST' })
    fetchCards(projectId)
  }

  const handleCancelWontDo = async (card: Card) => {
    closeCardMenu()
    await authedFetch(`/api/projects/${projectId}/cards/${card.id}/cancel-wont-do`, {
      method: 'POST',
    })
    fetchCards(projectId)
  }

  // Step the currently-dragged card originates from, or null when no drag
  // is active. Read during dragover (when dataTransfer is locked for
  // security) to tell an in-row reorder from a cross-row step move.
  const draggingFromStep = (() => {
    if (!draggingCardId) return null
    const c = cards.find((x) => x.id === draggingCardId)
    return c ? normalizeStep(c.step) : null
  })()

  const handleDragStart = (e: React.DragEvent, card: Card) => {
    e.dataTransfer.setData('cardId', card.id)
    e.dataTransfer.setData('fromStep', card.step)
    e.dataTransfer.effectAllowed = 'move'
    setDraggingCardId(card.id)
  }

  const handleDragEnd = () => {
    setDraggingCardId(null)
    setDragOver(null)
  }

  // Dragover on the row container (gaps + empty trailing space). Cross-row
  // hover → row accept band. Same-row hover with no card under the cursor →
  // drop at the end of the row.
  const handleColumnDragOver = (e: React.DragEvent, stepKey: string, cardCount: number) => {
    e.preventDefault()
    e.dataTransfer.dropEffect = 'move'
    if (draggingFromStep === stepKey) {
      // Same-row, between-cards: default to "insert at end" so a horizontal
      // drag past the last card still shows a meaningful indicator.
      setDragOver({ step: stepKey, insertIdx: cardCount })
    } else {
      setDragOver({ step: stepKey, insertIdx: null })
    }
  }

  // Dragover on a specific card. Insertion axis tracks the layout: leading
  // half of the card → insert before this card; trailing half → insert
  // after. On desktop (vertical columns) "leading" is the top half; on
  // mobile (horizontal rows) it's the left half. Cross-step hover still
  // falls through to the row accept band (no insert indicator) so the
  // gesture matches the layout.
  const handleCardDragOver = (
    e: React.DragEvent,
    stepKey: string,
    cardIndex: number,
    cardEl: HTMLElement,
  ) => {
    e.preventDefault()
    e.stopPropagation()
    e.dataTransfer.dropEffect = 'move'
    if (draggingFromStep !== stepKey) {
      setDragOver({ step: stepKey, insertIdx: null })
      return
    }
    const rect = cardEl.getBoundingClientRect()
    const insertIdx = e.clientY < rect.top + rect.height / 2 ? cardIndex : cardIndex + 1
    setDragOver({ step: stepKey, insertIdx })
  }

  const handleDragLeave = (e: React.DragEvent, stepKey: string) => {
    const related = e.relatedTarget as Node | null
    const current = e.currentTarget as Node
    if (!related || !current.contains(related)) {
      setDragOver((d) => (d?.step === stepKey ? null : d))
    }
  }

  const handleDrop = async (e: React.DragEvent, targetStep: string) => {
    e.preventDefault()
    const insertIdx = dragOver?.insertIdx ?? null
    setDragOver(null)
    const cardId = e.dataTransfer.getData('cardId')
    const fromStep = e.dataTransfer.getData('fromStep')
    if (!cardId) return

    // Cross-row drop → step change. Existing persistence.
    if (normalizeStep(fromStep) !== targetStep) {
      useProjectsStore.setState((s) => ({
        cards: s.cards.map((c) => (c.id === cardId ? { ...c, step: targetStep } : c)),
      }))
      try {
        await updateCard(projectId, cardId, { step: targetStep })
      } catch {
        fetchCards(projectId)
      }
      return
    }

    // Same-row drop → reorder via priority bucket. No `priority` field on
    // the dragged card means no-op (rare; only if the row sort changed
    // mid-drag). Without an explicit insert position we can't infer intent,
    // so leave it alone.
    if (insertIdx === null) return

    const rowCards = cardsByStep(targetStep)
    const draggedCard = rowCards.find((c) => c.id === cardId)
    if (!draggedCard) return
    const targetPriority = priorityAtInsertIdx(rowCards, cardId, insertIdx, draggedCard.priority)
    if (targetPriority === draggedCard.priority) return

    useProjectsStore.setState((s) => ({
      cards: s.cards.map((c) => (c.id === cardId ? { ...c, priority: targetPriority } : c)),
    }))
    try {
      await updateCard(projectId, cardId, { priority: targetPriority })
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
      <div className="kanban-board-header chat-toolbar">
        <span className="chat-toolbar-name">{project?.name ?? 'Project'}</span>
        {project && project.status !== 'active' && (
          <span className={`status-badge status-${project.status}`}>{project.status}</span>
        )}
        <span className="chat-toolbar-spacer" />
        <button
          type="button"
          className="kanban-header-icon-btn"
          onClick={() => setShowComms(true)}
          title="Worker communications"
          aria-label="Worker communications"
        >
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden="true"
          >
            <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
          </svg>
        </button>
        <button type="button" className="btn-primary btn-sm" onClick={() => setShowAddForm(true)}>
          Add Card
        </button>
        <MenuButton
          ariaLabel="Project menu"
          triggerClassName="chat-toolbar-menu"
          items={
            project
              ? ([
                  {
                    label: 'Edit project',
                    onSelect: () => setEditingProject(true),
                  },
                  { divider: true },
                  {
                    label: 'Tasks',
                    onSelect: onOpenTodos,
                    hidden: !onOpenTodos,
                    testId: 'project-menu-todos',
                  },
                  ...(onOpenPlugin
                    ? (pluginItems ?? []).map((item) => ({
                        label: item.label,
                        onSelect: () => onOpenPlugin(item.id),
                        testId: `project-menu-plugin-${item.id}`,
                      }))
                    : []),
                  { divider: true },
                  {
                    label: project.status === 'paused' ? 'Resume' : 'Pause',
                    onSelect: () =>
                      updateProject(projectId, {
                        status: project.status === 'paused' ? 'active' : 'paused',
                      } as Record<string, unknown>),
                  },
                ] as MenuItem[])
              : []
          }
        />
      </div>

      <div className="kanban-board-scroll">
        {project?.pause_reason && (
          <div className="project-pause-banner" role="status" data-testid="project-pause-banner">
            <strong>Project paused.</strong> {project.pause_reason}
          </div>
        )}

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
              <Modal
                onClose={() => setQuestionDialogOpen(false)}
                maxWidth={520}
                className="question-dialog"
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
              </Modal>
            )
          })()}

        {showAddForm && (
          <CardFormModal
            mode="create"
            projectId={projectId}
            onClose={() => setShowAddForm(false)}
          />
        )}

        <div className="kanban-columns">
          {visibleSteps.map((step) => {
            const rowCards = cardsByStep(step.key)
            const dragOverRow = dragOver?.step === step.key
            const showInsertIndicator = dragOverRow && dragOver?.insertIdx != null
            const showAcceptBand = dragOverRow && dragOver?.insertIdx == null
            return (
              <div
                key={step.key}
                className={`kanban-column${showAcceptBand ? ' drag-over' : ''}`}
                onDragOver={(e) => handleColumnDragOver(e, step.key, rowCards.length)}
                onDragLeave={(e) => handleDragLeave(e, step.key)}
                onDrop={(e) => handleDrop(e, step.key)}
              >
                <header className="kanban-column-header">
                  <h3>{step.label}</h3>
                  <span className="kanban-count" aria-label={`${rowCards.length} cards`}>
                    {rowCards.length}
                  </span>
                </header>
                <div className="kanban-cards">
                  {rowCards.length === 0 && (
                    <span className="kanban-cards-empty">No cards in {step.label}</span>
                  )}
                  {rowCards.map((card, cardIndex) => {
                    const todos = todosByCard[card.id]
                    const todoDone = todos ? todos.filter((t) => t.status === 'done').length : 0
                    const pendingDeps =
                      !card.worker_session_id && card.step !== 'done' && card.step !== 'wont_do'
                        ? unmetDeps(card)
                        : []
                    const description = (card.description ?? '').trim()
                    const sessionForCard = card.worker_session_id || card.last_worker_session_id
                    const sessionVisible =
                      !!sessionForCard && normalizeStep(card.step) !== 'backlog'
                    const dropBefore =
                      showInsertIndicator &&
                      dragOver?.insertIdx === cardIndex &&
                      draggingCardId !== card.id
                    // Last card carries the trailing indicator when the drop
                    // would land at the end of the row.
                    const dropAfter =
                      showInsertIndicator &&
                      cardIndex === rowCards.length - 1 &&
                      dragOver?.insertIdx === rowCards.length &&
                      draggingCardId !== card.id
                    const cardStep = normalizeStep(card.step)
                    const priorityLocked = cardStep === 'done' || cardStep === 'wont_do'
                    // Worker context badge: live value falls back to the
                    // fetch-time seed; hidden once the card is terminal.
                    const workerCtx = priorityLocked
                      ? 0
                      : (ctxByCard[card.id] ?? card.context_tokens ?? 0)
                    const expanded = expandedCardIds.has(card.id)
                    return (
                      <div
                        key={`${card.id}-${card.step}`}
                        className={`kanban-card ${card.blocked ? 'blocked' : ''}${
                          draggingCardId === card.id ? ' dragging' : ''
                        }${expanded ? ' expanded' : ''}`}
                        data-drop-before={dropBefore ? 'true' : undefined}
                        data-drop-after={dropAfter ? 'true' : undefined}
                        data-expanded={expanded ? 'true' : 'false'}
                        draggable={true}
                        onDragStart={(e) => handleDragStart(e, card)}
                        onDragEnd={handleDragEnd}
                        onDragOver={(e) =>
                          handleCardDragOver(e, step.key, cardIndex, e.currentTarget as HTMLElement)
                        }
                        onClick={(e) => {
                          // Clicks bubbling up from interactive descendants
                          // (priority picker, action buttons, 3-dot menu)
                          // already call `stopPropagation`. Anything that
                          // reaches the card root is a "tap the card body"
                          // intent — toggle the expand state.
                          if ((e.target as HTMLElement).closest('[data-no-toggle]')) return
                          toggleCardExpanded(card.id)
                        }}
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
                        <div className="kanban-card-header">
                          <PriorityChevron
                            value={card.priority}
                            disabled={priorityLocked}
                            priorities={priorities}
                            onChange={(next) => updateCard(projectId, card.id, { priority: next })}
                          />
                          <span className="kanban-card-title" title={card.title}>
                            {card.title}
                          </span>
                          <div className="kanban-card-actions" data-no-toggle>
                            {workerCtx > 0 && (
                              <span
                                className={`kanban-card-ctx${
                                  workerCtx >= 200_000
                                    ? ' over'
                                    : workerCtx >= 170_000
                                      ? ' warn'
                                      : ''
                                }`}
                                title={`Worker context: ${workerCtx.toLocaleString()} tokens (auto-compacts at 200k)`}
                              >
                                {Math.round(workerCtx / 1000)}k
                              </span>
                            )}
                            <button
                              className="kanban-card-menu-btn"
                              aria-label="Card menu"
                              onClick={(e) => {
                                e.stopPropagation()
                                openCardMenu(card.id, e.currentTarget as HTMLElement)
                              }}
                            >
                              ...
                            </button>
                            {cardMenuId === card.id && cardMenuRect && (
                              <div
                                className="kanban-card-menu"
                                data-no-toggle
                                style={{
                                  top: cardMenuRect.bottom + 4,
                                  right: Math.max(8, window.innerWidth - cardMenuRect.right),
                                }}
                              >
                                <button
                                  onClick={() => {
                                    closeCardMenu()
                                    setSelectedCard(card)
                                  }}
                                >
                                  View
                                </button>
                                <button
                                  disabled={!cardMenuPlanId}
                                  onClick={() => {
                                    if (!cardMenuPlanId) return
                                    closeCardMenu()
                                    openPlan(cardMenuPlanId)
                                  }}
                                  data-testid="card-menu-plan"
                                >
                                  Plan
                                </button>
                                {sessionVisible && (
                                  <button onClick={() => handleViewSession(sessionForCard!)}>
                                    View Session
                                  </button>
                                )}
                                <button
                                  onClick={() => {
                                    closeCardMenu()
                                    setEditingCard(card)
                                  }}
                                >
                                  Edit
                                </button>
                                {card.worker_session_id && cardStep !== 'backlog' && (
                                  <button onClick={() => handleStopWorker(card)}>
                                    Stop Worker
                                  </button>
                                )}
                                {!card.worker_session_id &&
                                  cardStep !== 'backlog' &&
                                  cardStep !== 'done' &&
                                  cardStep !== 'wont_do' && (
                                    <button onClick={() => handleRestartWorker(card)}>
                                      Restart Worker
                                    </button>
                                  )}
                                {card.step !== 'done' && card.step !== 'wont_do' && (
                                  <button
                                    className="danger"
                                    onClick={() => handleCancelWontDo(card)}
                                  >
                                    Cancel as Won't Do
                                  </button>
                                )}
                                <button
                                  className="danger"
                                  onClick={() => {
                                    closeCardMenu()
                                    setConfirmDeleteId(card.id)
                                  }}
                                >
                                  Delete
                                </button>
                              </div>
                            )}
                          </div>
                        </div>
                        {expanded && (
                          <div className="kanban-card-body">
                            {description ? (
                              <div className="kanban-card-desc" data-testid="card-description">
                                <SafeMarkdown className="kanban-card-desc-markdown">
                                  {description}
                                </SafeMarkdown>
                              </div>
                            ) : (
                              <div className="kanban-card-desc kanban-card-desc-empty">
                                No description
                              </div>
                            )}
                            {card.blocked && (
                              <div className="kanban-card-blocked">
                                <strong>Blocked</strong>
                                {card.block_reason ? `: ${card.block_reason}` : ''}
                              </div>
                            )}
                            <div className="kanban-card-meta">
                              {todos && todos.length > 0 && (
                                <span
                                  className="card-todo-badge"
                                  data-testid="card-todo-badge"
                                  title={`${todoDone} of ${todos.length} tasks done`}
                                >
                                  {todoDone}/{todos.length}
                                </span>
                              )}
                              {card.worker_session_id && cardStep !== 'backlog' && (
                                <span className="kanban-card-worker" title="Worker active">
                                  <span className="kanban-card-worker-dot" />
                                  Worker
                                </span>
                              )}
                              {pendingDeps.length > 0 && (
                                <span
                                  className="waiting-indicator"
                                  title={`Waiting on: ${pendingDeps
                                    .map((c) => c.title)
                                    .join(', ')}`}
                                >
                                  Waiting on {pendingDeps.length}{' '}
                                  {pendingDeps.length === 1 ? 'dep' : 'deps'}
                                </span>
                              )}
                            </div>
                            <div className="kanban-card-buttons" data-no-toggle>
                              {sessionVisible && (
                                <button
                                  type="button"
                                  className="kanban-card-action-btn"
                                  data-testid="card-quick-session"
                                  title="Open session"
                                  aria-label="Open session"
                                  onClick={(e) => {
                                    e.stopPropagation()
                                    handleViewSession(sessionForCard!)
                                  }}
                                >
                                  <svg
                                    width="14"
                                    height="14"
                                    viewBox="0 0 24 24"
                                    fill="none"
                                    stroke="currentColor"
                                    strokeWidth="2"
                                    strokeLinecap="round"
                                    strokeLinejoin="round"
                                    aria-hidden="true"
                                  >
                                    <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
                                  </svg>
                                  <span>Open</span>
                                </button>
                              )}
                              <button
                                type="button"
                                className="kanban-card-action-btn"
                                data-testid="card-quick-view"
                                title="View details"
                                aria-label="View details"
                                onClick={(e) => {
                                  e.stopPropagation()
                                  setSelectedCard(card)
                                }}
                              >
                                <svg
                                  width="14"
                                  height="14"
                                  viewBox="0 0 24 24"
                                  fill="none"
                                  stroke="currentColor"
                                  strokeWidth="2"
                                  strokeLinecap="round"
                                  strokeLinejoin="round"
                                  aria-hidden="true"
                                >
                                  <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" />
                                  <circle cx="12" cy="12" r="3" />
                                </svg>
                                <span>View</span>
                              </button>
                              <button
                                type="button"
                                className="kanban-card-action-btn"
                                data-testid="card-quick-edit"
                                title="Edit card"
                                aria-label="Edit card"
                                onClick={(e) => {
                                  e.stopPropagation()
                                  setEditingCard(card)
                                }}
                              >
                                <svg
                                  width="14"
                                  height="14"
                                  viewBox="0 0 24 24"
                                  fill="none"
                                  stroke="currentColor"
                                  strokeWidth="2"
                                  strokeLinecap="round"
                                  strokeLinejoin="round"
                                  aria-hidden="true"
                                >
                                  <path d="M12 20h9" />
                                  <path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5z" />
                                </svg>
                                <span>Edit</span>
                              </button>
                            </div>
                          </div>
                        )}
                      </div>
                    )
                  })}
                </div>
              </div>
            )
          })}
        </div>
      </div>

      {/* Project-level todo roll-up across every card's worker session.
          Docked at the bottom of the board, outside the scroll area —
          same pattern as the chat-side TodoPanel. */}
      <ProjectTodoSummary cards={cards} todosByCard={todosByCard} />

      {confirmDeleteId && (
        <Modal onClose={() => setConfirmDeleteId(null)} maxWidth={360}>
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
        </Modal>
      )}

      {selectedCard && (
        <Modal onClose={() => setSelectedCard(null)}>
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
              <div className="card-detail-row card-detail-row-description">
                <span className="card-detail-label">Description</span>
                <SafeMarkdown className="card-detail-description">
                  {selectedCard.description}
                </SafeMarkdown>
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
                      <span className="card-report-date">{r.date?.split('T')[0] ?? r.folder}</span>
                    </button>
                  ))}
                </div>
              </div>
            )}
          </div>
          <div className="card-detail-actions">
            {(selectedCard.worker_session_id || selectedCard.last_worker_session_id) &&
              normalizeStep(selectedCard.step) !== 'backlog' && (
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
        </Modal>
      )}

      {editingCard && (
        <CardFormModal
          mode="edit"
          projectId={projectId}
          card={editingCard}
          onClose={() => {
            setEditingCard(null)
            fetchCards(projectId)
          }}
        />
      )}

      {editingProject && project && (
        <EditProjectModal project={project} onClose={() => setEditingProject(false)} />
      )}
    </div>
  )
}
