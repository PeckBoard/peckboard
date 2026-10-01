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
/** Prefix on an utterance that talked over the assistant: the model must
 *  not restate the part of its reply the user never heard. */
export const INTERRUPT_MARKER =
  '[user interrupted; the rest of your previous reply was not heard, do not repeat it] '

export function stripInterruptMarker(text: string): string {
  return text.startsWith(INTERRUPT_MARKER) ? text.slice(INTERRUPT_MARKER.length) : text
}
/**
 * Inline pronunciation hint in the voice assistant's replies, misaki's
 * markup: `[Peckboard](/pˈɛkbɔɹd/)`. The chat shows the word; TTS speaks
 * the phonemes. With every word hinted, many phoneme strings are plain
 * ASCII (`[be](/bi/)`, `[a](/A/)`), so a hint is any `/…/` run free of the
 * characters paths and URLs are made of (`/ . : # ? = & %`, digits,
 * brackets). A bare relative link like `[docs](/docs/)` would read as a
 * hint too — an accepted cost; the voice assistant doesn't write those.
 * Mirrors `src/service/tts/hints.rs`.
 */
const HINT_SRC = String.raw`\[([^[\]\n]+)\]\(\/([^/()[\]\n.:#?=&%0-9]+)\/\)`
const HINT_RE = new RegExp(HINT_SRC, 'g')
const HINT_RE_ONE = new RegExp(`^${HINT_SRC}$`)
/** A hint missing its `/)` (`[Got](/ɡˈɑt [it]…`, `[Got](/ɡˈɑt)`): the word
 *  is kept and the stray phonemes dropped, as the server parses it. */
