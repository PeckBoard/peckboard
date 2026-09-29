import { useEffect, useSyncExternalStore } from 'react'

/**
 * One question modal at a time.
 *
 * Split panes and the views grid mount several ChatViews; each decides on
 * its own that it wants the modal, so without a shared owner two could
 * stack (a question lands in the focused pane while an unfocused pane's
 * modal, opened from its "Answer" row, is still up). The first claimant
 * holds the slot until it lets go; the rest keep their inline row and get
 * their turn in order.
 */

let owner: string | null = null
const waiters: string[] = []
const listeners = new Set<() => void>()

function notify() {
  for (const l of listeners) l()
}

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

function claim(id: string) {
  if (!waiters.includes(id)) waiters.push(id)
  if (owner === null) owner = waiters[0]
  notify()
}

function release(id: string) {
  const at = waiters.indexOf(id)
  if (at >= 0) waiters.splice(at, 1)
  if (owner === id) owner = waiters[0] ?? null
  notify()
}

/**
 * Whether this ChatView may render the question modal. Pass `want` while
 * the view has a question it would show; the return is true only for the
 * one view holding the slot. `id` must be unique per mounted view (not
 * the session id — the views grid can show one session twice).
 */
export function useQuestionModalSlot(id: string, want: boolean): boolean {
  const current = useSyncExternalStore(subscribe, () => owner)
  useEffect(() => {
    if (!want) return
    claim(id)
    return () => release(id)
  }, [id, want])
  return want && current === id
}
