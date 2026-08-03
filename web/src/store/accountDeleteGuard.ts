/** Shared with the claude/grok/kimi account stores: what a 409 from
 *  `DELETE /api/*-accounts/:id` reports about references still pinned to
 *  the account, so the section can offer a force-delete retry. */
export interface AccountDeleteRefs {
  sessions: string[]
  cards: number
  projects: number
  repeating_tasks: number
  queued_messages: number
  default_model: boolean
}

export class AccountDeleteConflict extends Error {
  refs: AccountDeleteRefs

  constructor(message: string, refs: AccountDeleteRefs) {
    super(message)
    this.name = 'AccountDeleteConflict'
    this.refs = refs
  }
}

/** Human-readable summary of what's still pinned to an account, for the
 *  force-delete confirmation dialog. */
export function describeAccountRefs(accountName: string, refs: AccountDeleteRefs): string {
  const parts: string[] = []
  if (refs.sessions.length > 0) {
    parts.push(`${refs.sessions.length} session${refs.sessions.length === 1 ? '' : 's'}`)
  }
  if (refs.cards > 0) parts.push(`${refs.cards} card${refs.cards === 1 ? '' : 's'}`)
  if (refs.projects > 0) parts.push(`${refs.projects} project${refs.projects === 1 ? '' : 's'}`)
  if (refs.repeating_tasks > 0) {
    parts.push(`${refs.repeating_tasks} repeating task${refs.repeating_tasks === 1 ? '' : 's'}`)
  }
  if (refs.queued_messages > 0) {
    parts.push(`${refs.queued_messages} queued message${refs.queued_messages === 1 ? '' : 's'}`)
  }
  if (refs.default_model) parts.push('the app-wide default model')
  const list = parts.length > 0 ? parts.join(', ') : 'other references'
  return (
    `"${accountName}" is still pinned by ${list}. Deleting anyway rewrites each reference to its ` +
    'bare model (falling back to the Default login) and removes the account. A session currently ' +
    "running under this account keeps its files until it finishes; they won't be cleaned up."
  )
}
