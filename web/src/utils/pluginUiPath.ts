/**
 * Validate a path a sandboxed plugin page asks the parent to fetch on its
 * behalf (the `plugin-ui-fetch` postMessage bridge). The parent attaches the
 * user's bearer token, so the request must stay inside the plugin's OWN
 * `/api/plugin-ui/<id>/` surface: a literal `..` check is not enough, since
 * the URL parser also folds `%2e%2e` into a dot-dot segment.
 *
 * Returns the normalised `pathname + search` to fetch, or `null` to reject.
 */
export function resolvePluginUiPath(plugin: string, raw: unknown): string | null {
  if (typeof raw !== 'string' || !raw.startsWith('/') || raw.startsWith('//')) return null
  const pathPart = raw.split(/[?#]/, 1)[0]
  // Encoded dots / slashes / backslashes in the path have no legitimate use
  // here and are how traversal sneaks past prefix checks.
  if (pathPart.includes('\\') || /%(2e|2f|5c)/i.test(pathPart)) return null
  let url: URL
  try {
    url = new URL(raw, window.location.origin)
  } catch {
    return null
  }
  if (url.origin !== window.location.origin) return null
  const prefix = `/api/plugin-ui/${encodeURIComponent(plugin)}/`
  if (!url.pathname.startsWith(prefix)) return null
  return url.pathname + url.search
}
