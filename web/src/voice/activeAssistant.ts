/**
 * Which assistant the browser is talking to right now. The voice store sets
 * it; every `/api/voice/*` call goes through `withAssistant` so the server
 * answers for that assistant instead of the default one.
 *
 * A plain module value, not store state: the stores that call the voice
 * routes import the voice store already, and reading the id from here keeps
 * that import one-way.
 */
let activeId: string | null = null

export function setActiveAssistantId(id: string | null): void {
  activeId = id
}

/** `url` with `assistant=<active id>` added; unchanged before one is known. */
export function withAssistant(url: string): string {
  if (!activeId) return url
  const sep = url.includes('?') ? '&' : '?'
  return `${url}${sep}assistant=${encodeURIComponent(activeId)}`
}
