import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from 'react'
import { MAX_MODEL_BYTES, type ModelFormat } from './model/formats'
import type { ModelInfo, ModelStage } from './model/engine'
import '../styles/model-viewer.css'

/** Where a viewer gets its bytes. */
export interface ModelSource {
  /** Stable identity (shared-file id, link URL): keys the byte cache, so
   *  the inline viewer and its fullscreen modal fetch once. */
  key: string
  name: string
  /** Known size, checked against the cap before anything is fetched. */
  size?: number
  /** A fetchable URL — called per load, so signed URLs are minted fresh. */
  url: () => Promise<string>
}

// ── WebGL probe ───────────────────────────────────────────────────────
const NO_WEBGL = '3D preview needs WebGL — download the file to open it in a 3D app.'

let webgl2: boolean | undefined

/** three r163+ needs WebGL2. Probed once, context freed right away. */
function webglSupported(): boolean {
  if (webgl2 !== undefined) return webgl2
  try {
    const gl = document.createElement('canvas').getContext('webgl2')
    webgl2 = !!gl
    gl?.getExtension('WEBGL_lose_context')?.loseContext()
  } catch {
    webgl2 = false
  }
  return webgl2
}

// ── The one live viewer ───────────────────────────────────────────────
// At most one viewer holds the WebGL canvas; the rest show a snapshot.
// Claiming demotes the current holder; releasing hands the canvas back to
// the most recent previous holder still mounted (closing the fullscreen
// modal re-activates the inline viewer it came from).

interface Slot {
  activate(): void
  deactivate(): void
}

let liveSlot: Slot | null = null
let previous: Slot[] = []

function claim(s: Slot) {
  if (liveSlot === s) return
  previous = previous.filter((p) => p !== s)
  if (liveSlot) {
    previous.push(liveSlot)
    liveSlot.deactivate()
  }
  liveSlot = s
  s.activate()
}

function claimIfFree(s: Slot) {
  if (!liveSlot) claim(s)
}

function release(s: Slot) {
  previous = previous.filter((p) => p !== s)
  if (liveSlot !== s) return
  liveSlot = null
  const next = previous.pop()
  if (next) claim(next)
}

// ── Bytes ─────────────────────────────────────────────────────────────

const bytesCache = new Map<string, Promise<ArrayBuffer>>()

function tooLargeMessage() {
  return `Too large to preview in the browser (over ${MAX_MODEL_BYTES / 1024 / 1024} MB) — download it to open it in a slicer or 3D app.`
}

function fetchModelBytes(src: ModelSource): Promise<ArrayBuffer> {
  const hit = bytesCache.get(src.key)
  if (hit) {
    bytesCache.delete(src.key)
    bytesCache.set(src.key, hit)
    return hit
  }
  const p = src
    .url()
    .then((u) => fetch(u))
    .then((r) => {
      if (!r.ok) throw new Error(r.status === 404 ? 'File no longer available' : `HTTP ${r.status}`)
      if (Number(r.headers.get('content-length')) > MAX_MODEL_BYTES)
        throw new Error(tooLargeMessage())
      return r.arrayBuffer()
    })
  p.catch(() => bytesCache.delete(src.key))
  bytesCache.set(src.key, p)
  // Small LRU: the last few files, so a modal or a remount doesn't refetch.
  while (bytesCache.size > 3) bytesCache.delete(bytesCache.keys().next().value as string)
  return p
}

// ── Theme ─────────────────────────────────────────────────────────────

function isDark(): boolean {
  const t = document.documentElement.getAttribute('data-theme')
  if (t === 'dark') return true
  if (t === 'light') return false
  return window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? false
}

function subscribeTheme(cb: () => void) {
  const mo = new MutationObserver(cb)
  mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] })
  const mq = window.matchMedia?.('(prefers-color-scheme: dark)')
  mq?.addEventListener('change', cb)
  return () => {
    mo.disconnect()
    mq?.removeEventListener('change', cb)
  }
}

function useDarkTheme(): boolean {
  return useSyncExternalStore(subscribeTheme, isDark)
}

// ── Formatting ────────────────────────────────────────────────────────

function fmtLen(n: number): string {
  return String(Number(n.toFixed(n >= 100 ? 0 : n >= 10 ? 1 : 2)))
}

function formatDims(info: ModelInfo): string {
  return `${info.size.map(fmtLen).join(' × ')} ${info.units === 'mm' ? 'mm' : 'units'}`
}

function formatCount(info: ModelInfo): string {
  if (info.layers) return `${info.layers.toLocaleString()} layers`
  return `${info.triangles.toLocaleString()} triangle${info.triangles === 1 ? '' : 's'}`
}

