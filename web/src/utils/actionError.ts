/**
 * Turn a thrown value from a store action into copy a human can read.
 *
 * The session store rejects with `new Error(body.error || '...')`, so the
 * message is usually already a sentence written for people. It isn't
 * guaranteed though: a proxy or a panic can put a JSON blob, an HTML page
 * or a stack trace on the wire, and dumping that into a dialog is worse
 * than saying nothing. Anything that doesn't look like a short sentence
 * falls back to the caller's copy.
 */
export function describeActionError(err: unknown, fallback: string): string {
  // A rejected fetch (server down, offline, DNS) throws a TypeError whose
  // message is browser jargon — "Failed to fetch", "NetworkError when
  // attempting to fetch resource". Never show that to a user.
  if (err instanceof TypeError) return fallback
  const raw = err instanceof Error ? err.message : typeof err === 'string' ? err : ''
  const text = raw.trim()
  if (!text) return fallback
  // Serialized bodies / markup — not for human eyes.
  if (/^[[{<]/.test(text)) return fallback
  // Multi-line or essay-length: almost certainly a trace or a body dump.
  if (text.length > 200 || text.includes('\n')) return fallback
  return text
}
