import { useCallback, useEffect, useState } from 'react'
import Modal from './Modal'
import { CodePreview } from './ToolUseBlock'
import type { SharedFile } from './chat/events'
import { authedFetch } from '../store/auth'

/** Previewable kinds of a shared file. Text is decided by extension first —
 *  `mime_guess` maps e.g. `.ts` to a video type. */
type SharedFileKind = 'image' | 'video' | 'text' | 'other'

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
const VIDEO_EXTS = new Set(['mp4', 'webm', 'mov'])
const ARCHIVE_EXTS = new Set(['zip', 'tar', 'gz', 'tgz', 'bz2', 'xz', '7z', 'rar'])

/** Text files above this are download-only (no inline viewer). */
const MAX_TEXT_PREVIEW_BYTES = 1024 * 1024

function extOf(name: string): string {
  return name.match(/\.([A-Za-z0-9]+)$/)?.[1]?.toLowerCase() ?? ''
}

function sharedFileKind(file: SharedFile): SharedFileKind {
  const ext = extOf(file.filename)
  if (TEXT_EXTS.has(ext)) return 'text'
  if (IMAGE_EXTS.has(ext)) return 'image'
  if (VIDEO_EXTS.has(ext)) return 'video'
  // SVG is scriptable: never rendered inline, download only.
  if (ext === 'svg' || ext === 'html' || ext === 'htm') return 'other'
  if (/^image\/(png|jpeg|gif|webp)$/.test(file.mimeType)) return 'image'
  if (/^video\/(mp4|webm|quicktime)$/.test(file.mimeType)) return 'video'
  if (file.mimeType.startsWith('text/') || file.mimeType === 'application/json') return 'text'
  return 'other'
}

function fileIcon(file: SharedFile, kind: SharedFileKind): string {
  const ext = extOf(file.filename)
  if (kind === 'image') return '\u{1F5BC}\u{FE0F}'
  if (kind === 'video') return '\u{1F3AC}'
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
  // A link expires after 10 minutes; media that errors re-mints once.
  const refresh = useCallback(() => setNonce((n) => (n < 1 ? n + 1 : n)), [])
  return { url, error, refresh }
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

/** Full preview of one shared file: image, video, text/code, or a
 *  "no preview" notice. Download always available in the header. */
export function FilePreviewModal({
  sessionId,
  file,
  onClose,
}: {
  sessionId: string
  file: SharedFile
  onClose: () => void
}) {
  const kind = sharedFileKind(file)
  const textTooBig = kind === 'text' && file.size > MAX_TEXT_PREVIEW_BYTES
  const previewable = kind !== 'other' && !textTooBig
  const { url, error, refresh } = useSignedUrl(sessionId, file.id, previewable)
  const [text, setText] = useState<string | null>(null)
  const [textError, setTextError] = useState<string | null>(null)

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
        {textTooBig ? 'File too large to preview.' : 'No preview available for this file type.'}
      </div>
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
  } else if (kind === 'video') {
    body = (
      <video
        className="shared-file-preview-video"
        src={url}
        controls
        autoPlay
        playsInline
        data-testid="shared-file-preview-video"
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
      onClose={onClose}
      className="shared-file-modal"
      maxWidth="min(960px, 96vw)"
      data-testid="shared-file-modal"
    >
      <div className="shared-file-modal-header">
        <h3 className="shared-file-modal-title" title={file.sourcePath ?? file.filename}>
          {file.filename}
        </h3>
        <span className="shared-file-size">{formatBytes(file.size)}</span>
        <DownloadButton sessionId={sessionId} file={file} className="btn-primary" />
        <button
          type="button"
          className="btn-secondary"
          onClick={onClose}
          aria-label="Close preview"
        >
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
 * size, caption, an inline thumbnail for images / videos, and an always-
 * visible Download button. Clicking the card opens `FilePreviewModal`.
 */
export default function SharedFileCard({
  sessionId,
  file,
}: {
  sessionId: string
  file: SharedFile
}) {
  const kind = sharedFileKind(file)
  const [open, setOpen] = useState(false)
  const media = kind === 'image' || kind === 'video'
  const { url, refresh } = useSignedUrl(sessionId, file.id, media)

  return (
    <div className="shared-file-card" data-testid="shared-file-card" data-kind={kind}>
      <button
        type="button"
        className="shared-file-open"
        onClick={() => setOpen(true)}
        aria-label={`Preview ${file.filename}`}
        data-testid="shared-file-open"
      >
        {media && url && kind === 'image' && (
          <img
            className="shared-file-thumb"
            src={url}
            alt=""
            loading="lazy"
            onError={refresh}
            data-testid="shared-file-thumb"
          />
        )}
        {media && url && kind === 'video' && (
          <video
            className="shared-file-thumb"
            src={url}
            preload="metadata"
            muted
            playsInline
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
            </span>
            {file.caption && <span className="shared-file-caption">{file.caption}</span>}
          </span>
        </span>
      </button>
      <DownloadButton sessionId={sessionId} file={file} />
      {open && (
        <FilePreviewModal sessionId={sessionId} file={file} onClose={() => setOpen(false)} />
      )}
    </div>
  )
}
