import { authedFetch } from '../store/auth'

/** Error message for a shared file the server no longer stores (deleted by
 *  data retention, or with its session). Retrying cannot bring it back. */
export const FILE_GONE = 'File no longer available'

/** Mint a fresh 10-minute signed URL for one shared file. */
export async function mintUrl(sessionId: string, fileId: string): Promise<string> {
  const res = await authedFetch(`/api/sessions/${sessionId}/shared-files/${fileId}/url`, {
    method: 'POST',
  })
  if (!res.ok) throw new Error(res.status === 404 ? FILE_GONE : `HTTP ${res.status}`)
  const { url } = (await res.json()) as { url: string }
  return url
}
