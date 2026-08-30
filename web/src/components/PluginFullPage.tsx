import { useEffect, useRef } from 'react'
import { authedFetch } from '../store/auth'
import { withPluginTheme } from '../util/appearance'
import usePluginDataForward from '../hooks/usePluginDataForward'

interface Props {
  /** Human label for the page (shown in the header). */
  title: string
  /** Plugin that declared this item — scopes the plugin-data event forward
   *  to this page's own plugin (and stable test ids). */
  plugin: string
  /** Server-absolute `/plugin-api/*` path the host embeds in the iframe. */
  path: string
  /** Scope this page runs in — sent to the backend so the plugin's
   *  folder-scoped host functions act in the project's/session's folder, or
   *  in the folder itself for a Folders-page item. Most specific wins on the
   *  server: session, then project, then folder. */
  scope: { projectId?: string; sessionId?: string; folderId?: string }
  /** Query string (no leading `?`) to hand the page, overriding the app
   *  URL's own. Set when another plugin page deep-linked here, so the
   *  target reloads with the new query even though the route didn't change. */
  search?: string
  /** Return to the chat/board view. */
  onBack: () => void
}

/** Path prefix a plugin page's data bridge is allowed to reach. */
const PLUGIN_UI_PREFIX = '/api/plugin-ui/'
const ALLOWED_METHODS = new Set(['GET', 'POST', 'PUT', 'PATCH', 'DELETE'])

/**
 * Renders a plugin-contributed full-page view (from a manifest `project_items`
 * / `session_items` / `folder_items` entry) inside the project, session, or
 * Folders page. Same sandboxed
 * differences: it fills the view (not a modal), and it injects the active
 * project/session id as a request header so core can resolve the plugin's
 * folder scope (see `PluginManager::serve_http_authed`). The JWT still never
 * project/session/folder id as a request header so core can resolve the plugin's
 * `/api/plugin-ui/*` requests, which the parent performs on its behalf.
 */
export default function PluginFullPage({ title, plugin, path, scope, search, onBack }: Props) {
  const frameRef = useRef<HTMLIFrameElement | null>(null)
  // Forward the app URL's query string into the iframe so deep links (e.g.
  // the chat's replay links: /plugin-page/…?run=<id>) reach the plugin page,
  // which reads its own location.search. Plugins ignore params they don't
  // know, so forwarding everything is safe. A `search` prop wins: it carries
  // the query from a plugin-to-plugin deep link, which App put on the URL
  // too but which must also survive a same-route navigation.
  const query = search ?? window.location.search.replace(/^\?/, '')
  const src = withPluginTheme(query ? `${path}${path.includes('?') ? '&' : '?'}${query}` : path)
  // Keep the latest scope in a ref so the long-lived message listener always
  // injects the current id without being torn down on every scope change.
  const scopeRef = useRef(scope)
  useEffect(() => {
    scopeRef.current = scope
  }, [scope])

  // Push "your data changed" notifications into the sandboxed page so it
  // refreshes on change instead of polling.
  usePluginDataForward(frameRef, plugin)
  useEffect(() => {
    async function onMessage(e: MessageEvent) {
      const frame = frameRef.current
      if (!frame || e.source !== frame.contentWindow) return
      const msg = e.data
      if (!msg) return
      // A plugin page asking the host to open a session tab (the app-manager
      // install flow's "open the install session" link). Pure UI navigation
      // under the signed-in user's authority — the same event the MCP
      // install flow dispatches (web/src/utils/installSession.ts).
      if (
        msg.type === 'plugin-ui-open-session' &&
        typeof msg.sessionId === 'string' &&
        msg.sessionId.length > 0 &&
        msg.sessionId.length <= 256
      ) {
        window.dispatchEvent(
          new CustomEvent('peckboard:open-session', { detail: { session_id: msg.sessionId } }),
        )
        return
      }
      // A plugin page handing the user off to ANOTHER plugin's page, with an
      // optional query (graphify's "Install from App Manager" button). It has
      // to be an in-app navigation: these iframes are sandboxed without
      // `allow-same-origin`, a `target=_blank` tab INHERITS that sandbox, and
      // the resulting opaque origin makes the app's own asset requests carry
      // `Origin: null` — which `origin_check` (src/security.rs) answers with a
      // 403, so the popup renders blank.
      //
      // Scoped to plugin pages on purpose: this navigates the user's app, so
      // the narrowest useful power is a page another plugin already contributes
      // (App resolves it against the sidebar-item catalog and shows its own
      // "no longer available" state when there's no such item).
      if (msg.type === 'plugin-ui-open-page') {
        const slug = /^[a-z0-9_-]{1,64}$/
        const query = typeof msg.query === 'string' ? msg.query : ''
        if (
          typeof msg.plugin !== 'string' ||
          typeof msg.item !== 'string' ||
          !slug.test(msg.plugin) ||
          !slug.test(msg.item) ||
          query.length > 512 ||
          // Query-string charset only: no whitespace, no control characters,
          // nothing that could break out of the URL it is spliced into.
          !/^[A-Za-z0-9._~%!$&'()*+,;=:@/?-]*$/.test(query)
        ) {
          return
        }
        window.dispatchEvent(
          new CustomEvent('peckboard:open-plugin-page', {
            detail: { plugin: msg.plugin, item: msg.item, query },
          }),
        )
        return
      }
      if (msg.type !== 'plugin-ui-fetch' || typeof msg.requestId !== 'number') return
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
      const { projectId, sessionId, folderId } = scopeRef.current
      if (sessionId) headers['x-peckboard-session-id'] = sessionId
      if (projectId) headers['x-peckboard-project-id'] = projectId
      if (folderId) headers['x-peckboard-folder-id'] = folderId

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
        src={src}
        sandbox="allow-scripts allow-forms allow-popups allow-downloads"
      />
    </div>
  )
}
