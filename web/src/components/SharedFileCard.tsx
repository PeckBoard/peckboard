import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type RefObject,
  type SyntheticEvent,
} from 'react'
import Modal from './Modal'
import AudioPlayer from './AudioPlayer'
import { CodePreview } from './ToolUseBlock'
import type { SharedFile } from './chat/events'
import { authedFetch } from '../store/auth'
import { useMediaElementPlayback } from '../media/playback'
import ModelViewer, { type ModelSource } from './ModelViewer'
import {
  isCadFile,
  MAX_MODEL_BYTES,
  modelFormatFromName,
  printFarmAccepts,
  sniffModelFormat,
  SNIFF_BYTES,
  type ModelFormat,
} from './model/formats'

/** Previewable kinds of a shared file. Text is decided by extension first —
 *  `mime_guess` maps e.g. `.ts` to a video type. `model` is an inline 3D
 *  viewer; `cad` (STEP / IGES / .blend) is download-only. */
type SharedFileKind = 'image' | 'video' | 'audio' | 'text' | 'model' | 'cad' | 'other'

const TEXT_EXTS = new Set([
  'txt',
  'md',
  'markdown',
  'log',
  'csv',
  'tsv',
  'json',
  'jsonl',
  'yaml',
  'yml',
  'toml',
  'ini',
  'xml',
  'rs',
  'ts',
  'tsx',
  'js',
  'jsx',
  'mjs',
  'py',
  'go',
  'java',
  'kt',
  'c',
  'h',
  'cpp',
  'hpp',
  'cs',
  'rb',
  'php',
  'sh',
  'bash',
  'zsh',
  'sql',
  'css',
  'scss',
  'diff',
  'patch',
  'env',
  'conf',
  'cfg',
])
const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp'])
const VIDEO_EXTS = new Set(['mp4', 'webm', 'mov', 'm4v', 'ogv'])
const AUDIO_EXTS = new Set(['mp3', 'wav', 'ogg', 'oga', 'opus', 'flac', 'm4a', 'aac', 'weba'])
const ARCHIVE_EXTS = new Set(['zip', 'tar', 'gz', 'tgz', 'bz2', 'xz', '7z', 'rar'])

/** Text files above this are download-only (no inline viewer). */
const MAX_TEXT_PREVIEW_BYTES = 1024 * 1024
/** Audio above this skips the waveform (it downloads + decodes the whole
 *  file) and shows a plain progress bar. */
const MAX_WAVEFORM_BYTES = 25 * 1024 * 1024

function extOf(name: string): string {
  return name.match(/\.([A-Za-z0-9]+)$/)?.[1]?.toLowerCase() ?? ''
}

function sharedFileKind(file: SharedFile): SharedFileKind {
  const ext = extOf(file.filename)
  if (modelFormatFromName(file.filename)) return 'model'
  if (isCadFile(file.filename)) return 'cad'
  if (TEXT_EXTS.has(ext)) return 'text'
  if (IMAGE_EXTS.has(ext)) return 'image'
  if (VIDEO_EXTS.has(ext)) return 'video'
  if (AUDIO_EXTS.has(ext)) return 'audio'
  // SVG is scriptable: never rendered inline, download only. MIDI has no
  // browser decoder.
  if (['svg', 'html', 'htm', 'mid', 'midi'].includes(ext)) return 'other'
  if (/^image\/(png|jpeg|gif|webp)$/.test(file.mimeType)) return 'image'
  if (/^video\/(mp4|webm|quicktime|ogg|x-m4v)$/.test(file.mimeType)) return 'video'
  if (file.mimeType.startsWith('audio/') && !file.mimeType.includes('midi')) return 'audio'
  if (file.mimeType.startsWith('text/') || file.mimeType === 'application/json') return 'text'
  return 'other'
}

