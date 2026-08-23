// Transcript export: pull a session's complete event history and download it
// as Markdown or raw JSON. The UI's in-memory event list may only hold the
// most recent page, so the export always re-fetches from
// `GET /api/sessions/:id/events`, paginating backward until a short page
// signals the start of history.

import type { Event } from '../types/api'
import { authedFetch } from '../store/auth'
import { buildDisplayItems, formatTime } from '../components/chat/events'

const PAGE = 500

/** Fetch the complete, seq-ascending event history for a session. */
export async function fetchAllEvents(sessionId: string): Promise<Event[]> {
  const all: Event[] = []
  let beforeSeq: number | null = null
  for (;;) {
    const qs = beforeSeq === null ? `?limit=${PAGE}` : `?limit=${PAGE}&before_seq=${beforeSeq}`
    const res = await authedFetch(`/api/sessions/${sessionId}/events${qs}`)
    if (!res.ok) throw new Error(`events fetch failed: ${res.status}`)
    const page = (await res.json()) as Event[]
    if (page.length === 0) break
    all.unshift(...page)
    beforeSeq = page[0].seq
    if (page.length < PAGE) break
  }
  return all
}

/** Render the transcript as readable Markdown via the same DisplayItem
 *  derivation the chat renders from, so the export matches what the user
 *  saw (tools as one-liners, diffs as ```diff fences, thinking as quotes). */
export function transcriptMarkdown(events: Event[], sessionName: string): string {
  const items = buildDisplayItems(events)
  const lines: string[] = [`# ${sessionName}`, '']
  for (const item of items) {
    switch (item.type) {
      case 'user':
        lines.push(`## You — ${formatTime(item.ts)}`, '', item.text, '')
        break
      case 'assistant':
        lines.push(`## Assistant — ${formatTime(item.ts)}`, '', item.text, '')
        break
      case 'thinking':
        lines.push('> _Thinking:_', ...item.text.split('\n').map((l) => `> ${l}`), '')
        break
      case 'tool': {
        const bare = item.toolName.replace(/^mcp__.+?__/, '')
        lines.push(`- \`${bare}\`${item.error ? ` — error: ${item.error}` : ''}`)
        if (item.diff) {
          lines.push('', '```diff', item.diff.diff, '```', '')
        }
        break
      }
      case 'file-diff':
        lines.push('', '```diff', item.diff.diff, '```', '')
        break
      case 'step':
        lines.push('---', '', `### ${item.label}`, '')
        break
      case 'question':
      case 'question-resolved':
        lines.push(`- ❓ ${item.questions.map((q) => q.question).join(' | ')}`)
        break
      case 'agent-crashed':
        lines.push(`- ⚠️ agent crashed: ${item.reason}`)
        break
      case 'handover':
        lines.push(
          `- ↔️ ${
            item.compaction
              ? 'context compacted'
              : item.recovery
                ? `recovery transcript sent to ${item.to}`
                : `handover to ${item.to}`
          }`,
        )
        break
      default:
        break
    }
  }
  return lines.join('\n')
}

/** Fetch the full history and trigger a browser download. */
export async function downloadTranscript(
  sessionId: string,
  sessionName: string,
  format: 'markdown' | 'json',
): Promise<void> {
  const events = await fetchAllEvents(sessionId)
  const stamp = new Date().toISOString().slice(0, 10)
  const safeName = (sessionName || sessionId).replace(/[^\w.-]+/g, '-').slice(0, 60)
  const [content, ext, mime] =
    format === 'json'
      ? [JSON.stringify(events, null, 2), 'json', 'application/json']
      : [transcriptMarkdown(events, sessionName), 'md', 'text/markdown']
  const blob = new Blob([content], { type: mime })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = `peckboard-${safeName}-${stamp}.${ext}`
  a.click()
  URL.revokeObjectURL(url)
}
