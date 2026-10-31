import { create } from 'zustand'
import { authedFetch } from './auth'

/** `GET /api/link-preview?url=` (`service::link_preview::Preview`). */
export interface LinkPreviewData {
  url: string
  final_url: string
  kind: 'embed' | 'card' | 'media' | 'internal'
  provider?: string
  title?: string
  description?: string
  site_name?: string
  /** Signed same-origin proxy URL — a plain `<img src>` works. */
  image?: string
  favicon?: string
  /** Server-vetted CSS colour. */
  theme_color?: string
  embed?: {
    iframe_src: string
    sandbox_mode: 'origin' | 'srcdoc'
    width?: number
    height?: number
    aspect?: string
  }
  media?: { type: 'image' | 'video' | 'audio' | 'model'; mime: string }
  json_ld_product?: {
    name?: string
    price?: string
    currency?: string
    availability?: string
    rating?: number
    review_count?: number
  }
  internal?: {
    entity: 'session' | 'project' | 'card' | 'report' | 'view' | 'page'
    id?: string
    project_id?: string
  }
  details?: Record<string, unknown>
}

export type PreviewEntry =
  | { status: 'loading' }
  | { status: 'ok'; preview: LinkPreviewData }
  | { status: 'error'; code: number; at: number }

/** "Auto-load embeds": which embeds mount their iframe without a click. */
export type AutoEmbeds = 'off' | 'trusted' | 'all'

/** Embed providers whose frames are origin-isolated and don't need the
 *  provider's own script — what "Trusted providers" auto-loads. */
export const TRUSTED_EMBED_PROVIDERS = new Set(['youtube', 'bluesky'])

/** Most embed iframes alive at once; mounting another reverts the oldest
 *  to its click-to-load facade. */
export const MAX_LIVE_EMBEDS = 4

export const AUTO_EMBEDS_KEY = 'peckboard_link_embeds_autoload'

/** A failed preview stays failed this long before it is asked for again
 *  (a rate-limit refusal much sooner). */
const ERROR_TTL_MS = 10 * 60 * 1000
const RATE_LIMIT_TTL_MS = 60 * 1000

function readAutoEmbeds(): AutoEmbeds {
  try {
    const v = localStorage.getItem(AUTO_EMBEDS_KEY)
    return v === 'trusted' || v === 'all' ? v : 'off'
  } catch {
    return 'off'
  }
}

interface LinkPreviewState {
  entries: Record<string, PreviewEntry>
  /** Fetch `url`'s preview unless it is cached, in flight, or failed
   *  recently. */
  load: (url: string) => void
  autoEmbeds: AutoEmbeds
  setAutoEmbeds: (mode: AutoEmbeds) => void
  /** Ids of the embeds whose iframe is mounted, oldest first. */
  liveEmbeds: string[]
  activateEmbed: (id: string) => void
  deactivateEmbed: (id: string) => void
}

const inFlight = new Set<string>()

export const useLinkPreviewStore = create<LinkPreviewState>((set, get) => ({
  entries: {},
  load: (url) => {
    const cur = get().entries[url]
    if (cur?.status === 'ok' || cur?.status === 'loading' || inFlight.has(url)) return
    if (cur?.status === 'error') {
      const ttl = cur.code === 429 ? RATE_LIMIT_TTL_MS : ERROR_TTL_MS
      if (Date.now() - cur.at < ttl) return
    }
    inFlight.add(url)
    const put = (entry: PreviewEntry) => set((s) => ({ entries: { ...s.entries, [url]: entry } }))
    put({ status: 'loading' })
    authedFetch(`/api/link-preview?url=${encodeURIComponent(url)}`)
      .then(async (res) => {
        if (!res.ok) {
          put({ status: 'error', code: res.status, at: Date.now() })
          return
        }
        put({ status: 'ok', preview: (await res.json()) as LinkPreviewData })
      })
      .catch(() => put({ status: 'error', code: 0, at: Date.now() }))
      .finally(() => inFlight.delete(url))
  },
  autoEmbeds: readAutoEmbeds(),
  setAutoEmbeds: (mode) => {
    try {
      if (mode === 'off') localStorage.removeItem(AUTO_EMBEDS_KEY)
      else localStorage.setItem(AUTO_EMBEDS_KEY, mode)
    } catch {
      // Storage unavailable — keep the in-memory value for this tab.
    }
    set({ autoEmbeds: mode })
  },
  liveEmbeds: [],
  activateEmbed: (id) =>
    set((s) => {
      const rest = s.liveEmbeds.filter((x) => x !== id)
      return { liveEmbeds: [...rest, id].slice(-MAX_LIVE_EMBEDS) }
    }),
  deactivateEmbed: (id) =>
    set((s) =>
      s.liveEmbeds.includes(id) ? { liveEmbeds: s.liveEmbeds.filter((x) => x !== id) } : s,
    ),
}))
