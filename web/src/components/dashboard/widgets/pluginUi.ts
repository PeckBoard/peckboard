import { authedFetch } from '../../../store/auth'

/** Thrown for non-2xx; `notInstalled` when core's proxy has no such plugin
 *  route (404). */
export class PluginUiError extends Error {
  notInstalled: boolean
  constructor(message: string, notInstalled: boolean) {
    super(message)
    this.notInstalled = notInstalled
  }
}

/** JSON from a plugin's app-UI endpoint via core's authed
 *  `/api/plugin-ui/<plugin>` proxy. */
export async function pluginUiFetch<T>(
  plugin: string,
  path: string,
  init?: RequestInit,
): Promise<T> {
  const res = await authedFetch(`/api/plugin-ui/${plugin}${path}`, init)
  if (!res.ok) {
    const body = await res.json().catch(() => null)
    const msg = (body && typeof body.error === 'string' && body.error) || `HTTP ${res.status}`
    throw new PluginUiError(msg, res.status === 404)
  }
  return (await res.json()) as T
}

/** POST a JSON body to a plugin's app-UI endpoint. */
export function pluginUiPost<T>(plugin: string, path: string, body?: unknown): Promise<T> {
  return pluginUiFetch<T>(plugin, path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  })
}
