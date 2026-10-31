import { useEffect, useMemo, useState } from 'react'
import { useVoiceStore } from '../../store/voice'
import { useLinkPreviewStore } from '../../store/linkPreviews'
import { foldVoiceTranscript } from '../../voice/text'
import { extractLinks } from '../../lib/links'
import { LinkPreview } from '../LinkPreview'

/** How many of the newest assistant / relay lines are searched for links. */
const RECENT_LINES = 6

/**
 * The newest embeddable link (video, post) the Assistant or a relayed update
 * mentioned, as a card beside the orb. Dismissing hides that link until a
 * newer embed arrives.
 */
export default function AssistantEmbedCard() {
  const events = useVoiceStore((s) => s.events)
  const entries = useLinkPreviewStore((s) => s.entries)
  const load = useLinkPreviewStore((s) => s.load)
  const [dismissed, setDismissed] = useState<string | null>(null)

  // Newest first.
  const candidates = useMemo(() => {
    const lines = foldVoiceTranscript(events).filter(
      (it) => it.kind === 'relay' || (it.kind === 'assistant' && !it.streaming),
    )
    return lines
      .slice(-RECENT_LINES)
      .reverse()
      .flatMap((it) => extractLinks(it.text).reverse())
  }, [events])

  useEffect(() => {
    for (const url of candidates) load(url)
  }, [candidates, load])

  const url = candidates.find((u) => {
    const e = entries[u]
    return e?.status === 'ok' && e.preview.kind === 'embed'
  })
  if (!url || url === dismissed) return null
  return (
    <aside
      className="assistant-embed-card"
      data-testid="assistant-embed-card"
      aria-label="Shared link"
    >
      <button
        type="button"
        className="assistant-embed-dismiss"
        onClick={() => setDismissed(url)}
        aria-label="Dismiss"
        title="Dismiss"
        data-testid="assistant-embed-dismiss"
      >
        {'×'}
      </button>
      <LinkPreview url={url} />
    </aside>
  )
}
