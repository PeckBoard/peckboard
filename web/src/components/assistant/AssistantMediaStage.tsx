import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { mintUrl } from '../../lib/sharedFileUrl'
import { useMediaElementPlayback } from '../../media/playback'
import { useLinkPreviewStore } from '../../store/linkPreviews'
import { useVoiceStore } from '../../store/voice'
import {
  useAssistantMedia,
  type AssistantMediaItem,
  type SequencedCommand,
} from '../../store/assistantMedia'
import { serverMicLevel, wantsServerRecognition } from '../../voice/serverStt'
import ModelViewer, { type ModelSource } from '../ModelViewer'
import type { ModelStage } from '../model/engine'
import { modelFormatFromMime, modelFormatFromName } from '../model/formats'

/** Video volume while someone is talking. */
const DUCKED_VOLUME = 0.2
/** How long after the last sign of speech the video stays lowered. */
const DUCK_HOLD_MS = 900
const DUCK_POLL_MS = 100
/** Echo-cancelled mic level (0..1) that counts as the user talking. */
const MIC_SPEECH_LEVEL = 0.12
const VOLUME_STEP_DOWN = 0.3
const VOLUME_STEP_UP = 0.06

/** Apply each new command once, as soon as the player can take it. */
function useCommands(ready: boolean, apply: (c: SequencedCommand) => void) {
  const commands = useAssistantMedia((s) => s.commands)
  const applied = useRef(0)
  useEffect(() => {
    if (!ready) return
    for (const c of commands) {
      if (c.seq <= applied.current) continue
      applied.current = c.seq
      apply(c)
    }
  }, [ready, commands, apply])
}

/**
 * Lower the video while anyone is talking, so voice commands get through:
 * the Assistant's voice, or the user's. Server recognition gives a real
 * (echo-cancelled) mic level; browser recognition exposes no audio, so there
 * the first interim result is the earliest sign, a moment after they start.
 */
function useVolumeDucking(ref: React.RefObject<HTMLVideoElement | null>) {
  const [ducked, setDucked] = useState(false)
  useEffect(() => {
    let lastSpeech = 0
    const timer = setInterval(() => {
      const el = ref.current
      if (!el) return
      const { interim, speakingText } = useVoiceStore.getState()
      const talking =
        interim !== '' ||
        speakingText !== '' ||
        (wantsServerRecognition() && serverMicLevel() > MIC_SPEECH_LEVEL)
      const now = Date.now()
      if (talking) lastSpeech = now
      const duck = now - lastSpeech < DUCK_HOLD_MS
      setDucked(duck)
      const target = duck ? DUCKED_VOLUME : 1
      el.volume =
        el.volume > target
          ? Math.max(target, el.volume - VOLUME_STEP_DOWN)
          : Math.min(target, el.volume + VOLUME_STEP_UP)
    }, DUCK_POLL_MS)
    return () => clearInterval(timer)
  }, [ref])
  return ducked
}