function fileIcon(file: SharedFile, kind: SharedFileKind): string {
  const ext = extOf(file.filename)
  if (kind === 'model') return '\u{1F9CA}'
  if (kind === 'cad') return '\u{1F4D0}'
  if (kind === 'image') return '\u{1F5BC}\u{FE0F}'
  if (kind === 'video') return '\u{1F3AC}'
  if (kind === 'audio') return '\u{1F3B5}'
  if (ARCHIVE_EXTS.has(ext)) return '\u{1F5DC}\u{FE0F}'
  if (ext === 'pdf') return '\u{1F4D1}'
  if (kind === 'text') return '\u{1F4C4}'
  return '\u{1F4E6}'
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`
  const units = ['KB', 'MB', 'GB']
  let v = n / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v >= 10 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`
}

/** Mint a fresh 10-minute signed URL for one shared file. */
async function mintUrl(sessionId: string, fileId: string): Promise<string> {
  const res = await authedFetch(`/api/sessions/${sessionId}/shared-files/${fileId}/url`, {
    method: 'POST',
  })
  if (!res.ok)
    throw new Error(res.status === 404 ? 'File no longer available' : `HTTP ${res.status}`)
  const { url } = (await res.json()) as { url: string }
  return url
}

/** Start a browser download: the signed URL with `download=1` answers with
 *  `Content-Disposition: attachment`, so navigating to it keeps the page. */
async function downloadSharedFile(sessionId: string, file: SharedFile): Promise<void> {
  const url = await mintUrl(sessionId, file.id)
  const a = document.createElement('a')
  a.href = `${url}&download=1`
  a.rel = 'noopener'
  document.body.appendChild(a)
  a.click()
  a.remove()
}

// ── Print-farm hand-off ───────────────────────────────────────────────

const PRINT_FARM_API = '/api/plugin-ui/print-farm'
/** Raw bytes per upload request → exactly 1 MiB of base64, print-farm's cap. */
const PRINT_FARM_CHUNK = 768 * 1024

let printFarmProbe: Promise<boolean> | null = null

/** Is the print-farm plugin active? Read from the `/api/plugins` catalog
 *  (its sidebar entry), once; re-read after a plugin approval changes. */
function probePrintFarm(): Promise<boolean> {
  if (!printFarmProbe) {
    window.addEventListener('peckboard:plugin-approval', () => (printFarmProbe = null), {
      once: true,
    })
    printFarmProbe = authedFetch('/api/plugins')
      .then((r) => (r.ok ? r.json() : null))
      .then((b: { sidebar_items?: { plugin: string }[] } | null) =>
        (b?.sidebar_items ?? []).some((i) => i.plugin === 'print-farm'),
      )
      .catch(() => false)
  }
  return printFarmProbe
}

function usePrintFarm(enabled: boolean): boolean {
  const [active, setActive] = useState(false)
  useEffect(() => {
    if (!enabled) return
    let cancelled = false
    probePrintFarm().then((on) => !cancelled && setActive(on))
    return () => {
      cancelled = true
    }
  }, [enabled])
  return enabled && active
}

function bytesToB64(bytes: Uint8Array): string {
  let s = ''
  for (let i = 0; i < bytes.length; i += 0x8000)
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000))
  return btoa(s)
}

interface PrintFarmFile {
  id: string
  size: number
  complete: boolean
}

