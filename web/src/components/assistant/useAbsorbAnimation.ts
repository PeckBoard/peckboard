import { useEffect, useRef, useState } from 'react'
import type { AssistantTopic } from '../../voice/assistantContract'

const ABSORB_DURATION_MS = 1100

export interface AbsorbingTopic {
  topic: AssistantTopic
  animationKey: number
}

export function useAbsorbAnimation(topics: AssistantTopic[], currentId: string | null) {
  const [displayedId, setDisplayedId] = useState<string | null>(currentId)
  const [absorbing, setAbsorbing] = useState<AbsorbingTopic | null>(null)
  const previousId = useRef<string | null>(currentId)
  const topicsRef = useRef(topics)

  useEffect(() => {
    topicsRef.current = topics
  }, [topics])
  const animationKey = useRef(0)

  useEffect(() => {
    if (previousId.current === currentId) return
    previousId.current = currentId

    const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches
    const topic = currentId ? topicsRef.current.find((item) => item.id === currentId) : null

    if (!topic || reducedMotion) {
      setAbsorbing(null)
      setDisplayedId(currentId)
      return
    }

    animationKey.current += 1
    setAbsorbing({ topic, animationKey: animationKey.current })

    const timer = window.setTimeout(() => {
      setDisplayedId(currentId)
      setAbsorbing(null)
    }, ABSORB_DURATION_MS)

    return () => window.clearTimeout(timer)
  }, [currentId])

  return { absorbing, displayedId, durationMs: ABSORB_DURATION_MS }
}
