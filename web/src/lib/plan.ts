import { authedFetch } from '../store/auth'

/** Latest plan id for a card or session, or null when none exists (so a menu
 *  item can render disabled). */
export async function fetchPlanId(params: {
  cardId?: string
  sessionId?: string
}): Promise<string | null> {
  const q = params.cardId
    ? `card_id=${encodeURIComponent(params.cardId)}`
    : `session_id=${encodeURIComponent(params.sessionId ?? '')}`
  try {
    const res = await authedFetch(`/api/plans?${q}`)
    if (res.status === 204 || !res.ok) return null
    const data = (await res.json()) as { plan?: { id: string } }
    return data.plan?.id ?? null
  } catch {
    return null
  }
}

/** Navigate to the full-page plan viewer at /plan/<id>. Uses pushState +
 *  a synthetic popstate so App's router picks it up without prop threading. */
export function openPlan(planId: string) {
  window.history.pushState(null, '', `/plan/${planId}`)
  window.dispatchEvent(new PopStateEvent('popstate'))
}
