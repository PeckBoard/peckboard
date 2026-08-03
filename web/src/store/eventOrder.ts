import type { Event } from '../types/api'

/**
 * Append an event to a session's log, keeping it ordered by `seq`.
 *
 * Resume/resync replays can deliver events OLDER than the current tail: the
 * server's socket loop can forward a live broadcast between the client's
 * `subscribe` and `resume` frames, so replayed seqs land AFTER a newer live
 * one. A straight push leaves an old event sitting last, which corrupts every
 * reader that walks the array from the end (`latestTodoSnapshot`) and renders
 * old turns below new ones. The push is the hot path; re-sort only when an
 * out-of-order seq actually arrives.
 */
export function appendEventOrdered(existing: Event[], event: Event): Event[] {
  const last = existing[existing.length - 1]
  if (last !== undefined && event.seq < last.seq) {
    return [...existing, event].sort((a, b) => a.seq - b.seq)
  }
  return [...existing, event]
}

/**
 * Fold an event's seq into the per-session last-seq watermark. Never regresses:
 * a replayed older seq must not overwrite the live tail, or the persisted value
 * forces redundant replay on the next resume (and the replay lands below the
 * tail all over again).
 */
export function nextLastSeq(prev: number | undefined, seq: number): number {
  return Math.max(prev ?? 0, seq)
}
