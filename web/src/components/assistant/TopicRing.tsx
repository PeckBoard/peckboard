import {
  type CSSProperties,
  type ReactNode,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import type { AssistantTopic, TopicKind } from '../../voice/assistantContract'
import type { AbsorbingTopic } from './useAbsorbAnimation'

interface TopicRingProps {
  topics: AssistantTopic[]
  currentId: string | null
  absorbing: AbsorbingTopic | null
  absorbDurationMs: number
  onFocus: (id: string) => void
  children: ReactNode
}

interface RingGeometry {
  width: number
  height: number
  centerX: number
  centerY: number
  orbWidth: number
  orbHeight: number
  mobile: boolean
}

interface Placement {
  x: number
  y: number
  terminalX: number
  terminalY: number
  path: string
  absorbViaX: number
  absorbViaY: number
  absorbEndX: number
  absorbEndY: number
  absorbPath: string
}

type KindedTopic = AssistantTopic & { kind?: TopicKind }

const INITIAL_GEOMETRY: RingGeometry = {
  width: 1200,
  height: 720,
  centerX: 600,
  centerY: 345,
  orbWidth: 570,
  orbHeight: 570,
  mobile: false,
}

function topicKind(topic: AssistantTopic): TopicKind {
  return (topic as KindedTopic).kind ?? 'conversation'
}

function topicIcon(kind: TopicKind) {
  return (
    <span className="assistant-topic-kind" aria-hidden>
      {kind === 'conversation' ? (
        <svg viewBox="0 0 16 16">
          <path d="M2.5 4h11v7H8.2L5 13v-2H2.5z" />
        </svg>
      ) : kind === 'session' ? (
        <svg viewBox="0 0 16 16">
          <path d="M3 3.5h10v7H8l-3.2 2v-2H3z" />
        </svg>
      ) : (
        <svg viewBox="0 0 16 16">
          <path d="M2.5 4.5h4l1.2 1.4h5.8v6.6h-11z" />
        </svg>
      )}
    </span>
  )
}

function buildGeometry(width: number, measuredHeight: number, topicCount: number): RingGeometry {
  const mobile = width < 768
  if (mobile) {
    const orbWidth = Math.min(width - 92, 300)
    const orbHeight = orbWidth
    const centerY = 72 + orbHeight / 2
    const rows = Math.ceil(topicCount / 2)
    const height = Math.max(measuredHeight, centerY + orbHeight / 2 + 76 + rows * 78)
    return {
      width,
      height,
      centerX: width / 2,
      centerY,
      orbWidth,
      orbHeight,
      mobile,
    }
  }

  const orbWidth = Math.min(610, Math.max(390, width * 0.42), measuredHeight * 0.72)
  const orbHeight = orbWidth
  return {
    width,
    height: measuredHeight,
    centerX: width / 2,
    centerY: measuredHeight * 0.49,
    orbWidth,
    orbHeight,
    mobile,
  }
}

function buildPlacement(geometry: RingGeometry, index: number, count: number): Placement {
  const { centerX, centerY, orbWidth, orbHeight, mobile, width } = geometry

  if (mobile) {
    const column = index % 2
    const row = Math.floor(index / 2)
    const x = width * (column === 0 ? 0.28 : 0.72)
    const y = centerY + orbHeight / 2 + 70 + row * 78
    const angle = column === 0 ? 2.28 : 0.86
    const startX = centerX + Math.cos(angle) * orbWidth * 0.48
    const startY = centerY + Math.sin(angle) * orbHeight * 0.48
    const stubX = x + (column === 0 ? 68 : -68)
    const lineY = y
    return {
      x,
      y,
      path: `M ${startX} ${startY} Q ${stubX} ${startY + 30} ${stubX} ${lineY} L ${x} ${lineY}`,
      absorbPath: `M ${x} ${y} L ${stubX} ${lineY} Q ${stubX} ${startY + 30} ${startX} ${startY} L ${centerX} ${centerY}`,
      absorbViaX: stubX,
      absorbViaY: lineY,
      absorbEndX: centerX,
      absorbEndY: centerY,
      terminalX: x,
      terminalY: lineY,
    }
  }
  const angleOffset = count % 2 === 0 ? Math.PI / count / 2 : 0
  const angle = -Math.PI / 2 + angleOffset + (Math.PI * 2 * index) / Math.max(1, count)
  const cos = Math.cos(angle)
  const sin = Math.sin(angle)
  const rimX = centerX + (orbWidth / 2) * cos
  const rimY = centerY + (orbHeight / 2) * sin
  const radialX = centerX + (orbWidth / 2 + Math.min(118, width * 0.07)) * cos
  const radialY = centerY + (orbHeight / 2 + 72) * sin
  const rawX = radialX + (cos >= 0 ? 78 : -78)
  const x = Math.min(width - 112, Math.max(112, rawX))
  const y = Math.min(geometry.height - 52, Math.max(50, radialY))
  const lineY = y + 20
  const lineEndX = x + (cos >= 0 ? 98 : -98)
  const direction = cos >= 0 ? 1 : -1
  const controlX = rimX + cos * 46
  const controlY = rimY + sin * 38
  const path = `M ${rimX} ${rimY} C ${controlX} ${controlY} ${radialX - direction * 24} ${lineY} ${radialX} ${lineY} L ${lineEndX} ${lineY}`
  const absorbPath = `M ${x} ${y} L ${lineEndX} ${lineY} L ${radialX} ${lineY} C ${controlX} ${controlY} ${rimX} ${rimY} ${centerX} ${centerY}`
  return {
    x,
    y,
    path,
    absorbPath,
    absorbViaX: radialX,
    absorbViaY: lineY,
    absorbEndX: centerX,
    absorbEndY: centerY,
    terminalX: lineEndX,
    terminalY: lineY,
  }
}

export default function TopicRing({
  topics,
  currentId,
  absorbing,
  absorbDurationMs,
  onFocus,
  children,
}: TopicRingProps) {
  const rootRef = useRef<HTMLDivElement>(null)
  const [geometry, setGeometry] = useState(INITIAL_GEOMETRY)
  const [activeTopicId, setActiveTopicId] = useState<string | null>(null)

  const ringTopics = useMemo(
    () => topics.filter((topic) => topic.id !== currentId),
    [currentId, topics],
  )
  const layoutTopics = useMemo(() => {
    if (!absorbing || ringTopics.some((topic) => topic.id === absorbing.topic.id)) {
      return ringTopics
    }
    const order = new Map(topics.map((topic, index) => [topic.id, index]))
    return [...ringTopics, absorbing.topic].sort(
      (a, b) => (order.get(a.id) ?? 0) - (order.get(b.id) ?? 0),
    )
  }, [absorbing, ringTopics, topics])

  useLayoutEffect(() => {
    const root = rootRef.current
    if (!root) return
    const measure = () => {
      const bounds = root.getBoundingClientRect()
      setGeometry(buildGeometry(bounds.width, Math.max(bounds.height, 620), ringTopics.length))
    }
    const observer = new ResizeObserver(measure)
    observer.observe(root)
    measure()
    return () => observer.disconnect()
  }, [ringTopics.length])

  const placements = useMemo(() => {
    const result = new Map<string, Placement>()
    layoutTopics.forEach((topic, index) => {
      result.set(topic.id, buildPlacement(geometry, index, layoutTopics.length))
    })
    return result
  }, [geometry, layoutTopics])

  const stageStyle = {
    '--assistant-orb-width': `${geometry.orbWidth}px`,
    '--assistant-orb-height': `${geometry.orbHeight}px`,
    '--assistant-orb-x': `${geometry.centerX}px`,
    '--assistant-orb-y': `${geometry.centerY}px`,
    minHeight: geometry.mobile ? geometry.height : undefined,
  } as CSSProperties

  const absorbPlacement = absorbing ? placements.get(absorbing.topic.id) : null

  return (
    <div
      ref={rootRef}
      className="assistant-topic-stage"
      style={stageStyle}
      role="group"
      aria-label="Assistant topics"
    >
      <svg
        className="assistant-connectors"
        viewBox={`0 0 ${geometry.width} ${geometry.height}`}
        preserveAspectRatio="none"
        aria-hidden
      >
        {ringTopics.map((topic) => {
          const placement = placements.get(topic.id)
          if (!placement) return null
          return (
            <g
              key={topic.id}
              className="assistant-connector-group"
              data-active={activeTopicId === topic.id ? 'true' : undefined}
              data-kind={topicKind(topic)}
            >
              <path className="assistant-connector-shadow" d={placement.path} />
              <path className="assistant-connector" d={placement.path} />
              <path className="assistant-connector-energy" d={placement.path} />
              <circle
                className="assistant-connector-node"
                cx={placement.terminalX}
                cy={placement.terminalY}
                r="3.2"
              />
            </g>
          )
        })}

        {absorbing && absorbPlacement && (
          <g className="assistant-connector-group assistant-connector-absorbing">
            <path className="assistant-connector-shadow" d={absorbPlacement.path} />
            <path className="assistant-connector" d={absorbPlacement.path} />
            <path className="assistant-connector-energy" d={absorbPlacement.path} />
          </g>
        )}
      </svg>

      {absorbing && absorbPlacement && (
        <div
          key={absorbing.animationKey}
          className="assistant-absorb-label"
          data-testid="assistant-absorbing"
          style={
            {
              '--absorb-start-x': `${absorbPlacement.x}px`,
              '--absorb-start-y': `${absorbPlacement.y}px`,
              '--absorb-via-x': `${absorbPlacement.absorbViaX}px`,
              '--absorb-via-y': `${absorbPlacement.absorbViaY}px`,
              '--absorb-end-x': `${absorbPlacement.absorbEndX}px`,
              '--absorb-end-y': `${absorbPlacement.absorbEndY}px`,
              animationDuration: `${absorbDurationMs}ms`,
            } as CSSProperties
          }
          aria-hidden
        >
          {absorbing.topic.title}
        </div>
      )}
      <div className="assistant-orb-position">{children}</div>

      <div className="assistant-topic-buttons">
        {ringTopics.map((topic) => {
          const placement = placements.get(topic.id)
          if (!placement) return null
          const kind = topicKind(topic)
          const title = [topic.summary, topic.status].filter(Boolean).join(' — ') || topic.title
          return (
            <button
              key={topic.id}
              type="button"
              className="assistant-topic"
              data-testid="assistant-topic"
              data-topic-id={topic.id}
              data-kind={kind}
              data-tone={placement.y > geometry.height * 0.58 ? 'dark' : 'light'}
              title={title}
              style={{ left: placement.x, top: placement.y }}
              onPointerEnter={() => setActiveTopicId(topic.id)}
              onPointerLeave={() => setActiveTopicId(null)}
              onFocus={() => setActiveTopicId(topic.id)}
              onBlur={() => setActiveTopicId(null)}
              onClick={() => onFocus(topic.id)}
            >
              <span className="assistant-topic-heading">
                {topicIcon(kind)}
                <span className="assistant-topic-title">{topic.title}</span>
              </span>
              {kind !== 'conversation' && topic.status && (
                <span className="assistant-topic-status">{topic.status}</span>
              )}
            </button>
          )
        })}
      </div>
    </div>
  )
}
