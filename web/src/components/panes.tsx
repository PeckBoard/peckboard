import { useLayoutEffect, useMemo, useRef } from 'react'
import { useSessionsStore } from '../store/sessions'
import { useUsageStore } from '../store/usage'
import { ChatRow } from './ChatView'
import {
  EMPTY_EVENTS,
  createDisplayItemsFolder,
  deriveAgentStatus,
  getStatusDotClass,
  getStatusLabel,
  type AgentStatus,
} from './chat/events'

/** Status dot (+ "Done" badge once the agent finished a turn and went idle)
 *  for a session pane header. Subscribes to the session's own events, so a
 *  streaming pane re-renders only this chip, not the whole split layout. */
export function SessionPaneStatus({
  sessionId,
  showDone = false,
}: {
  sessionId: string
  showDone?: boolean
}) {
  const status = useSessionsStore((s) =>
    deriveAgentStatus(s.eventsBySession[sessionId] ?? EMPTY_EVENTS),
  )
  const finished = useSessionsStore(
    (s) =>
      showDone &&
      (s.eventsBySession[sessionId] ?? EMPTY_EVENTS).some((e) => e.kind === 'agent-end'),
  )
  return <PaneStatus status={status} done={finished && status === 'idle'} />
}

export function PaneStatus({ status, done }: { status: AgentStatus; done?: boolean }) {
  return (
    <>
      <span
        className={getStatusDotClass(status)}
        role="img"
        aria-label={getStatusLabel(status)}
        data-testid="split-pane-status"
        data-status={status}
      />
      {done && (
        <span className="split-pane-badge" data-testid="split-pane-badge">
          Done
        </span>
      )}
    </>
  )
}

/** Read-only pane for a Claude-native subagent (built-in Agent / Task tool).
 *  Its events live in the PARENT's stream, tagged with `parentToolUseId`;
 *  this folds just those, with the same row renderer as the chat feed. */
export function NativeSubagentPane({
  parentSessionId,
  toolUseId,
  subagentType,
  description,
  running,
}: {
  parentSessionId: string
  toolUseId: string
  subagentType: string
  description: string
  running: boolean
}) {
  const events = useSessionsStore((s) => s.eventsBySession[parentSessionId] ?? EMPTY_EVENTS)
  const costTable = useUsageStore((s) => s.costTable)
  const fold = useMemo(() => createDisplayItemsFolder({ subagentOf: toolUseId }), [toolUseId])
  const items = useMemo(() => fold(events), [fold, events])
  const feedRef = useRef<HTMLDivElement | null>(null)
  const followRef = useRef(true)
  useLayoutEffect(() => {
    const el = feedRef.current
    if (el && followRef.current) el.scrollTop = el.scrollHeight
  }, [items])
  return (
    <div className="native-pane" data-testid="native-subagent-pane" data-tool-use-id={toolUseId}>
      <div className="native-pane-meta">
        {subagentType ? `${subagentType} · ` : ''}
        {description || 'Sub-agent'} · {running ? 'Running' : 'Finished'} · read-only
      </div>
      <div
        className="native-pane-feed"
        ref={feedRef}
        role="region"
        aria-label="Subagent transcript"
        onScroll={(e) => {
          const el = e.currentTarget
          followRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60
        }}
      >
        {items.length === 0 ? (
          <div className="chat-empty">
            {running ? 'Waiting for the subagent…' : 'No subagent output.'}
          </div>
        ) : (
          items.map((item) => (
            <ChatRow key={item.key} item={item} sessionId={parentSessionId} costTable={costTable} />
          ))
        )}
      </div>
    </div>
  )
}
