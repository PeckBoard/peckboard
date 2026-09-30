import type { Event } from '../types/api'

/** Prefix the backend puts on messages it injects into the voice session on
 *  behalf of other sessions (their questions / progress updates). */
export const RELAY_PREFIX = '[relay] '

export function isRelayText(text: string): boolean {
  return text.startsWith(RELAY_PREFIX)
}

export function stripRelayPrefix(text: string): string {
  return isRelayText(text) ? text.slice(RELAY_PREFIX.length) : text
}

/**
 * Turn markdown-ish assistant text into something worth reading aloud:
 * drop code blocks, URLs and markdown syntax, collapse whitespace.
 */
export function stripForSpeech(text: string): string {
  let t = text
  // Fenced code blocks → a short spoken placeholder.
  t = t.replace(/```[\s\S]*?```/g, ' code block ')
  // Inline code: keep the content, lose the ticks.
  t = t.replace(/`([^`]*)`/g, '$1')
  // Markdown links / images: keep the label.
  t = t.replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
  // Bare URLs.
  t = t.replace(/https?:\/\/\S+/g, ' link ')
  // Headings, blockquotes, list bullets, horizontal rules.
  t = t.replace(/^\s{0,3}#{1,6}\s+/gm, '')
  t = t.replace(/^\s{0,3}>\s?/gm, '')
  t = t.replace(/^\s*[-*+]\s+/gm, '')
  t = t.replace(/^\s*\d+\.\s+/gm, '')
  t = t.replace(/^\s*([-*_])\s*\1\s*\1[\s\-*_]*$/gm, '')
  // Emphasis / strikethrough markers.
  t = t.replace(/(\*\*|__|~~)(.*?)\1/g, '$2')
  t = t.replace(/(^|\s)[*_](\S.*?\S|\S)[*_](?=\s|$|[.,!?])/g, '$1$2')
  // Table pipes.
  t = t.replace(/^\s*\|.*\|\s*$/gm, (row) =>
    row
      .split('|')
      .map((c) => c.trim())
      .filter(Boolean)
      .join(', '),
  )
  t = t.replace(/^\s*[:\-| ]+\s*$/gm, '')
  return t.replace(/\s+/g, ' ').trim()
}

/**
 * Split streamed text into complete sentences ready to speak and a tail
 * that is still being generated. A sentence ends at `.`, `!`, `?`, `…`,
 * `:` or `;` followed by whitespace, or at a newline.
 */
export function takeSentences(buffer: string): { sentences: string[]; rest: string } {
  const sentences: string[] = []
  let rest = buffer
  const re = /[^.!?:;…\n]*(?:[.!?:;…]+["')\]]*(?=\s)|\n)/
  for (;;) {
    const m = re.exec(rest)
    if (!m || m.index !== 0) break
    const piece = m[0].trim()
    rest = rest.slice(m[0].length)
    if (piece) sentences.push(piece)
    rest = rest.replace(/^\s+/, '')
  }
  return { sentences, rest }
}

/** Tail length past which a still-open sentence is cut at a clause
 *  boundary, so a long sentence starts playing before it is finished. */
const CLAUSE_SPLIT_AT = 90

/**
 * Chunk streamed assistant text for live speech: complete sentences, plus —
 * when the unfinished tail grows long — its leading clauses (cut after a
 * comma / dash). The returned `rest` is still being generated.
 */
export function takeSpeakable(buffer: string): { chunks: string[]; rest: string } {
  const { sentences, rest: tail } = takeSentences(buffer)
  const chunks = [...sentences]
  let rest = tail
  while (rest.length > CLAUSE_SPLIT_AT) {
    const head = rest.slice(0, rest.length - 1)
    const cut = Math.max(head.lastIndexOf(', '), head.lastIndexOf(' — '), head.lastIndexOf(' - '))
    if (cut < 30) break
    chunks.push(rest.slice(0, cut + 1).trim())
    rest = rest.slice(cut + 1).replace(/^\s+/, '')
  }
  return { chunks, rest }
}

/** Lowercased words of `text`, punctuation dropped. */
export function speechWords(text: string): string[] {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}'\s]+/gu, ' ')
    .split(/\s+/)
    .map((w) => w.replace(/^'+|'+$/g, ''))
    .filter(Boolean)
}

/**
 * Share (0–1) of the words in recognized speech `heard` that occur in what
 * was just being spoken (`spoken`) — high means the microphone is most
 * likely picking up the assistant's own voice. Empty `heard` counts as 1.
 */
export function echoOverlap(heard: string, spoken: string[]): number {
  const words = speechWords(heard)
  if (words.length === 0) return 1
  const pool = new Set(spoken.flatMap(speechWords))
  if (pool.size === 0) return 0
  return words.filter((w) => pool.has(w)).length / words.length
}

export type VoiceTranscriptItem =
  | { kind: 'user'; key: string; text: string; ts: number; pending?: boolean }
  | { kind: 'relay'; key: string; text: string; ts: number }
  | { kind: 'assistant'; key: string; text: string; ts: number; streaming: boolean }

/**
 * Fold the voice session's raw events into transcript lines: user
 * utterances, backend-relayed updates, and assistant replies (one line per
 * turn, concatenating streamed `agent-text` chunks).
 */
export function foldVoiceTranscript(events: Event[]): VoiceTranscriptItem[] {
  const items: VoiceTranscriptItem[] = []
  let open: Extract<VoiceTranscriptItem, { kind: 'assistant' }> | null = null
  const close = () => {
    if (open) {
      open.streaming = false
      open = null
    }
  }
  for (const ev of events) {
    switch (ev.kind) {
      case 'user': {
        close()
        const text = typeof ev.data.text === 'string' ? ev.data.text : ''
        if (isRelayText(text)) {
          items.push({ kind: 'relay', key: ev.id, text: stripRelayPrefix(text), ts: ev.ts })
        } else {
          items.push({ kind: 'user', key: ev.id, text, ts: ev.ts })
        }
        break
      }
      case 'agent-text': {
        const chunk = typeof ev.data.text === 'string' ? ev.data.text : ''
        if (!open) {
          open = { kind: 'assistant', key: ev.id, text: '', ts: ev.ts, streaming: true }
          items.push(open)
        }
        open.text += chunk
        break
      }
      case 'agent-end':
        close()
        break
      default:
        break
    }
  }
  return items.filter((it) => it.kind !== 'assistant' || it.text.trim() !== '')
}