const BROKEN_HINT_RE = /\[([^[\]\n]+)\]\(\/[^/()[\]\n.:#?=&%0-9]+?(?:\)|(?=\s*(?:\[|\n|$)))/g
/** A hint still being streamed at the end of the text: `[word`, `[word](`,
 *  `[word](/pˈɛk`, … (bounded, so a stray `[` doesn't hold speech forever). */
const PARTIAL_HINT_RE = /\[([^[\]\n]{0,40})(?:\](?:\((?:\/[^\n)]{0,80})?)?)?$/

/** Replace every pronunciation hint with its plain word. `partialTail`
 *  also hides a half-streamed hint at the end (keeping the word);
 *  `partialHead` hides the tail end of a hint that a bubble starts inside
 *  (`team](/tˈim/) …` → `team …`), for text split across bubbles. */
export function stripPronunciationHints(
  text: string,
  opts: { partialTail?: boolean; partialHead?: boolean } = {},
): string {
  const head = opts.partialHead ? stripOrphanHead(text) : text
  if (!head.includes('[')) return head
  // Broken hints first: a complete hint's phonemes end at `/`, so it never
  // matches, and a broken one can't borrow the next hint's `/)`.
  const t = head.replace(BROKEN_HINT_RE, '$1').replace(HINT_RE, '$1')
  return opts.partialTail ? t.replace(PARTIAL_HINT_RE, (_m, word: string) => word) : t
}

/** Drop the remainder of a hint the text starts inside: `word](/ph…/)`
 *  keeps `word`; `(/ph…/)`, `/ph…/)` and a bare IPA `ph…/)` go entirely. */
function stripOrphanHead(text: string): string {
  const end = text.indexOf('/)')
  if (end < 0) return text
  const head = text.slice(0, end)
  if (head.includes('[')) return text
  const rest = text.slice(end + 2)
  const labelled = /^([^\]\n]*?)\]\(\/[^/()\n]*$/.exec(head)
  if (labelled) return labelled[1] + rest
  if (/^\(?\/[^/()\n]*$/.test(head)) return rest
  if (/^[^\s/()[\]]+$/.test(head) && /[\u0080-\uffff]/.test(head)) return rest
  return text
}

/** Same-length copy of `text` with every hint — complete, or still open at
 *  the end — blanked out, so sentence/clause scanning never cuts one. */
function maskHints(text: string): string {
  return blankHints(text).replace(PARTIAL_HINT_RE, (m) => 'x'.repeat(m.length))
}

function blankHints(text: string): string {
  return text.replace(HINT_RE, (m) => 'x'.repeat(m.length))
}

/** Does `text` end in a pronunciation hint that is still being streamed? */
export function hasOpenHint(text: string): boolean {
  return PARTIAL_HINT_RE.test(blankHints(text))
}

/** Replace a half-streamed hint at the end of `text` with its bare word,
 *  keeping complete hints — for speaking a tail that will not be finished. */
export function closeOpenHint(text: string): string {
  const m = PARTIAL_HINT_RE.exec(blankHints(text))
  return m ? text.slice(0, m.index) + m[1] : text
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
  // Markdown links / images: keep the label. Pronunciation hints stay —
  // the server's TTS reads them.
  t = t.replace(/!?\[([^\]\n]*)\]\(([^)[\n]*)\)/g, (m: string, label: string) =>
    HINT_RE_ONE.test(m) ? m : label,
  )
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
  // Scan a hint-masked copy (same length, masked once) so a hint is never
  // split and a fully hinted buffer isn't re-masked per sentence.
  const masked = maskHints(buffer)
  const re = /[^.!?:;…\n]*(?:[.!?:;…]+["')\]]*(?=\s)|\n)\s*/y
  let pos = 0
  for (;;) {
    re.lastIndex = pos
    const m = re.exec(masked)
    if (!m) break
    const piece = buffer.slice(pos, pos + m[0].length).trim()
    pos += m[0].length
    if (piece) sentences.push(piece)
  }
  return { sentences, rest: buffer.slice(pos) }
}

/** Heard (hint-stripped) length past which a still-open sentence is cut at
 *  a clause boundary, so a long sentence starts playing before it is
 *  finished. */
const CLAUSE_SPLIT_AT = 90

/**
 * Chunk streamed assistant text for live speech: complete sentences, plus —
 * when the unfinished tail grows long — its leading clauses (cut after a
 * comma / dash). The returned `rest` is still being generated. Lengths
 * count the words heard, not hint markup.
 */
export function takeSpeakable(buffer: string): { chunks: string[]; rest: string } {
  const { sentences, rest: tail } = takeSentences(buffer)
  const chunks = [...sentences]
  let rest = tail
  const heard = (s: string) => stripPronunciationHints(s, { partialTail: true }).length
  while (rest.length > CLAUSE_SPLIT_AT && heard(rest) > CLAUSE_SPLIT_AT) {
    const head = maskHints(rest).slice(0, rest.length - 1)
    const cut = Math.max(head.lastIndexOf(', '), head.lastIndexOf(' — '), head.lastIndexOf(' - '))
    if (cut < 0 || heard(rest.slice(0, cut)) < 30) break
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
  // What was spoken may carry pronunciation hints; the mic hears the words.
  const pool = new Set(spoken.flatMap((s) => speechWords(stripPronunciationHints(s))))
  if (pool.size === 0) return 0
  return words.filter((w) => pool.has(w)).length / words.length
}

/** Words a sentence rarely ends on: conjunctions, prepositions, articles,
 *  determiners, auxiliaries and fillers — the speaker is mid-thought. */
const UNFINISHED_TAIL = new Set(
  (
    'and but or nor so because since though although unless until while ' +
    'the a an this these those my your his her its our their some any every ' +
    'to of for with in on at by from into onto about like than as ' +
    'that which who whom whose where if when whether then ' +
    'um uh er erm hmm ' +
    'is are was were be been being am will would can could should shall ' +
    'may might must do does did have has had ' +
    // Verbs and adverbs that wait for what comes next ("I also want …").
    'want wanna need gonna also just'
  ).split(' '),
)
const UNFINISHED_PHRASES = ['you know', 'i mean', 'kind of', 'sort of']

/** `text` ends in sentence-final punctuation (`.`, `!`, `?`). */
export function endsSentence(text: string): boolean {
  return /[.!?]["')\]]*\s*$/.test(text) && !/(\.\.\.|…)\s*$/.test(text)
}

/**
 * The user's transcript so far looks cut off mid-sentence: it ends on a
 * connective / article / auxiliary / filler, on a comma, dash or ellipsis,
 * or is a very short fragment with no sentence-final punctuation.
 */
/** Short replies that are complete on their own — answers to the
 *  assistant's yes/no questions must not wait out the mid-sentence pause. */
const COMPLETE_SHORT_REPLIES = new Set([
  'yes',
  'yeah',
  'yep',
  'yup',
  'sure',
  'no',
  'nope',
  'nah',
  'ok',
  'okay',
  'stop',
  'wait',
  'thanks',
  'thank you',
  'go ahead',
  'do it',
  'sounds good',
  'got it',
  'never mind',
  'cancel',
  'continue',
  'next',
  'correct',
  'right',
  'exactly',
  'perfect',
  'great',
  'cool',
  'fine',
  'done',
  'hello',
  'hi',
  'hey',
  'yes please',
  'no thanks',
  'not now',
  'later',
  'stop that',
  'shut up',
])

export function looksUnfinished(text: string): boolean {
  const t = text.trim()
  if (!t) return false
  if (/(,|-|–|—|\.\.\.|…)\s*$/.test(t)) return true
  if (endsSentence(t)) return false
  const words = speechWords(t)
  if (COMPLETE_SHORT_REPLIES.has(words.join(' '))) return false
  if (words.length <= 2) return true
  const tail = words.slice(-2).join(' ')
  return UNFINISHED_TAIL.has(words[words.length - 1]) || UNFINISHED_PHRASES.includes(tail)
}

/** Silence after a finished-looking utterance before it is sent. */
export const END_OF_TURN_MS = 1300
/** Extra wait when the recognizer gave no sentence-final punctuation (it
 *  often doesn't), without anything else suggesting an unfinished thought. */
export const UNPUNCTUATED_EXTRA_MS = 500

/** How long to wait in silence before sending `text` as the user's turn. */
export function endOfTurnDelay(text: string, maxPauseMs: number): number {
  if (looksUnfinished(text)) return Math.max(END_OF_TURN_MS, maxPauseMs)
  return endsSentence(text) ? END_OF_TURN_MS : END_OF_TURN_MS + UNPUNCTUATED_EXTRA_MS
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
          items.push({
            kind: 'relay',
            key: ev.id,
            text: stripPronunciationHints(stripRelayPrefix(text)),
            ts: ev.ts,
          })
        } else {
          items.push({ kind: 'user', key: ev.id, text: stripInterruptMarker(text), ts: ev.ts })
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
  // Hints can straddle streamed chunks: strip once the turn is joined.
  for (const it of items) {
    if (it.kind === 'assistant') {
      it.text = stripPronunciationHints(it.text, { partialTail: it.streaming })
    }
  }
  return items.filter((it) => it.kind !== 'assistant' || it.text.trim() !== '')
}