function StageVideo({
  src,
  title,
  refreshSrc,
}: {
  src: string
  title: string
  /** A fresh URL once a signed one expires (10 minutes). */
  refreshSrc?: () => Promise<string>
}) {
  const ref = useRef<HTMLVideoElement | null>(null)
  const [current, setCurrent] = useState(src)
  const [mutedByBrowser, setMutedByBrowser] = useState(false)
  const resumeAt = useRef<number | null>(null)
  const refreshedAt = useRef(0)
  useMediaElementPlayback(ref, current, { voiceControlled: true })
  const ducked = useVolumeDucking(ref)

  /** Start playing: browsers can refuse sound without a recent tap, and then
   *  muted playback is the only way to autoplay at all. */
  const start = useCallback(() => {
    const el = ref.current
    if (!el || document.hidden) return
    el.play().catch((e: Error) => {
      if (e.name !== 'NotAllowedError' || el.muted) return
      el.muted = true
      setMutedByBrowser(true)
      el.play().catch(() => undefined)
    })
  }, [])

  useEffect(() => {
    start()
  }, [current, start])

  const apply = useCallback(
    (c: SequencedCommand) => {
      const el = ref.current
      if (!el) return
      switch (c.action) {
        case 'play':
          start()
          break
        case 'pause':
          el.pause()
          break
        case 'mute':
          el.muted = true
          setMutedByBrowser(false)
          break
        case 'unmute':
          el.muted = false
          setMutedByBrowser(false)
          start()
          break
        case 'seek': {
          const to = 'seconds' in c ? c.seconds : el.currentTime + c.by
          const end = Number.isFinite(el.duration) ? el.duration : Infinity
          el.currentTime = Math.max(0, Math.min(end, to))
          break
        }
        default:
          break
      }
    },
    [start],
  )
  useCommands(true, apply)

  return (
    <>
      <video
        ref={ref}
        className="assistant-media-video"
        src={current}
        controls
        playsInline
        aria-label={title}
        data-testid="assistant-media-video"
        data-ducked={ducked ? 'true' : 'false'}
        onLoadedMetadata={(e) => {
          if (resumeAt.current === null) return
          e.currentTarget.currentTime = resumeAt.current
          resumeAt.current = null
          start()
        }}
        onVolumeChange={(e) => {
          if (!e.currentTarget.muted) setMutedByBrowser(false)
        }}
        onError={(e) => {
          // Most likely the signed URL expired mid-watch: seeking needs a
          // new request. Pick up where it was, but don't loop on a real error.
          if (!refreshSrc || Date.now() - refreshedAt.current < 60_000) return
          refreshedAt.current = Date.now()
          resumeAt.current = e.currentTarget.currentTime
          void refreshSrc()
            .then(setCurrent)
            .catch(() => undefined)
        }}
      />
      {mutedByBrowser && (
        <button
          type="button"
          className="assistant-media-sound"
          data-testid="assistant-media-sound"
          onClick={() => {
            const el = ref.current
            if (!el) return
            el.muted = false
            setMutedByBrowser(false)
          }}
        >
          Tap for sound
        </button>
      )}
    </>
  )
}

function StageImage({ src, title }: { src: string; title: string }) {
  const [view, setView] = useState({ degrees: 0, zoom: 1 })
  const apply = useCallback((c: SequencedCommand) => {
    if (c.action === 'rotate') setView((v) => ({ ...v, degrees: v.degrees + c.degrees }))
    else if (c.action === 'zoom')
      setView((v) => ({ ...v, zoom: Math.min(10, Math.max(0.1, v.zoom * c.factor)) }))
    else if (c.action === 'reset') setView({ degrees: 0, zoom: 1 })
  }, [])
  useCommands(true, apply)
  return (
    <img
      className="assistant-media-image"
      src={src}
      alt={title}
      data-testid="assistant-media-image"
      data-rotation={view.degrees}
      data-zoom={view.zoom}
      style={{ transform: `rotate(${view.degrees}deg) scale(${view.zoom})` }}
    />
  )
}

function StageModel({
  source,
  format,
}: {
  source: ModelSource
  format: NonNullable<ReturnType<typeof modelFormatFromName>>
}) {
  const hostRef = useRef<HTMLDivElement | null>(null)
  const [stage, setStage] = useState<ModelStage | null>(null)

  useEffect(() => {
    const host = hostRef.current
    if (!stage || !host) return
    /** The camera's real position, for anything checking the turn landed. */
    const report = () => {
      const { yaw, polar, zoom } = stage.viewState()
      host.dataset.yaw = yaw.toFixed(1)
      host.dataset.polar = polar.toFixed(1)
      host.dataset.zoom = zoom.toFixed(2)
    }
    stage.setOnSettled(report)
    report()
    return () => {
      stage.setOnSettled(null)
    }
  }, [stage])

  const apply = useCallback(
    (c: SequencedCommand) => {
      if (!stage) return
      if (c.action === 'rotate') stage.rotate(c.axis, c.degrees)
      else if (c.action === 'zoom') stage.zoom(c.factor)
      else if (c.action === 'reset') stage.fit()
    },
    [stage],
  )
  useCommands(stage !== null, apply)

  return (
    <div ref={hostRef} className="assistant-media-model" data-testid="assistant-media-model">
      <ModelViewer source={source} format={format} variant="modal" onStage={setStage} />
    </div>
  )
}

