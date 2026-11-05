import { useCallback, useEffect, useRef, useState, type PointerEvent } from 'react'

import { useMockupPaneStore, type MockupDoc } from '../store/mockupPane'
import './MockupPane.css'

const WIDTH_KEY = 'peckboard_mockup_pane_width'
const DEFAULT_WIDTH = 640
const MIN_WIDTH = 320
/** Room always left for the view beside the pane. */
const MIN_VIEW_WIDTH = 360
const KEYBOARD_STEP = 40

function clampWidth(width: number): number {
  const max = Math.max(MIN_WIDTH, window.innerWidth - MIN_VIEW_WIDTH)
  return Math.min(max, Math.max(MIN_WIDTH, width))
}

function storedWidth(): number {
  const stored = Number(localStorage.getItem(WIDTH_KEY))
  return clampWidth(stored > 0 ? stored : DEFAULT_WIDTH)
}

/**
 * The right-hand pane an agent's HTML mockup runs in. A sibling of the main
 * content, so the view beside it shrinks to fit whichever screen is open.
 *
 * The mockup runs in `/mockup-frame` (see src/routes/mockup_frame.rs), which
 * reports ready and is then posted the HTML. `allow-same-origin` must never be
 * added to the sandbox: that opaque origin is what keeps a mockup away from
 * the user's login.
 */
export default function MockupPane() {
  const doc = useMockupPaneStore((s) => s.doc)
  const openId = useMockupPaneStore((s) => s.openId)
  if (!doc) return null
  // A new key per open: every mockup starts from a clean frame.
  return <OpenPane key={openId} doc={doc} />
}

function OpenPane({ doc }: { doc: MockupDoc }) {
  const close = useMockupPaneStore((s) => s.close)
  const [width, setWidth] = useState(storedWidth)
  const [resizing, setResizing] = useState(false)
  const [reloadCount, setReloadCount] = useState(0)
  const [error, setError] = useState<string | null>(null)
  const frameRef = useRef<HTMLIFrameElement | null>(null)
  const closeRef = useRef<HTMLButtonElement | null>(null)

  // Focus lands on Close so Esc works straight away, and goes back to whatever
  // opened the pane when it closes.
  useEffect(() => {
    const opener = document.activeElement
    closeRef.current?.focus()
    return () => {
      if (opener instanceof HTMLElement && opener.isConnected) opener.focus()
    }
  }, [])

  // Keep the view beside the pane usable when the window shrinks.
  useEffect(() => {
    const onResize = () => setWidth(clampWidth)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  useEffect(() => {
    let cancelled = false
    let sent = false
    // Fetched up front, so a failed load shows here rather than as a blank
    // frame. Either side may be ready first.
    const html = doc.load()
    html.catch((e: Error) => {
      if (!cancelled) setError(e.message || 'Could not load this mockup.')
    })
    const onMessage = (e: MessageEvent) => {
      // Only this frame, and only once: after load the frame's content is the
      // mockup's own code, which must not be able to drive this listener.
      if (sent || e.source !== frameRef.current?.contentWindow) return
      if ((e.data as { type?: unknown } | null)?.type !== 'mockup-frame-ready') return
      sent = true
      void html.then(
        (content) => {
          if (cancelled) return
          // The target is `*` because a sandboxed frame has an opaque origin.
          frameRef.current?.contentWindow?.postMessage(
            { type: 'mockup-frame-load', html: content },
            '*',
          )
        },
        () => {},
      )
    }
    window.addEventListener('message', onMessage)
    return () => {
      cancelled = true
      window.removeEventListener('message', onMessage)
    }
  }, [doc, reloadCount])

  const reload = useCallback(() => {
    setError(null)
    setReloadCount((n) => n + 1)
  }, [])

  const commitWidth = (next: number) => {
    const clamped = clampWidth(next)
    setWidth(clamped)
    localStorage.setItem(WIDTH_KEY, String(clamped))
  }

  // Pointer capture keeps the drag on the handle while the pointer crosses the
  // iframe, which would otherwise swallow the move events.
  const onHandleDown = (e: PointerEvent<HTMLDivElement>) => {
    e.currentTarget.setPointerCapture(e.pointerId)
    setResizing(true)
  }
  const onHandleMove = (e: PointerEvent<HTMLDivElement>) => {
    if (resizing) commitWidth(window.innerWidth - e.clientX)
  }

  return (
    <aside
      className={`mockup-pane${resizing ? ' mockup-pane--resizing' : ''}`}
      style={{ width }}
      aria-label={`Mockup: ${doc.title}`}
      data-testid="mockup-pane"
      onKeyDown={(e) => {
        if (e.key === 'Escape') close()
      }}
    >
      <div
        className="mockup-pane-handle"
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize mockup pane"
        aria-valuenow={width}
        tabIndex={0}
        data-testid="mockup-pane-handle"
        onPointerDown={onHandleDown}
        onPointerMove={onHandleMove}
        onPointerUp={() => setResizing(false)}
        onPointerCancel={() => setResizing(false)}
        onKeyDown={(e) => {
          if (e.key === 'ArrowLeft') commitWidth(width + KEYBOARD_STEP)
          if (e.key === 'ArrowRight') commitWidth(width - KEYBOARD_STEP)
        }}
      />
      <header className="mockup-pane-header">
        <h2 className="mockup-pane-title" title={doc.title} data-testid="mockup-pane-title">
          {doc.title}
        </h2>
        <button
          type="button"
          className="btn-secondary"
          onClick={reload}
          data-testid="mockup-pane-reload"
        >
          Reload
        </button>
        <button
          ref={closeRef}
          type="button"
          className="btn-secondary"
          onClick={close}
          data-testid="mockup-pane-close"
        >
          Close
        </button>
      </header>
      {error ? (
        <div className="mockup-pane-error" role="alert" data-testid="mockup-pane-error">
          {error}
        </div>
      ) : (
        <iframe
          key={reloadCount}
          ref={frameRef}
          className="mockup-pane-frame"
          title={doc.title}
          src="/mockup-frame"
          sandbox="allow-scripts"
          data-testid="mockup-pane-frame"
        />
      )}
    </aside>
  )
}
