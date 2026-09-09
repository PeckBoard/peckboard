import { authedFetch } from '../store/auth'
import { useFoldersStore } from '../store/folders'

/**
 * Kick off a temporary "install this binary" session for a missing host
 * command (stdio MCP server, Codex CLI, …):
 *
 *  1. register (or reuse) a working folder at the server-suggested path
 *     (`~/peckboard-installs/<command>`), creating the directory on disk;
 *  2. create a temp session there (auto-deleted when its tab closes);
 *  3. send the install prompt as the first message — including the
 *     `sudo -A` rule so root steps raise the askpass password dialog;
 *  4. hand the session id to App via `peckboard:open-session` so the tab
 *     opens and the user can watch/approve the install.
 *
 * Returns the new session id.
 */
export async function startInstallSession(opts: {
  command: string
  /** Noun phrase after "required by", e.g. `the MCP server "github"` or `the Codex provider`. */
  requiredBy: string
  steps: string[]
  suggestedFolderPath: string
  /** Completes "When done, tell me to …". */
  doneHint: string
}): Promise<string> {
  const { command, requiredBy, steps, suggestedFolderPath, doneHint } = opts

  const foldersStore = useFoldersStore.getState()
  await foldersStore.fetchFolders()
  let folder = useFoldersStore.getState().folders.find((f) => f.path === suggestedFolderPath)
  if (!folder) {
    folder = await foldersStore.createFolder(`Install ${command}`, suggestedFolderPath, true)
  }

  const res = await authedFetch('/api/sessions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      name: `Install ${command}`,
      folder_id: folder.id,
      is_temp: true,
    }),
  })
  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: 'Failed to create session' }))
    throw new Error(err.error || 'Failed to create session')
  }
  const session: { id: string } = await res.json()

  const msg = await authedFetch(`/api/sessions/${session.id}/message`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ text: buildInstallPrompt(command, requiredBy, steps, doneHint) }),
  })
  if (!msg.ok) {
    const err = await msg.json().catch(() => ({ error: 'Failed to send install prompt' }))
    throw new Error(err.error || 'Failed to send install prompt')
  }

  window.dispatchEvent(
    new CustomEvent('peckboard:open-session', { detail: { session_id: session.id } }),
  )
  return session.id
}

function buildInstallPrompt(
  command: string,
  requiredBy: string,
  steps: string[],
  doneHint: string,
): string {
  const stepsBlock =
    steps.length > 0
      ? `Known install steps (verify they fit this machine before running):\n${steps
          .map((s) => `- ${s}`)
          .join('\n')}\n\n`
      : ''
  return (
    `Install the \`${command}\` binary required by ${requiredBy} so it is available on PATH for the Peckboard server.\n\n` +
    stepsBlock +
    `Rules:\n` +
    `- Prefer a user-level install that needs no root when one exists.\n` +
    `- If a step needs root, run it as \`sudo -A <cmd>\`. The \`-A\` flag routes sudo's password prompt to a masked dialog in the Peckboard UI. Plain \`sudo\` will fail here (no TTY). Never put the password on a command line and never echo it.\n` +
    `- Finish by verifying: run \`${command} --version\` (or the closest equivalent) and report the installed version.\n` +
    `- When done, tell me to ${doneHint}.`
  )
}
