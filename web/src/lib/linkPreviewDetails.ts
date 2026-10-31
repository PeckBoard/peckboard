/** Forge `details.type`s (GitHub-shaped: GitHub, GitLab, Gitea, Bitbucket). */
export const FORGE_TYPES = new Set(['repo', 'pull', 'issue', 'commit', 'run'])

/** Types whose card uses the compact row layout (avatar thumb, no hero). */
const COMPACT_TYPES = new Set([
  ...FORGE_TYPES,
  'gist',
  'package',
  'ticket',
  'question',
  'hn',
  'model',
  'dataset',
  'app',
])

export function isCompactDetails(d: Record<string, unknown> | undefined): boolean {
  return !!d && COMPACT_TYPES.has(String(d.type))
}