function Note({ children }: { children: string }) {
  return (
    <div className="assistant-media-note" role="status" data-testid="assistant-media-note">
      {children}
    </div>
  )
}

/** A local file: signed URLs from the shared-file route. */
function FileMedia({ media }: { media: AssistantMediaItem }) {
  const source = media.source
  const [src, setSrc] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const mint = useMemo(
    () => (source.type === 'file' ? () => mintUrl(source.sessionId, source.id) : null),
    [source],
  )
  useEffect(() => {
    if (!mint) return
    let cancelled = false
    mint()
      .then((url) => !cancelled && setSrc(url))
      .catch((e: Error) => !cancelled && setError(e.message))
    return () => {
      cancelled = true
    }
  }, [mint])
  const modelSource = useMemo<ModelSource | null>(
    () =>
      source.type === 'file' && mint
        ? { key: `shared:${source.id}`, name: source.name, url: mint }
        : null,
    [source, mint],
  )
  if (source.type !== 'file' || !mint) return null
  if (error) return <Note>{`Couldn't open it: ${error}`}</Note>
  if (media.kind === 'model') {
    const format = modelFormatFromName(source.name)
    if (!format || !modelSource) return <Note>This 3D format can't be previewed.</Note>
    return <StageModel source={modelSource} format={format} />
  }
  if (!src) return null
  return media.kind === 'video' ? (
    <StageVideo src={src} title={media.title} refreshSrc={mint} />
  ) : (
    <StageImage src={src} title={media.title} />
  )
}

/** A link: the server's signed proxy, because the page's CSP blocks remote
 *  media. Pages (YouTube and the like) aren't direct files, so voice can't
 *  drive them. */
function UrlMedia({ media }: { media: AssistantMediaItem }) {
  const url = media.source.type === 'url' ? media.source.url : ''
  const entry = useLinkPreviewStore((s) => s.entries[url])
  const load = useLinkPreviewStore((s) => s.load)
  useEffect(() => {
    if (url) load(url)
  }, [url, load])
  const preview = entry?.status === 'ok' ? entry.preview : null
  const modelSource = useMemo<ModelSource | null>(() => {
    const src = preview?.media?.src
    return src ? { key: `link:${url}`, name: media.title, url: () => Promise.resolve(src) } : null
  }, [preview, url, media.title])
  if (!entry || entry.status === 'loading') return null
  if (!preview) return <Note>That link couldn't be opened.</Note>
  const notDirect = "That link isn't a direct media file, so it can't be shown here."
  if (media.kind === 'image') {
    return preview.image ? (
      <StageImage src={preview.image} title={media.title} />
    ) : (
      <Note>{notDirect}</Note>
    )
  }
  if (media.kind === 'video') {
    return preview.media?.src ? (
      <StageVideo src={preview.media.src} title={media.title} />
    ) : (
      <Note>{notDirect}</Note>
    )
  }
  const format =
    modelFormatFromName(new URL(preview.final_url || url).pathname) ??
    modelFormatFromMime(preview.media?.mime ?? '')
  return modelSource && format ? (
    <StageModel source={modelSource} format={format} />
  ) : (
    <Note>{notDirect}</Note>
  )
}

/** The voice assistant's image, video, or 3D model, above the orb. */
export default function AssistantMediaStage() {
  const media = useAssistantMedia((s) => s.media)
  const hide = useAssistantMedia((s) => s.hide)
  if (!media) return null
  return (
    <section
      className="assistant-media-stage"
      data-testid="assistant-media-stage"
      data-kind={media.kind}
      aria-label={media.title || 'Media from the assistant'}
    >
      <div className="assistant-media-frame">
        {media.source.type === 'file' ? (
          <FileMedia key={media.key} media={media} />
        ) : (
          <UrlMedia key={media.key} media={media} />
        )}
      </div>
      <div className="assistant-media-bar">
        <span className="assistant-media-title" title={media.title}>
          {media.title}
        </span>
        <button
          type="button"
          className="assistant-media-close"
          onClick={hide}
          aria-label="Close"
          title="Close"
          data-testid="assistant-media-close"
        >
          {'×'}
        </button>
      </div>
    </section>
  )
}