// ── Component ─────────────────────────────────────────────────────────

interface ModelViewerProps {
  source: ModelSource
  format: ModelFormat
  /** `modal` claims the live canvas at once and is taller. */
  variant?: 'inline' | 'modal'
  /** Inline: open the larger viewer (fullscreen button). */
  onExpand?: () => void
}

function ViewerNote({ state, children }: { state: string; children: ReactNode }) {
  return (
    <div className="model-viewer model-viewer-note" data-state={state} data-testid="model-viewer">
      <div className="shared-file-media-note" role="status" data-testid="model-viewer-note">
        {children}
      </div>
    </div>
  )
}

/**
 * Interactive preview of a 3D file — printable meshes (STL, 3MF, OBJ, PLY,
 * AMF), G-code toolpaths, and game assets (glTF/GLB, FBX, DAE, USDZ). The
 * three.js engine is a lazy chunk loaded on first use; only one viewer at a
 * time renders live (see `claim`), the rest show a snapshot until clicked.
 */
export default function ModelViewer(props: ModelViewerProps) {
  if (!webglSupported()) return <ViewerNote state="nowebgl">{NO_WEBGL}</ViewerNote>
  if (props.source.size !== undefined && props.source.size > MAX_MODEL_BYTES)
    return <ViewerNote state="toolarge">{tooLargeMessage()}</ViewerNote>
  return <LiveModelViewer {...props} />
}

