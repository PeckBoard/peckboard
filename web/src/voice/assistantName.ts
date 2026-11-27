/**
 * Call by name: does an utterance open by addressing one of the assistants?
 *
 * "Hey Nova, how's the build" → Nova, "how's the build". The name must be
 * the first thing said (after an optional greeting): a name in the middle of
 * a sentence is talk about an assistant, not to it.
 *
 * Matching is on a speech transcript, so it ignores case, accents and
 * punctuation. After a greeting ("hey …") a single-word name of five or
 * more letters may also be one letter off, which is how recognisers usually
 * get an unfamiliar name wrong; without a greeting the name must match
 * exactly, or ordinary sentences would be taken for a call.
 */

export interface AddressedAssistant {
  id: string
  /** What was said after the name; empty when only the name was said. */
  rest: string
}

const GREETINGS = new Set(['hey', 'hi', 'hello', 'ok', 'okay', 'yo'])

function fold(s: string): string {
  return s.normalize('NFD').replace(/\p{M}/gu, '').toLowerCase()
}

/** Letters and digits of a word, lower-cased, without accents. */
function bare(word: string): string {
  return fold(word).replace(/[^\p{L}\p{N}]/gu, '')
}

/** True when `a` and `b` differ by at most one inserted, removed or changed
 *  letter. */
function withinOneEdit(a: string, b: string): boolean {
  if (a === b) return true
  if (Math.abs(a.length - b.length) > 1) return false
  const [short, long] = a.length <= b.length ? [a, b] : [b, a]
  let i = 0
  while (i < short.length && short[i] === long[i]) i++
  if (short.length === long.length) return short.slice(i + 1) === long.slice(i + 1)
  return short.slice(i) === long.slice(i + 1)
}

export function addressedAssistant(
  text: string,
  assistants: { id: string; name: string }[],
): AddressedAssistant | null {
  const words = text.trim().split(/\s+/).filter(Boolean)
  if (words.length === 0) return null
  const greeted = words.length > 1 && GREETINGS.has(bare(words[0]))
  const start = greeted ? 1 : 0

  // Longest name first, so "Nova Prime" wins over "Nova".
  const byLength = [...assistants].sort((a, b) => b.name.length - a.name.length)
  for (const a of byLength) {
    const nameWords = a.name.trim().split(/\s+/).map(bare).filter(Boolean)
    if (nameWords.length === 0 || start + nameWords.length > words.length) continue
    const said = words.slice(start, start + nameWords.length).map(bare)
    const exact = said.every((w, i) => w === nameWords[i])
    const close =
      greeted &&
      nameWords.length === 1 &&
      nameWords[0].length >= 5 &&
      withinOneEdit(said[0], nameWords[0])
    if (!exact && !close) continue
    const rest = words
      .slice(start + nameWords.length)
      .join(' ')
      .replace(/^[\s,.:;!?-]+/, '')
    return { id: a.id, rest }
  }
  return null
}