async function printFarmPost(path: string, body: unknown): Promise<PrintFarmFile> {
  const res = await authedFetch(`${PRINT_FARM_API}/${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
  const parsed = (await res.json().catch(() => ({}))) as { file?: PrintFarmFile; error?: string }
  if (!res.ok || typeof parsed.error === 'string' || !parsed.file)
    throw new Error(parsed.error ?? `Print Farm upload failed (HTTP ${res.status})`)
  return parsed.file
}

/** Upload a shared file to print-farm (its chunked `uploads` API, the same
 *  one its Slice tab uses), then open the Print Farm page on that tab. */
async function sendToPrinter(sessionId: string, file: SharedFile): Promise<void> {
  const res = await fetch(await mintUrl(sessionId, file.id))
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  const bytes = new Uint8Array(await res.arrayBuffer())
  const chunk = (off: number) => bytesToB64(bytes.subarray(off, off + PRINT_FARM_CHUNK))
  let rec = await printFarmPost('uploads', {
    name: file.filename,
    data_base64: chunk(0),
    done: bytes.length <= PRINT_FARM_CHUNK,
  })
  while (!rec.complete) {
    const off = rec.size
    const next = await printFarmPost(`uploads/${encodeURIComponent(rec.id)}/chunk`, {
      offset: off,
      data_base64: chunk(off),
      done: off + PRINT_FARM_CHUNK >= bytes.length,
    })
    if (!next.complete && next.size <= off) throw new Error('Print Farm upload stalled')
    rec = next
  }
  window.dispatchEvent(
    new CustomEvent('peckboard:open-plugin-page', {
      detail: { plugin: 'print-farm', item: 'print-farm', query: 'tab=slice' },
    }),
  )
}

// ── 3D files ──────────────────────────────────────────────────────────

/** Viewer byte source for a shared file (signed URL minted per load). */
function useModelSource(sessionId: string, file: SharedFile): ModelSource {
  return useMemo(
    () => ({
      key: `shared:${file.id}`,
      name: file.filename,
      size: file.size,
      url: () => mintUrl(sessionId, file.id),
    }),
    [sessionId, file.id, file.filename, file.size],
  )
}

const NEVER_SNIFF = new Set(['pdf', 'svg', 'html', 'htm', 'mid', 'midi'])

/** An unrecognised binary (no / unknown extension) may still be a binary
 *  STL or a GLB: worth reading its first bytes. */
function sniffable(file: SharedFile): boolean {
  const ext = extOf(file.filename)
  return (
    !NEVER_SNIFF.has(ext) &&
    !ARCHIVE_EXTS.has(ext) &&
    (!file.mimeType || file.mimeType === 'application/octet-stream') &&
    file.size >= 12 &&
    file.size <= MAX_MODEL_BYTES
  )
}

/** The 3D format of a file by its magic bytes (one Range request). */
function useSniffedModel(sessionId: string, file: SharedFile, enabled: boolean) {
  const [format, setFormat] = useState<ModelFormat | null>(null)
  useEffect(() => {
    if (!enabled) return
    let cancelled = false
    mintUrl(sessionId, file.id)
      .then((u) => fetch(u, { headers: { Range: `bytes=0-${SNIFF_BYTES - 1}` } }))
      .then((r) => (r.ok ? r.arrayBuffer() : null))
      .then((b) => {
        if (cancelled || !b) return
        const head = new Uint8Array(b, 0, Math.min(SNIFF_BYTES, b.byteLength))
        setFormat(sniffModelFormat(head, file.size))
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [sessionId, file.id, file.size, enabled])
  return enabled ? format : null
}
/** Signed URL for inline media; `null` until minted (or when disabled). */
function useSignedUrl(sessionId: string, fileId: string, enabled: boolean) {
  const [url, setUrl] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [nonce, setNonce] = useState(0)
  useEffect(() => {
    if (!enabled) return
    let cancelled = false
    mintUrl(sessionId, fileId)
      .then((u) => !cancelled && setUrl(u))
      .catch((e: Error) => !cancelled && setError(e.message))
    return () => {
      cancelled = true
    }
  }, [sessionId, fileId, enabled, nonce])
  // A link expires after 10 minutes; an image that errors re-mints once.
  const refresh = useCallback(() => setNonce((n) => (n < 1 ? n + 1 : n)), [])
  // Media outlives one link (a long clip); `useMediaSource` guards loops.
  const remint = useCallback(() => setNonce((n) => n + 1), [])
  return { url, error, refresh, remint }
}

/** Where a player picks up: position (s) and whether it was playing. */
interface ResumePoint {
  time: number
  play: boolean
}

/**
 * Signed `src` for an `<audio>` / `<video>` that survives link expiry: an
 * error on a link that already loaded re-mints it and resumes at the same
 * position and play state. An error on a link that never loaded means the
 * browser can't decode the file → `unsupported`.
 */
function useMediaSource(sessionId: string, fileId: string, start?: ResumePoint) {
  const { url, error, remint } = useSignedUrl(sessionId, fileId, true)
  const [unsupported, setUnsupported] = useState(false)
  const resume = useRef<ResumePoint | null>(start ?? null)
  const loadedUrl = useRef<string | null>(null)
  const onLoadedMetadata = useCallback(
    (e: SyntheticEvent<HTMLMediaElement>) => {
      loadedUrl.current = url
      const el = e.currentTarget
      const r = resume.current
      resume.current = null
      if (!r) return
      if (r.time > 0 && Number.isFinite(r.time)) el.currentTime = r.time
      if (r.play) void el.play().catch(() => {})
    },
    [url],
  )
  const onError = useCallback(
    (e: SyntheticEvent<HTMLMediaElement>) => {
      if (!url || loadedUrl.current !== url) {
        setUnsupported(true)
        return
      }
      const el = e.currentTarget
      resume.current ??= { time: el.currentTime, play: !el.paused }
      remint()
    },
    [url, remint],
  )
  return { url, error, unsupported, onLoadedMetadata, onError }
}

function DownloadButton({
  sessionId,
  file,
  className,
}: {
  sessionId: string
  file: SharedFile
  className?: string
}) {
  const [state, setState] = useState<'idle' | 'busy' | 'failed'>('idle')
  return (
    <button
      type="button"
      className={className ?? 'btn-secondary shared-file-download'}
      data-testid="shared-file-download"
      aria-label={`Download ${file.filename}`}
      disabled={state === 'busy'}
      onClick={(e) => {
        e.stopPropagation()
        setState('busy')
        downloadSharedFile(sessionId, file)
          .then(() => setState('idle'))
          .catch(() => setState('failed'))
      }}
    >
      {state === 'busy' ? 'Preparing…' : state === 'failed' ? 'Retry download' : 'Download'}
    </button>
  )
}

function SendToPrinterButton({ sessionId, file }: { sessionId: string; file: SharedFile }) {
  const [state, setState] = useState<'idle' | 'busy' | 'failed'>('idle')
  const [error, setError] = useState<string | null>(null)
  return (
    <button
      type="button"
      className="btn-secondary shared-file-download"
      data-testid="shared-file-send-to-printer"
      title={error ?? 'Upload to Print Farm and open its Slice tab'}
      disabled={state === 'busy'}
      onClick={(e) => {
        e.stopPropagation()
        setState('busy')
        sendToPrinter(sessionId, file)
          .then(() => setState('idle'))
          .catch((err: Error) => {
            setError(err.message)
            setState('failed')
          })
      }}
    >
      {state === 'busy' ? 'Sending…' : state === 'failed' ? 'Retry send' : 'Send to printer'}
    </button>
  )
}
function MediaUnsupported() {
  return (
    <div className="shared-file-media-note" role="status" data-testid="shared-file-unsupported">
      Your browser can't play this format. Download it to open it in another app.
    </div>
  )
}

function VideoPlayer({
  src,
  videoRef,
  className,
  testId,
  autoFrame,
  onLoadedMetadata,
  onError,
}: {
  src: string
  videoRef: RefObject<HTMLMediaElement | null>
  className: string
  testId: string
  /** Show the first frame as the poster (iOS paints nothing on `metadata`). */
  autoFrame: boolean
  onLoadedMetadata: (e: SyntheticEvent<HTMLMediaElement>) => void
  onError: (e: SyntheticEvent<HTMLMediaElement>) => void
}) {
  useMediaElementPlayback(videoRef, src)
  return (
    <video
      ref={videoRef as RefObject<HTMLVideoElement | null>}
      className={className}
      src={autoFrame ? `${src}#t=0.001` : src}
      controls
      preload="metadata"
      playsInline
      data-testid={testId}
      onLoadedMetadata={onLoadedMetadata}
      onError={onError}
    />
  )
}

