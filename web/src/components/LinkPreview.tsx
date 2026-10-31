import { useCallback, useEffect, useId, useMemo, useRef, useState, type CSSProperties } from 'react'
import Modal from './Modal'
import AudioPlayer from './AudioPlayer'
import InternalLinkPreview from './LinkPreviewInternal'
import { extractLinks } from '../lib/links'
import { useMediaElementPlayback, useMediaPlayback } from '../media/playback'
import {
  PARENT_HOST_TOKEN,
  TRUSTED_EMBED_PROVIDERS,
  useLinkPreviewStore,
  type LinkPreviewData,
} from '../store/linkPreviews'
import ModelViewer from './ModelViewer'
import { modelFormatFromMime, modelFormatFromName, type ModelFormat } from './model/formats'
import { AssetActions, DetailsLine, EventCard, PostCard } from './LinkPreviewDetails'
import { isCompactDetails } from '../lib/linkPreviewDetails'
import '../styles/link-preview.css'

/** A streamed message is only scanned once it has stopped growing, so a
 *  half-written URL is never fetched. */
const SETTLE_MS = 600
/** Height of an auto-height embed (X, Bluesky) until it reports its own. */
const DEFAULT_EMBED_HEIGHT = 420
const MAX_EMBED_HEIGHT = 1600

function useSettled(text: string): string {
  const [settled, setSettled] = useState('')
  useEffect(() => {
    const t = setTimeout(() => setSettled(text), SETTLE_MS)
    return () => clearTimeout(t)
  }, [text])
  return settled
}

function domainOf(url: string): string {
  try {
    return new URL(url).hostname.replace(/^www\./, '')
  } catch {
    return url
  }
}

function sameOrigin(url: string): boolean {
  try {
    return new URL(url, window.location.href).origin === window.location.origin
  } catch {
    return false
  }
}

/** `<img>` that removes itself when it fails to load. */
function SafeImg({ src, className, testId }: { src: string; className: string; testId?: string }) {
  const [failed, setFailed] = useState(false)
  if (failed) return null
  return (
    <img
      className={className}
      src={src}
      alt=""
      loading="lazy"
      onError={() => setFailed(true)}
      data-testid={testId}
    />
  )
}

function SiteLine({ p, label }: { p: LinkPreviewData; label?: string }) {
  const domain = domainOf(p.final_url || p.url)
  return (
    <span className="link-card-site">
      {p.favicon && <SafeImg src={p.favicon} className="link-card-favicon" />}
      <span className="link-card-sitename">{label ?? p.site_name ?? domain}</span>
      {(label ?? p.site_name) && <span className="link-card-domain">{domain}</span>}
    </span>
  )
}

// ── Card ──────────────────────────────────────────────────────────────

