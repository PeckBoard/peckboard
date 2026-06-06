import { useCallback, useEffect, useRef, useState } from 'react'
import type { Event } from '../types/api'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import InputBar from './InputBar'
import ToolUseBlock from './ToolUseBlock'

interface ChatViewProps {
  sessionId: string
}

/** A display item derived from one or more raw events. */
type DisplayItem =
  | { type: 'user'; text: string; key: string }
  | { type: 'assistant'; text: string; key: string }
  | { type: 'tool'; toolName: string; input?: Record<string, unknown>; output?: Record<string, unknown>; error?: string; isRunning: boolean; key: string }
  | { type: 'status'; text: string; key: string }
  | { type: 'system'; text: string; key: string }
  | { type: 'step'; label: string; key: string }

function buildDisplayItems(events: Event[]): DisplayItem[] {
  const items: DisplayItem[] = []
  let assistantBuffer = ''
  let assistantKey = ''

  const flushAssistant = () => {
    if (assistantBuffer) {
      items.push({ type: 'assistant', text: assistantBuffer, key: assistantKey })
      assistantBuffer = ''
      assistantKey = ''
    }
  }

  // Track open tools by their tool_use_id
  const openTools = new Map<string, number>() // tool_use_id -> index in items

  for (const ev of events) {
    switch (ev.kind) {
      case 'user': {
        flushAssistant()
        const text = (ev.data.text as string) ?? JSON.stringify(ev.data)
        items.push({ type: 'user', text, key: ev.id })
        break
      }
      case 'agent-text': {
        const chunk = (ev.data.text as string) ?? ''
        if (!assistantKey) assistantKey = ev.id
        assistantBuffer += chunk
        break
      }
      case 'agent-tool-start': {
        flushAssistant()
        const toolName = (ev.data.tool_name as string) ?? 'tool'
        const input = (ev.data.input as Record<string, unknown>) ?? undefined
        const toolUseId = (ev.data.tool_use_id as string) ?? ev.id
        const idx = items.length
        items.push({ type: 'tool', toolName, input, isRunning: true, key: ev.id })
        openTools.set(toolUseId, idx)
        break
      }
      case 'agent-tool-end': {
        flushAssistant()
        const toolUseId = (ev.data.tool_use_id as string) ?? ''
        const idx = openTools.get(toolUseId)
        if (idx !== undefined) {
          const existing = items[idx] as Extract<DisplayItem, { type: 'tool' }>
          const errorText = ev.data.error as string | undefined
          const output = (ev.data.output as Record<string, unknown>) ?? undefined
          items[idx] = { ...existing, isRunning: false, output, error: errorText }
          openTools.delete(toolUseId)
        } else {
          const toolName = (ev.data.tool_name as string) ?? 'tool'
          const errorText = ev.data.error as string | undefined
          const output = (ev.data.output as Record<string, unknown>) ?? undefined
          items.push({ type: 'tool', toolName, output, error: errorText, isRunning: false, key: ev.id })
        }
        break
      }
      case 'agent-start': {
        flushAssistant()
        items.push({ type: 'status', text: 'Agent started', key: ev.id })
        break
      }
      case 'agent-end': {
        flushAssistant()
        items.push({ type: 'status', text: 'Agent finished', key: ev.id })
        break
      }
      case 'system': {
        flushAssistant()
        const text = (ev.data.text as string) ?? (ev.data.message as string) ?? JSON.stringify(ev.data)
        items.push({ type: 'system', text, key: ev.id })
        break
      }
      case 'step-change': {
        flushAssistant()
        const label = (ev.data.step as string) ?? (ev.data.label as string) ?? 'Step'
        items.push({ type: 'step', label, key: ev.id })
        break
      }
      default: {
        // Unknown event kinds: skip or render as system
        break
      }
    }
  }

  flushAssistant()
  return items
}

export default function ChatView({ sessionId }: ChatViewProps) {
  const [events, setEvents] = useState<Event[]>([])
  const [loading, setLoading] = useState(true)
  const scrollRef = useRef<HTMLDivElement>(null)
  const userScrolledUp = useRef(false)

  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)

  // Fetch initial events
  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setEvents([])
    userScrolledUp.current = false

    authedFetch(`/api/sessions/${sessionId}/events`)
      .then((res) => (res.ok ? res.json() : []))
      .then((data: Event[]) => {
        if (!cancelled) {
          setEvents(data)
          setLoading(false)
        }
      })
      .catch(() => {
        if (!cancelled) setLoading(false)
      })

    return () => {
      cancelled = true
    }
  }, [sessionId])

  // Subscribe to WS events for this session
  useEffect(() => {
    subscribe(sessionId)

    const listener = (event: Event) => {
      if (event.session_id === sessionId) {
        setEvents((prev) => {
          // Dedupe by id
          if (prev.some((e) => e.id === event.id)) return prev
          return [...prev, event]
        })
      }
    }

    addEventListener(listener)

    return () => {
      removeEventListener(listener)
      unsubscribe(sessionId)
    }
  }, [sessionId, subscribe, unsubscribe, addEventListener, removeEventListener])

  // Scroll handling
  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    const threshold = 60
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < threshold
    userScrolledUp.current = !atBottom
  }, [])

  useEffect(() => {
    if (!userScrolledUp.current) {
      const el = scrollRef.current
      if (el) {
        el.scrollTop = el.scrollHeight
      }
    }
  }, [events])

  const displayItems = buildDisplayItems(events)

  // Determine if agent is working
  const agentWorking = (() => {
    for (let i = events.length - 1; i >= 0; i--) {
      const kind = events[i].kind
      if (kind === 'agent-start') return true
      if (kind === 'agent-end') return false
    }
    return false
  })()

  if (loading) {
    return (
      <div className="chat-container">
        <div className="chat-loading">Loading events...</div>
      </div>
    )
  }

  return (
    <div className="chat-container">
      <div className="chat-messages" ref={scrollRef} onScroll={handleScroll}>
        {displayItems.length === 0 && (
          <div className="chat-empty">No messages yet. Send one below.</div>
        )}
        {displayItems.map((item) => {
          switch (item.type) {
            case 'user':
              return (
                <div key={item.key} className="chat-row chat-row-user">
                  <div className="chat-bubble chat-bubble-user">{item.text}</div>
                </div>
              )
            case 'assistant':
              return (
                <div key={item.key} className="chat-row chat-row-assistant">
                  <div className="chat-bubble chat-bubble-assistant">{item.text}</div>
                </div>
              )
            case 'tool':
              return (
                <div key={item.key} className="chat-row chat-row-tool">
                  <ToolUseBlock
                    toolName={item.toolName}
                    input={item.input}
                    output={item.output}
                    error={item.error}
                    isRunning={item.isRunning}
                  />
                </div>
              )
            case 'status':
              return (
                <div key={item.key} className="chat-row chat-row-status">
                  <span className="chat-status">{item.text}</span>
                </div>
              )
            case 'system':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-system-notice">{item.text}</div>
                </div>
              )
            case 'step':
              return (
                <div key={item.key} className="chat-row chat-row-step">
                  <div className="chat-step-divider">
                    <span>{item.label}</span>
                  </div>
                </div>
              )
          }
        })}
      </div>
      <InputBar sessionId={sessionId} agentWorking={agentWorking} />
    </div>
  )
}
