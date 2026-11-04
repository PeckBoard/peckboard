import { authedFetch } from '../store/auth'

/** Mint a fresh 10-minute signed URL for one shared file. */
export async function mintUrl(sessionId: string, fileId: string): Promise<string> {
  const res = await authedFetch(`/api/sessions/${sessionId}/shared-files/${fileId}/url`, {
    method: 'POST',
  })
  if (!res.ok)
    throw new Error(res.status === 404 ? 'File no longer available' : `HTTP ${res.status}`)
  const { url } = (await res.json()) as { url: string }
  return url
}
