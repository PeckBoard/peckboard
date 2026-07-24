import { useEffect, useRef } from 'react'
import { authedFetch } from '../store/auth'

interface Props {
  /** Human label for the page (shown in the header). */
  title: string
  /** Plugin that declared this item — only used for stable test ids. */
  plugin: string
  /** Server-absolute `/plugin-api/*` path the host embeds in the iframe. */
  path: string
  /** Scope this page runs in — sent to the backend so the plugin's
   *  folder-scoped host functions act in the project's/session's folder. */
  scope: { projectId?: string; sessionId?: string }
  /** Return to the chat/board view. */
  onBack: () => void
}

/** Path prefix a plugin page's data bridge is allowed to reach. */
const PLUGIN_UI_PREFIX = '/api/plugin-ui/'
const ALLOWED_METHODS = new Set(['GET', 'POST', 'PUT', 'PATCH', 'DELETE'])

/**
 * Renders a plugin-contributed full-page view (from a manifest `project_items`
 * / `session_items` entry) inside the project or session page. Same sandboxed
 * `<iframe>` + parent-proxied fetch bridge as {@link PluginPanelModal}, with two
 * differences: it fills the view (not a modal), and it injects the active
 * project/session id as a request header so core can resolve the plugin's
 * folder scope (see `PluginManager::serve_http_authed`). The JWT still never
 * leaves the parent; the iframe runs at an opaque origin and can only ask for
 * `/api/plugin-ui/*` requests, which the parent performs on its behalf.
 */
export default function PluginFullPage({ title, plugin, path, scope, onBack }: Props) {
  const frameRef = useRef<HTMLIFrameElement | null>(null)
  // Keep the latest scope in a ref so the long-lived message listener always
  // injects the current id without being torn down on every scope change.
  const scopeRef = useRef(scope)
  useEffect(() => {
    scopeRef.current = scope
  }, [scope])

  useEffect(() => {
    async function onMessage(e: MessageEvent) {
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
      if (!reqPath.startsWith(PLUGIN_UI_PREFIX) || reqPath.includes('..')) {
        reply(403, JSON.stringify({ error: 'path not allowed' }))
        return
      }
      if (!ALLOWED_METHODS.has(method)) {
        reply(405, JSON.stringify({ error: 'method not allowed' }))
        return
      }

      // Inject the scope as a header so the backend can resolve the plugin's
      // folder. These are scope selectors only — the authed surface already
      // runs under the user's full authority.
      const headers: Record<string, string> = {}
      if (msg.body) headers['content-type'] = 'application/json'
      const { projectId, sessionId } = scopeRef.current
      if (sessionId) headers['x-peckboard-session-id'] = sessionId
      if (projectId) headers['x-peckboard-project-id'] = projectId

      try {
        const res = await authedFetch(reqPath, {
          method,
          headers,
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
    <div className="plugin-fullpage" data-testid="plugin-fullpage" data-plugin={plugin}>
      <div className="plugin-fullpage-header">
        <button type="button" className="btn-secondary" onClick={onBack}>
          ← Back
        </button>
        <h2 className="plugin-fullpage-title">{title}</h2>
      </div>
      <iframe
        ref={frameRef}
        className="plugin-fullpage-frame"
        data-testid="plugin-fullpage-frame"
        data-plugin={plugin}
        title={title}
        src={path}
        sandbox="allow-scripts allow-forms allow-popups allow-downloads"
      />
    </div>
  )
}
