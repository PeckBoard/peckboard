import { useEffect, useRef } from 'react'
import Modal from './Modal'
import { authedFetch } from '../store/auth'

interface Props {
  /** Human label for the panel (the iframe/page title). */
  title: string
  /** Plugin that declared this panel — only used for stable test ids. */
  plugin: string
  /** Server-absolute `/plugin-api/*` path the host embeds in the iframe. */
  path: string
  onClose: () => void
}

/** Path prefix a panel iframe is allowed to reach through the data bridge. */
const PLUGIN_UI_PREFIX = '/api/plugin-ui/'
const ALLOWED_METHODS = new Set(['GET', 'POST', 'PUT', 'PATCH', 'DELETE'])

/**
 * Renders a plugin-contributed UI panel in a sandboxed `<iframe>` pointed
 * at the plugin's own `/plugin-api/*` page. Core does not render the page —
 * the plugin owns it end to end; this is just the generic host frame.
 *
 * Security model:
 * - The iframe is sandboxed WITHOUT `allow-same-origin`, so the
 *   plugin-authored page runs with an opaque origin and cannot reach the host
 *   app's `localStorage` (where the session JWT lives) or script the parent.
 * - To let the page show the user's own data it does NOT get the token.
 *   Instead it asks, over `postMessage`, for a `/api/plugin-ui/*` request; the
 *   parent performs the authenticated fetch on its behalf and posts the
 *   response back. The JWT never leaves the parent, and the bridge refuses any
 *   path outside the `/api/plugin-ui/*` surface (which core already serves via
 *   the plugin under the user's authority) and any non-standard method.
 *
 * Message protocol (both directions target the opaque-origin iframe with `*`;
 * the parent authenticates the *source* instead, by identity of the iframe's
 * `contentWindow`):
 *   page → parent: `{ type: 'plugin-ui-fetch', requestId, method, path, body? }`
 *   parent → page: `{ type: 'plugin-ui-fetch-result', requestId, status, body }`
 */
export default function PluginPanelModal({ title, plugin, path, onClose }: Props) {
  const frameRef = useRef<HTMLIFrameElement | null>(null)

  useEffect(() => {
    async function onMessage(e: MessageEvent) {
      // Only honor messages from THIS panel's iframe. The iframe has an opaque
      // origin (no allow-same-origin), so `e.origin` is "null" and useless for
      // trust — identity of the source window is the real check.
      const frame = frameRef.current
      if (!frame || e.source !== frame.contentWindow) return
      const msg = e.data
      if (!msg || msg.type !== 'plugin-ui-fetch' || typeof msg.requestId !== 'number') return

      const reply = (status: number, body: string) =>
        frame.contentWindow?.postMessage(
          { type: 'plugin-ui-fetch-result', requestId: msg.requestId, status, body },
          '*',
        )

      const method = typeof msg.method === 'string' ? msg.method.toUpperCase() : 'GET'
      const reqPath = typeof msg.path === 'string' ? msg.path : ''
      // Hard scope: only the plugin-UI surface, no traversal, allowed methods.
      if (!reqPath.startsWith(PLUGIN_UI_PREFIX) || reqPath.includes('..')) {
        reply(403, JSON.stringify({ error: 'path not allowed' }))
        return
      }
      if (!ALLOWED_METHODS.has(method)) {
        reply(405, JSON.stringify({ error: 'method not allowed' }))
        return
      }
      try {
        const res = await authedFetch(reqPath, {
          method,
          headers: msg.body ? { 'content-type': 'application/json' } : undefined,
          body: typeof msg.body === 'string' ? msg.body : undefined,
        })
        reply(res.status, await res.text())
      } catch (err) {
        reply(502, JSON.stringify({ error: String(err) }))
      }
    }
    window.addEventListener('message', onMessage)
    return () => window.removeEventListener('message', onMessage)
  }, [])

  return (
    <Modal
      onClose={onClose}
      className="plugin-panel-modal"
      maxWidth="min(960px, 96vw)"
      data-testid="plugin-panel-modal"
    >
      <h2 className="plugin-panel-title">{title}</h2>
      <iframe
        ref={frameRef}
        className="plugin-panel-frame"
        data-testid="plugin-panel-frame"
        data-plugin={plugin}
        title={title}
        src={path}
        sandbox="allow-scripts allow-forms allow-popups allow-downloads"
      />
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}