function availabilityLabel(raw: string): string {
  const tail = raw.replace(/^https?:\/\/schema\.org\//i, '')
  return tail.replace(/([a-z])([A-Z])/g, '$1 $2').replace(/^\w/, (c) => c.toUpperCase())
}

function priceLabel(price: string, currency?: string): string {
  const n = Number(price)
  if (currency && Number.isFinite(n)) {
    try {
      return new Intl.NumberFormat(undefined, { style: 'currency', currency }).format(n)
    } catch {
      // Unknown currency code — fall through.
    }
  }
  return currency ? `${price} ${currency}` : price
}

function ProductLine({ product }: { product: NonNullable<LinkPreviewData['json_ld_product']> }) {
  const inStock = product.availability ? /instock|in stock/i.test(product.availability) : null
  return (
    <span className="link-card-meta" data-testid="link-card-product">
      {product.price && (
        <span className="link-card-price">{priceLabel(product.price, product.currency)}</span>
      )}
      {product.availability && (
        <span
          className={`link-badge${inStock === false ? ' link-badge-danger' : ' link-badge-ok'}`}
        >
          {availabilityLabel(product.availability)}
        </span>
      )}
      {typeof product.rating === 'number' && (
        <span title="Rating">
          {'★'} {product.rating.toFixed(1)}
          {typeof product.review_count === 'number' ? ` (${product.review_count})` : ''}
        </span>
      )}
    </span>
  )
}

function CardPreview({ p, note }: { p: LinkPreviewData; note?: string }) {
  const href = p.final_url || p.url
  const compact = p.provider === 'github' || isCompactDetails(p.details)
  const accent: CSSProperties | undefined = p.theme_color
    ? { borderInlineStartColor: p.theme_color }
    : undefined
  const card = (
    <a
      className={`link-card${compact ? ' link-card-compact' : ''}${p.theme_color ? ' link-card-accented' : ''}`}
      href={href}
      target="_blank"
      rel="noreferrer noopener"
      data-testid="link-card"
      data-kind={p.kind}
      data-type={typeof p.details?.type === 'string' ? p.details.type : undefined}
      style={accent}
    >
      {p.image && (
        <SafeImg
          src={p.image}
          className={compact ? 'link-card-thumb' : 'link-card-image'}
          testId="link-card-image"
        />
      )}
      <span className="link-card-body">
        <SiteLine p={p} />
        {p.title && (
          <span className="link-card-title" data-testid="link-card-title">
            {p.title}
          </span>
        )}
        {p.description && <span className="link-card-desc">{p.description}</span>}
        {p.json_ld_product && <ProductLine product={p.json_ld_product} />}
        {p.details && <DetailsLine d={p.details} />}
        {note && <span className="link-card-note">{note}</span>}
      </span>
    </a>
  )
  if (p.details?.type !== 'asset' || !p.details.download_src) return card
  return (
    <div className="link-card-stack">
      {card}
      <AssetActions d={p.details} />
      <AssetModelToggle d={p.details} />
    </div>
  )
}

/** "View in 3D" under an asset card whose page links a model file the
 *  viewer can read; mounts the same viewer as a direct model link. */
function AssetModelToggle({ d }: { d: NonNullable<LinkPreviewData['details']> }) {
  const [shown, setShown] = useState(false)
  const src = typeof d.download_src === 'string' ? d.download_src : null
  const raw = typeof d.download_url === 'string' ? d.download_url : null
  if (!src || !raw) return null
  let format: ModelFormat | null
  try {
    format = modelFormatFromName(new URL(raw, location.href).pathname)
  } catch {
    format = null
  }
  if (!format) return null
  const name = raw.split(/[?#]/)[0].split('/').pop() || 'model'
  if (!shown)
    return (
      <div className="link-card-actions">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => setShown(true)}
          data-testid="link-asset-view-3d"
        >
          View in 3D
        </button>
      </div>
    )
  return <ModelLinkPreview src={src} href={raw} name={name} format={format} />
}

// ── Embed ─────────────────────────────────────────────────────────────

function PlayIcon() {
  return (
    <svg viewBox="0 0 24 24" width="28" height="28" aria-hidden="true">
      <path d="M8 5v14l11-7z" fill="currentColor" />
    </svg>
  )
}

function escapeAttr(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;')
}

/** The wrapper document of a `srcdoc` embed: the provider's frame inside an
 *  opaque-origin sandbox, plus our relay script (same-origin, so the
 *  inherited CSP allows it) that reports the widget's height. */
function srcdocFor(src: string, height: number): string {
  return (
    '<!doctype html><html><head><meta charset="utf-8">' +
    `<script src="${escapeAttr(window.location.origin)}/embed-relay.js"></script>` +
    `</head><body><iframe src="${escapeAttr(src)}" width="100%" height="${height}" ` +
    'frameborder="0" scrolling="no" allowfullscreen></iframe></body></html>'
  )
}

function aspectStyle(embed: NonNullable<LinkPreviewData['embed']>): CSSProperties | null {
  if (embed.aspect) return { aspectRatio: embed.aspect.replace('/', ' / ') }
  if (embed.width && embed.height) return { aspectRatio: `${embed.width} / ${embed.height}` }
  return null
}

/** The height an embed frame reports for itself, by provider protocol:
 *  Bluesky / Telegram `{height}`, X `{"twttr.embed": {method:
 *  "twttr.private.resize", params: [{height}]}}`, Instagram `{type: "MEASURE",
 *  details: {height}}`, Reddit `{type: "resize.embed", data}`. */
function reportedHeight(m: Record<string, unknown>): number | null {
  const num = (v: unknown) => (typeof v === 'number' && v > 0 ? v : null)
  const tw = m['twttr.embed'] as { method?: unknown; params?: unknown } | undefined
  if (tw && tw.method === 'twttr.private.resize' && Array.isArray(tw.params)) {
    return num((tw.params[0] as Record<string, unknown> | undefined)?.height)
  }
  if (m.type === 'MEASURE') return num((m.details as Record<string, unknown> | undefined)?.height)
  if (m.type === 'resize.embed') return num(m.data)
  return num(m.height)
}
function EmbedFrame({
  p,
  autoplay,
  onRevert,
}: {
  p: LinkPreviewData
  autoplay: boolean
  onRevert: () => void
}) {
  const embed = p.embed!
  const ref = useRef<HTMLIFrameElement>(null)
  const youtube = p.provider === 'youtube'
  const [height, setHeight] = useState<number | null>(null)
  const src = useMemo(() => {
    const raw = embed.iframe_src
      .split(PARENT_HOST_TOKEN)
      .join(encodeURIComponent(window.location.hostname))
    try {
      const u = new URL(raw)
      if (youtube) {
        u.searchParams.set('enablejsapi', '1')
        u.searchParams.set('origin', window.location.origin)
        if (autoplay) u.searchParams.set('autoplay', '1')
      }
      return u.href
    } catch {
      return raw
    }
  }, [embed.iframe_src, youtube, autoplay])
  const frameOrigin = useMemo(() => {
    try {
      return new URL(src).origin
    } catch {
      return '*'
    }
  }, [src])

  // YouTube pauses over its JS API; any other embed has no pause, so
  // pausing it unloads the frame back to its facade.
  const pause = useCallback(() => {
    const w = ref.current?.contentWindow
    if (youtube && w) {
      w.postMessage(JSON.stringify({ event: 'command', func: 'pauseVideo', args: [] }), frameOrigin)
    } else {
      onRevert()
    }
  }, [youtube, frameOrigin, onRevert])
  const media = useMediaPlayback(pause)

  useEffect(() => {
    const onMessage = (e: MessageEvent) => {
      if (!ref.current || e.source !== ref.current.contentWindow) return
      let d: unknown = e.data
      if (typeof d === 'string') {
        try {
          d = JSON.parse(d)
        } catch {
          return
        }
      }
      if (!d || typeof d !== 'object') return
      const m = d as Record<string, unknown>
      if (youtube) {
        const info = m.info as Record<string, unknown> | number | undefined
        const state =
          m.event === 'onStateChange'
            ? info
            : m.event === 'infoDelivery' && info && typeof info === 'object'
              ? info.playerState
              : undefined
        if (state === 1) media.playing()
        else if (state === 0 || state === 2) media.stopped()
        return
      }
      const h = reportedHeight(m)
      if (h) setHeight(Math.min(MAX_EMBED_HEIGHT, Math.ceil(h)))
    }
    window.addEventListener('message', onMessage)
    return () => window.removeEventListener('message', onMessage)
  }, [youtube, media])

  const aspect = aspectStyle(embed)
  const frameHeight = height ?? embed.height ?? DEFAULT_EMBED_HEIGHT
  const style: CSSProperties = aspect ?? { height: frameHeight }
  const title = p.title ?? `${p.site_name ?? 'Embedded'} content`
  // Built once per source: a new srcdoc reloads the frame, and the relay
  // sizes the inner frame itself as the widget reports its height.
  const srcdoc = useMemo(
    () => srcdocFor(src, embed.height ?? DEFAULT_EMBED_HEIGHT),
    [src, embed.height],
  )
  return (
    <div className="link-embed-live" style={embed.width ? { maxWidth: embed.width } : undefined}>
      {embed.sandbox_mode === 'srcdoc' ? (
        <iframe
          ref={ref}
          className="link-embed-frame"
          title={title}
          srcDoc={srcdoc}
          sandbox="allow-scripts allow-popups"
          referrerPolicy="strict-origin-when-cross-origin"
          loading="lazy"
          style={style}
          data-testid="link-embed-frame"
          data-sandbox-mode="srcdoc"
        />
      ) : (
        <iframe
          ref={ref}
          className="link-embed-frame"
          title={title}
          src={src}
          sandbox="allow-scripts allow-same-origin allow-popups allow-presentation"
          allow="autoplay; encrypted-media; picture-in-picture; fullscreen"
          referrerPolicy="strict-origin-when-cross-origin"
          loading="lazy"
          allowFullScreen
          style={style}
          data-testid="link-embed-frame"
          data-sandbox-mode="origin"
          onLoad={() => {
            // Subscribe to the player's state events (play / pause).
            if (youtube) {
              ref.current?.contentWindow?.postMessage(
                JSON.stringify({ event: 'listening', id: 1, channel: 'widget' }),
                frameOrigin,
              )
            }
          }}
        />
      )}
      <button
        type="button"
        className="link-embed-close"
        onClick={onRevert}
        aria-label="Unload embed"
        title="Unload embed"
        data-testid="link-embed-close"
      >
        {'×'}
      </button>
    </div>
  )
}

function EmbedPreview({ p }: { p: LinkPreviewData }) {
  const id = useId()
  const embed = p.embed!
  const autoEmbeds = useLinkPreviewStore((s) => s.autoEmbeds)
  const live = useLinkPreviewStore((s) => s.liveEmbeds.includes(id))
  const activate = useLinkPreviewStore((s) => s.activateEmbed)
  const deactivate = useLinkPreviewStore((s) => s.deactivateEmbed)
  const [clicked, setClicked] = useState(false)
  const auto =
    autoEmbeds === 'all' ||
    (autoEmbeds === 'trusted' && TRUSTED_EMBED_PROVIDERS.has(p.provider ?? ''))
  useEffect(() => {
    if (auto) activate(id)
  }, [auto, id, activate])
  useEffect(() => () => deactivate(id), [id, deactivate])
  const revert = useCallback(() => {
    setClicked(false)
    deactivate(id)
  }, [id, deactivate])

  const site = p.site_name ?? domainOf(p.url)
  const aspect = aspectStyle(embed)
  return (
    <div
      className="link-embed"
      data-testid="link-embed"
      data-provider={p.provider}
      data-live={live ? 'true' : 'false'}
    >
      {live ? (
        <EmbedFrame p={p} autoplay={clicked} onRevert={revert} />
      ) : (
        <button
          type="button"
          className={`link-embed-facade${aspect ? '' : ' link-embed-facade-auto'}`}
          style={aspect ?? undefined}
          onClick={() => {
            setClicked(true)
            activate(id)
          }}
          aria-label={`Load ${site}${p.title ? `: ${p.title}` : ''}`}
          data-testid="link-embed-facade"
        >
          {p.image && <SafeImg src={p.image} className="link-embed-thumb" />}
          <span className="link-embed-play" aria-hidden="true">
            <PlayIcon />
          </span>
          <span className="link-embed-caption">
            <SiteLine p={p} label={site} />
            {p.title && (
              <span className="link-card-title" data-testid="link-embed-title">
                {p.title}
              </span>
            )}
          </span>
        </button>
      )}
      <a className="link-embed-open" href={p.url} target="_blank" rel="noreferrer noopener">
        Open on {site} {'↗'}
      </a>
    </div>
  )
}

// ── Media ─────────────────────────────────────────────────────────────

function InlineVideo({ src }: { src: string }) {
  const ref = useRef<HTMLVideoElement | null>(null)
  useMediaElementPlayback(ref, src)
  return (
    <video
      ref={ref}
      className="shared-file-video"
      src={`${src}#t=0.001`}
      controls
      preload="metadata"
      playsInline
      data-testid="link-media-video"
    />
  )
}

/** A linked 3D file, viewed inline from the server's signed proxy URL
 *  (`media.src`); Fullscreen opens a larger viewer. */
function ModelLinkPreview({
  src,
  href,
  name,
  format,
}: {
  src: string
  href: string
  name: string
  format: ModelFormat
}) {
  const [open, setOpen] = useState(false)
  const source = useMemo(
    () => ({ key: `link:${href}`, name, url: () => Promise.resolve(src) }),
    [href, name, src],
  )
  return (
    <div className="shared-file-card link-media" data-kind="model" data-testid="link-media">
      <ModelViewer source={source} format={format} onExpand={() => setOpen(true)} />
      <a className="link-embed-open" href={href} target="_blank" rel="noreferrer noopener">
        {name} {'↗'}
      </a>
      {open && (
        <Modal
          onClose={() => setOpen(false)}
          className="shared-file-modal"
          maxWidth="min(1200px, 96vw)"
        >
          <div className="shared-file-modal-header">
            <h3 className="shared-file-modal-title" title={href}>
              {name}
            </h3>
            <a className="btn-secondary" href={href} target="_blank" rel="noreferrer noopener">
              Open
            </a>
            <button type="button" className="btn-secondary" onClick={() => setOpen(false)}>
              Close
            </button>
          </div>
          <div className="shared-file-modal-body">
            <ModelViewer source={source} format={format} variant="modal" />
          </div>
        </Modal>
      )}
    </div>
  )
}
function MediaPreview({ p }: { p: LinkPreviewData }) {
  const [open, setOpen] = useState(false)
  const type = p.media?.type
  const href = p.final_url || p.url
  const name = p.title ?? domainOf(href)
  if (type === 'image' && p.image) {
    return (
      <>
        <button
          type="button"
          className="link-media-image"
          onClick={() => setOpen(true)}
          aria-label={`Preview ${name}`}
          data-testid="link-media-image"
        >
          <SafeImg src={p.image} className="shared-file-thumb" />
        </button>
        {open && (
          <Modal
            onClose={() => setOpen(false)}
            className="shared-file-modal"
            maxWidth="min(960px, 96vw)"
          >
            <div className="shared-file-modal-header">
              <h3 className="shared-file-modal-title" title={href}>
                {name}
              </h3>
              <a className="btn-secondary" href={href} target="_blank" rel="noreferrer noopener">
                Open
              </a>
              <button type="button" className="btn-secondary" onClick={() => setOpen(false)}>
                Close
              </button>
            </div>
            <div className="shared-file-modal-body">
              <img className="shared-file-preview-img" src={p.image} alt={name} />
            </div>
          </Modal>
        )}
      </>
    )
  }
  // The page CSP only lets <video>/<audio> load this origin's media: a
  // remote file plays through the server's signed stream (`media.src`).
  const playable =
    type === 'video' || type === 'audio' ? (p.media?.src ?? (sameOrigin(href) ? href : null)) : null
  if (playable) {
    return (
      <div className="shared-file-card link-media" data-kind={type} data-testid="link-media">
        {type === 'video' ? (
          <InlineVideo src={playable} />
        ) : (
          <AudioPlayer src={playable} label={name} waveformKey={null} />
        )}
      </div>
    )
  }
  if (type === 'model') {
    const src = (p.media as { src?: string } | undefined)?.src
    const format =
      modelFormatFromName(new URL(href, location.href).pathname) ??
      modelFormatFromMime(p.media?.mime ?? '')
    if (src && format) return <ModelLinkPreview src={src} href={href} name={name} format={format} />
  }
  const note =
    type === 'model'
      ? '3D model — open the link to view it.'
      : type === 'video'
        ? 'Video — open the link to play it.'
        : type === 'audio'
          ? 'Audio — open the link to play it.'
          : undefined
  return <CardPreview p={{ ...p, image: type === 'image' ? undefined : p.image }} note={note} />
}

// ── Public API ────────────────────────────────────────────────────────

/** One link's preview, by kind; nothing while loading or when the server
 *  had nothing to show (the plain link in the message stays). */
export function LinkPreview({ url }: { url: string }) {
  const entry = useLinkPreviewStore((s) => s.entries[url])
  const load = useLinkPreviewStore((s) => s.load)
  useEffect(() => load(url), [url, load])
  if (entry?.status !== 'ok') return null
  const p = entry.preview
  switch (p.kind) {
    case 'embed':
      return p.embed ? <EmbedPreview p={p} /> : <CardPreview p={p} />
    case 'media':
      return <MediaPreview p={p} />
    case 'internal':
      return <InternalLinkPreview preview={p} />
    default:
      if (p.details?.type === 'event') return <EventCard p={p} />
      if (p.details?.type === 'post') return <PostCard p={p} />
      return p.title || p.description || p.image ? <CardPreview p={p} /> : null
  }
}

/**
 * Previews for the distinct http(s) links in a chat message (at most three,
 * none from code): site cards, click-to-load embeds, inline media, and live
 * cards for links to this instance. Renders under the message body.
 */
export default function LinkPreviews({ text, className }: { text: string; className?: string }) {
  const settled = useSettled(text)
  const urls = useMemo(() => extractLinks(settled), [settled])
  if (urls.length === 0) return null
  return (
    <div className={`link-previews${className ? ` ${className}` : ''}`} data-testid="link-previews">
      {urls.map((u) => (
        <LinkPreview key={u} url={u} />
      ))}
    </div>
  )
}
