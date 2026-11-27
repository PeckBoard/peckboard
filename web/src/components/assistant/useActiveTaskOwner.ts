import { useMemo } from 'react'
import { useVoiceStore } from '../../store/voice'
import type { TaskOwner } from './upcoming'

/** The assistant being talked to, as the owner of scheduled tasks; `null`
 *  until the assistants have loaded. */
export function useActiveTaskOwner(): TaskOwner | null {
  const id = useVoiceStore((s) => s.assistantId)
  const isDefault = useVoiceStore(
    (s) => s.assistants.find((a) => a.id === s.assistantId)?.is_default ?? null,
  )
  return useMemo(() => (id && isDefault !== null ? { id, isDefault } : null), [id, isDefault])
}