function LiveModelViewer({ source, format, variant = 'inline', onExpand }: ModelViewerProps) {
  const modal = variant === 'modal'
  const rootRef = useRef<HTMLDivElement | null>(null)
  const hostRef = useRef<HTMLDivElement | null>(null)
  const engineRef = useRef<typeof import('./model/engine') | null>(null)
  const slotRef = useRef<Slot | null>(null)
  const [visible, setVisible] = useState(modal)
  const [loadWanted, setLoadWanted] = useState(modal)
  const [stage, setStage] = useState<ModelStage | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [live, setLive] = useState(false)
  const [snapshot, setSnapshot] = useState<{ url: string; dark: boolean } | null>(null)
  const [wire, setWire] = useState(false)
  const [layer, setLayer] = useState<number | null>(null)
  const [playing, setPlaying] = useState(true)
  const [clip, setClip] = useState(0)
  const [lost, setLost] = useState(false)
  const dark = useDarkTheme()
  const darkRef = useRef(dark)
  const playingRef = useRef(playing)

  // Off-screen viewers neither load nor render.
  useEffect(() => {
    if (modal) return
    const el = rootRef.current
    if (!el) return
    const io = new IntersectionObserver(
      ([e]) => {
        setVisible(e.isIntersecting)
        if (e.isIntersecting) setLoadWanted(true)
      },
      { rootMargin: '200px' },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [modal])

  useEffect(() => {
    if (!loadWanted) return
    let cancelled = false
    let loaded: ModelStage | null = null
    Promise.all([import('./model/engine'), fetchModelBytes(source)])
      .then(([engine, buf]) => {
        engineRef.current = engine
        return engine.loadModel(buf, format, darkRef.current, () => cancelled)
      })
      .then((st) => {
        if (cancelled) return st.dispose()
        loaded = st
        setStage(st)
      })
      .catch((e: Error) => !cancelled && setError(e.message))
    return () => {
      cancelled = true
      loaded?.dispose()
      setStage(null)
    }
  }, [loadWanted, source, format])

  // Register for the live canvas once parsed.
  useEffect(() => {
    if (!stage) return
    const slot: Slot = { activate: () => setLive(true), deactivate: () => setLive(false) }
    slotRef.current = slot
    if (modal) claim(slot)
    return () => {
      slotRef.current = null
      release(slot)
    }
  }, [stage, modal])

  useEffect(() => {
    if (stage && visible && !modal && slotRef.current) claimIfFree(slotRef.current)
  }, [stage, visible, modal])

  useEffect(() => {
    const host = hostRef.current
    if (!stage || !live || !host) return
    try {
      stage.attach(host)
    } catch {
      // Renderer creation failed (WebGL blocked / out of contexts).
      queueMicrotask(() => setError(NO_WEBGL))
      return
    }
    stage.setPlaying(playingRef.current)
    return () => {
      stage.detach()
      if (stage.lastSnapshot) setSnapshot({ url: stage.lastSnapshot, dark: darkRef.current })
    }
  }, [stage, live])

  useEffect(() => {
    if (live) stage?.setVisible(visible)
  }, [stage, live, visible])

  useEffect(() => {
    darkRef.current = dark
    stage?.setTheme(dark)
  }, [stage, dark])

  // Not live: a still, rendered once (and again after a theme change).
  const needSnapshot = !!stage && !live && visible && snapshot?.dark !== dark
  useEffect(() => {
    const host = hostRef.current
    if (!needSnapshot || !stage || !host) return
    const id = requestAnimationFrame(() => {
      try {
        const url = stage.snapshot(host.clientWidth, host.clientHeight)
        if (url) setSnapshot({ url, dark })
      } catch {
        setError(NO_WEBGL)
      }
    })
    return () => cancelAnimationFrame(id)
  }, [needSnapshot, stage, dark])

  useEffect(() => {
    if (!stage || !engineRef.current) return
    return engineRef.current.onContextLost(setLost)
  }, [stage])

  const info = stage?.info ?? null
  const state = error ? 'error' : !info ? 'loading' : live ? 'live' : 'snapshot'
  const activate = () => slotRef.current && claim(slotRef.current)

  return (
    <div
      ref={rootRef}
      className={`model-viewer${modal ? ' model-viewer-modal' : ''}`}
      data-state={state}
      data-testid="model-viewer"
    >
      <div className="model-viewer-stage">
        <div ref={hostRef} className="model-viewer-host" data-testid="model-canvas-host" />
        {state === 'loading' && (
          <div className="model-viewer-overlay" role="status">
            Loading 3D model…
          </div>
        )}
        {state === 'error' && (
          <div className="model-viewer-overlay" role="alert" data-testid="model-viewer-error">
            {error}
          </div>
        )}
        {state === 'snapshot' && (
          <button
            type="button"
            className="model-viewer-snapshot"
            onClick={activate}
            aria-label={`Show ${source.name} in 3D`}
            data-testid="model-snapshot-button"
          >
            {snapshot && <img src={snapshot.url} alt="" data-testid="model-snapshot" />}
            <span className="model-viewer-activate">Click to view in 3D</span>
          </button>
        )}
        {state === 'live' && lost && (
          <div className="model-viewer-overlay" role="status">
            Graphics context lost — restoring…
          </div>
        )}
      </div>
      {info && stage && (
        <div className="model-viewer-toolbar">
          <span
            className="model-viewer-stat"
            data-testid="model-dims"
            title="Bounding box X × Y × Z"
          >
            {formatDims(info)}
          </span>
          <span className="model-viewer-stat" data-testid="model-tris">
            {formatCount(info)}
          </span>
          <span className="model-viewer-spacer" />
          {live && info.layers > 1 && (
            <label className="model-viewer-layer">
              Layer
              <input
                type="range"
                min={1}
                max={info.layers}
                value={(layer ?? info.layers - 1) + 1}
                onChange={(e) => {
                  const n = Number(e.target.value) - 1
                  setLayer(n)
                  stage.setLayer(n)
                }}
                aria-label="G-code layer"
                data-testid="model-layer"
              />
              <span className="model-viewer-stat">{(layer ?? info.layers - 1) + 1}</span>
            </label>
          )}
          {live && info.clips.length > 0 && (
            <>
              {info.clips.length > 1 && (
                <select
                  className="model-viewer-select"
                  value={clip}
                  onChange={(e) => {
                    const i = Number(e.target.value)
                    setClip(i)
                    stage.setClip(i)
                  }}
                  aria-label="Animation clip"
                >
                  {info.clips.map((name, i) => (
                    <option key={i} value={i}>
                      {name}
                    </option>
                  ))}
                </select>
              )}
              <button
                type="button"
                className="model-viewer-btn"
                aria-pressed={playing}
                onClick={() => {
                  playingRef.current = !playing
                  setPlaying(!playing)
                  stage.setPlaying(!playing)
                }}
              >
                {playing ? 'Pause' : 'Play'}
              </button>
            </>
          )}
          {live && (
            <>
              <button type="button" className="model-viewer-btn" onClick={() => stage.fit()}>
                Reset view
              </button>
              <button
                type="button"
                className="model-viewer-btn"
                aria-pressed={wire}
                onClick={() => {
                  setWire(!wire)
                  stage.setWireframe(!wire)
                }}
              >
                Wireframe
              </button>
            </>
          )}
          {onExpand && (
            <button
              type="button"
              className="model-viewer-btn"
              onClick={onExpand}
              data-testid="model-expand"
            >
              Fullscreen
            </button>
          )}
        </div>
      )}
    </div>
  )
}