/** Inline video / audio for a shared file, with signed-URL renewal and the
 *  "can't play" fallback. `mediaRef` exposes the element to the caller. */
function SharedMedia({
  sessionId,
  file,
  kind,
  start,
  mediaRef,
  className = 'shared-file-preview-video',
  testId = 'shared-file-preview-video',
}: {
  sessionId: string
  file: SharedFile
  kind: 'video' | 'audio'
  start?: ResumePoint
  mediaRef: RefObject<HTMLMediaElement | null>
  className?: string
  testId?: string
}) {
  const { url, error, unsupported, onLoadedMetadata, onError } = useMediaSource(
    sessionId,
    file.id,
    start,
  )
  if (unsupported) return <MediaUnsupported />
  if (error) return <div className="shared-file-media-note">{error}</div>
  if (!url) return <div className="shared-file-media-note">Loading…</div>
  if (kind === 'audio') {
    return (
      <AudioPlayer
        src={url}
        label={file.filename}
        waveformKey={file.size <= MAX_WAVEFORM_BYTES ? file.id : null}
        audioRef={mediaRef as RefObject<HTMLAudioElement | null>}
        onLoadedMetadata={onLoadedMetadata}
        onError={onError}
      />
    )
  }
  return (
    <VideoPlayer
      src={url}
      videoRef={mediaRef}
      className={className}
      testId={testId}
      autoFrame={!start}
      onLoadedMetadata={onLoadedMetadata}
      onError={onError}
    />
  )
}

/** Full preview of one shared file: image, video, audio, text/code, a 3D
 *  viewer, or a "no preview" notice. Download always available in the
 *  header. Media starts at `start` and reports where it stopped through
 *  `onClose`. `modelFormat` carries a format the card sniffed from the
 *  file's bytes (no / wrong extension). */
export function FilePreviewModal({
  sessionId,
  file,
  start,
  modelFormat,
  onClose,
}: {
  sessionId: string
  file: SharedFile
  start?: ResumePoint
  modelFormat?: ModelFormat | null
  onClose: (time?: number) => void
}) {
  const format = modelFormat ?? modelFormatFromName(file.filename)
  const baseKind = sharedFileKind(file)
  const kind: SharedFileKind = baseKind === 'other' && format ? 'model' : baseKind
  const media = kind === 'video' || kind === 'audio'
  const textTooBig = kind === 'text' && file.size > MAX_TEXT_PREVIEW_BYTES
  const previewable = kind !== 'other' && kind !== 'cad' && !textTooBig
  const { url, error, refresh } = useSignedUrl(
    sessionId,
    file.id,
    previewable && !media && kind !== 'model',
  )
  const modelSource = useModelSource(sessionId, file)
  const [text, setText] = useState<string | null>(null)
  const [textError, setTextError] = useState<string | null>(null)
  const mediaRef = useRef<HTMLMediaElement | null>(null)
  const close = useCallback(() => onClose(mediaRef.current?.currentTime), [onClose])

  useEffect(() => {
    if (kind !== 'text' || !url || textTooBig) return
    let cancelled = false
    fetch(url)
      .then((r) => (r.ok ? r.text() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((t) => !cancelled && setText(t))
      .catch((e: Error) => !cancelled && setTextError(e.message))
    return () => {
      cancelled = true
    }
  }, [kind, url, textTooBig])

  let body
  if (!previewable) {
    body = (
      <div className="shared-file-nopreview" data-testid="shared-file-nopreview">
        {textTooBig
          ? 'File too large to preview.'
          : kind === 'cad'
            ? 'CAD files need a CAD app — download it to open it there.'
            : 'No preview available for this file type.'}
      </div>
    )
  } else if (kind === 'model' && format) {
    body = <ModelViewer source={modelSource} format={format} variant="modal" />
  } else if (media) {
    body = (
      <SharedMedia
        sessionId={sessionId}
        file={file}
        kind={kind}
        start={start ?? { time: 0, play: true }}
        mediaRef={mediaRef}
      />
    )
  } else if (error || textError) {
    body = <div className="shared-file-nopreview">{error ?? textError}</div>
  } else if (!url || (kind === 'text' && text === null)) {
    body = <div className="shared-file-nopreview">Loading…</div>
  } else if (kind === 'image') {
    body = (
      <img
        className="shared-file-preview-img"
        src={url}
        alt={file.filename}
        data-testid="shared-file-preview-img"
        onError={refresh}
      />
    )
  } else {
    body = (
      <div className="shared-file-preview-text" data-testid="shared-file-preview-text">
        <CodePreview path={file.filename} content={text ?? ''} />
      </div>
    )
  }

  return (
    <Modal
      onClose={close}
      className="shared-file-modal"
      maxWidth={kind === 'model' ? 'min(1200px, 96vw)' : 'min(960px, 96vw)'}
      data-testid="shared-file-modal"
    >
      <div className="shared-file-modal-header">
        <h3 className="shared-file-modal-title" title={file.sourcePath ?? file.filename}>
          {file.filename}
        </h3>
        <span className="shared-file-size">{formatBytes(file.size)}</span>
        <DownloadButton sessionId={sessionId} file={file} className="btn-primary" />
        <button type="button" className="btn-secondary" onClick={close} aria-label="Close preview">
          Close
        </button>
      </div>
      {file.caption && <p className="shared-file-modal-caption">{file.caption}</p>}
      <div className="shared-file-modal-body">{body}</div>
    </Modal>
  )
}

/**
 * Chat card for a file an agent shared (`file-shared` event): icon, name,
 * size, caption, an inline thumbnail for images, an inline player for
 * video / audio, an interactive viewer for 3D files, and an always-visible
 * Download button (plus "Send to printer" for printables when the
 * print-farm plugin is active). Clicking the name opens `FilePreviewModal`;
 * media there continues from the inline position.
 */
export default function SharedFileCard({
  sessionId,
  file,
}: {
  sessionId: string
  file: SharedFile
}) {
  const baseKind = sharedFileKind(file)
  const sniffed = useSniffedModel(sessionId, file, baseKind === 'other' && sniffable(file))
  const modelFormat = modelFormatFromName(file.filename) ?? sniffed
  const kind: SharedFileKind = baseKind === 'other' && sniffed ? 'model' : baseKind
  const [preview, setPreview] = useState<{ start?: ResumePoint } | null>(null)
  const { url, refresh } = useSignedUrl(sessionId, file.id, kind === 'image')
  const mediaRef = useRef<HTMLMediaElement | null>(null)
  const modelSource = useModelSource(sessionId, file)
  const printFarm = usePrintFarm(
    (kind === 'model' || kind === 'cad') && printFarmAccepts(file.filename),
  )

  const openPreview = () => {
    const el = mediaRef.current
    // The modal picks up where the inline player is; the inline one pauses.
    const start = el ? { time: el.currentTime, play: true } : undefined
    el?.pause()
    setPreview({ start })
  }
  const closePreview = (time?: number) => {
    setPreview(null)
    const el = mediaRef.current
    if (el && time !== undefined && Number.isFinite(time)) el.currentTime = time
  }

  return (
    <div className="shared-file-card" data-testid="shared-file-card" data-kind={kind}>
      {kind === 'video' && (
        <SharedMedia
          sessionId={sessionId}
          file={file}
          kind="video"
          mediaRef={mediaRef}
          className="shared-file-video"
          testId="shared-file-video"
        />
      )}
      {kind === 'audio' && (
        <SharedMedia sessionId={sessionId} file={file} kind="audio" mediaRef={mediaRef} />
      )}
      {kind === 'model' && modelFormat && (
        <ModelViewer source={modelSource} format={modelFormat} onExpand={openPreview} />
      )}
      <button
        type="button"
        className="shared-file-open"
        onClick={openPreview}
        aria-label={`Preview ${file.filename}`}
        data-testid="shared-file-open"
      >
        {kind === 'image' && url && (
          <img
            className="shared-file-thumb"
            src={url}
            alt=""
            loading="lazy"
            onError={refresh}
            data-testid="shared-file-thumb"
          />
        )}
        <span className="shared-file-meta">
          <span className="shared-file-icon" aria-hidden="true">
            {fileIcon(file, kind)}
          </span>
          <span className="shared-file-text">
            <span className="shared-file-name" title={file.sourcePath ?? file.filename}>
              {file.filename}
            </span>
            <span className="shared-file-sub">
              {formatBytes(file.size)}
              {file.origin === 'generated' ? ' · generated' : ''}
              {kind === 'cad' ? ' · needs a CAD app' : ''}
            </span>
            {file.caption && <span className="shared-file-caption">{file.caption}</span>}
          </span>
        </span>
      </button>
      <DownloadButton sessionId={sessionId} file={file} />
      {printFarm && <SendToPrinterButton sessionId={sessionId} file={file} />}
      {preview && (
        <FilePreviewModal
          sessionId={sessionId}
          file={file}
          start={preview.start}
          modelFormat={modelFormat}
          onClose={closePreview}
        />
      )}
    </div>
  )
}
