import { authedFetch } from '../store/auth'

/** file | report | plan — selects the backend source adapter. */
export type ReviewSourceKind = 'file' | 'report' | 'plan'

/** annotating | running | needs_input | approved */
export type ReviewStatus = 'annotating' | 'running' | 'needs_input' | 'approved'

/** The six annotation kinds the doc pane's selection popover produces.
 *  `clarify` is not one of them — clarify sends a message instead. */
export type ReviewCommentKind = 'comment' | 'suggest' | 'wrong' | 'expand' | 'shorten'

/** pending | sent → open; fixed | declined | answered → resolved. */
export type ReviewCommentStatus = 'pending' | 'sent' | 'fixed' | 'declined' | 'answered'

export interface DocReview {
  id: string
  title: string
  source_kind: ReviewSourceKind
  /** file → `<folder_id>:<relative/path.md>`, report → `<YYYY-MM-DD>/<file.md>`,
   *  plan → `<plan_id>`. */
  source_ref: string
  folder_id: string | null
  project_id: string | null
  /** The review AI session; null until the first pass creates it. */
  session_id: string | null
  status: ReviewStatus
  current_version: number
  created_at: string
  updated_at: string
}

export interface DocReviewComment {
  id: string
  review_id: string
  version: number
  start_line: number
  end_line: number
  quote: string | null
  kind: ReviewCommentKind
  body: string
  status: ReviewCommentStatus
  resolution_note: string | null
  created_at: string
}

/** History-list entry — no markdown body (the whole document per row would
 *  ship the doc N times just to render a list). */
export interface DocReviewVersionMeta {
  review_id: string
  version: number
  note: string
  created_by: 'user' | 'assistant'
  created_at: string
}

export interface DocReviewVersion extends DocReviewVersionMeta {
  markdown: string
}

/** GET /api/doc-reviews/{id} — one request holds everything the review
 *  screen renders. */
export interface ReviewDetail {
  review: DocReview
  markdown: string
  comments: DocReviewComment[]
}

/** One `.md` file under a folder, as listed by the jailed walker. */
/** One `.md` file under a folder, as listed by the jailed walker. */
export interface MarkdownFileEntry {
  path: string
  size: number
}

/** A non-2xx response, carrying the server's `{"error"}` message and the
 *  status. The status matters to the review screen: a 404 means the review
 *  was deleted (from another tab, or by the list view), and the screen bails
 *  back to the list rather than parking on an error nobody can clear. */
export class ReviewRequestError extends Error {
  readonly status: number

  constructor(message: string, status: number) {
    super(message)
    this.name = 'ReviewRequestError'
    this.status = status
  }
}

/** Turn a non-2xx response into an Error carrying the server's `{"error"}`
 *  message, so callers can hand it straight to `describeActionError`. */
async function fail(res: Response, fallback: string): Promise<never> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  throw new ReviewRequestError(body?.error || `${fallback} (HTTP ${res.status})`, res.status)
}

async function json<T>(res: Response, fallback: string): Promise<T> {
  if (!res.ok) await fail(res, fallback)
  return (await res.json()) as T
}

// ─── Reviews ───

export async function listReviews(): Promise<DocReview[]> {
  const res = await authedFetch('/api/doc-reviews')
  const data = await json<{ reviews: DocReview[] }>(res, "Couldn't load reviews")
  return data.reviews
}

export interface CreateReviewInput {
  source_kind: ReviewSourceKind
  source_ref: string
  /** Overrides the title the backend resolves from the document. */
  title?: string
  project_id?: string
  folder_id?: string
}

export async function createReview(input: CreateReviewInput): Promise<ReviewDetail> {
  const res = await authedFetch('/api/doc-reviews', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(input),
  })
  return json<ReviewDetail>(res, "Couldn't create the review")
}

/** `comments: 'all'` also returns the resolved annotations, which the review
 *  screen's rail groups under "Resolved" with the note the assistant left.
 *  The default stays open-only. */
export async function getReview(
  id: string,
  opts: { comments?: 'open' | 'all' } = {},
): Promise<ReviewDetail> {
  const query = opts.comments === 'all' ? '?comments=all' : ''
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}${query}`)
  return json<ReviewDetail>(res, "Couldn't load this review")
}

export async function deleteReview(id: string): Promise<void> {
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}`, { method: 'DELETE' })
  if (!res.ok) await fail(res, "Couldn't delete the review")
}

// ─── Versions ───

export async function listVersions(id: string): Promise<DocReviewVersionMeta[]> {
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}/versions`)
  const data = await json<{ versions: DocReviewVersionMeta[] }>(res, "Couldn't load the history")
  return data.versions
}

export async function getVersion(id: string, n: number): Promise<DocReviewVersion> {
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}/versions/${n}`)
  const data = await json<{ version: DocReviewVersion }>(res, "Couldn't load that version")
  return data.version
}

// ─── Annotations ───

export interface AddCommentInput {
  start_line: number
  end_line?: number
  quote?: string | null
  kind: ReviewCommentKind
  body: string
}

export async function addComment(id: string, input: AddCommentInput): Promise<DocReviewComment> {
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}/comments`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(input),
  })
  const data = await json<{ comment: DocReviewComment }>(res, "Couldn't save the annotation")
  return data.comment
}

export interface UpdateCommentInput {
  body?: string
  kind?: ReviewCommentKind
  status?: ReviewCommentStatus
  resolution_note?: string
}

export async function updateComment(
  id: string,
  commentId: string,
  input: UpdateCommentInput,
): Promise<DocReviewComment> {
  const res = await authedFetch(
    `/api/doc-reviews/${encodeURIComponent(id)}/comments/${encodeURIComponent(commentId)}`,
    {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    },
  )
  const data = await json<{ comment: DocReviewComment }>(res, "Couldn't update the annotation")
  return data.comment
}

export async function deleteComment(id: string, commentId: string): Promise<void> {
  const res = await authedFetch(
    `/api/doc-reviews/${encodeURIComponent(id)}/comments/${encodeURIComponent(commentId)}`,
    { method: 'DELETE' },
  )
  if (!res.ok) await fail(res, "Couldn't delete the annotation")
}

// ─── Passes and the source document ───

export interface RunPassInput {
  /** Free-form ask. Required when there are no pending annotations. */
  message?: string
  /** Defaults to true server-side. The chat lane and Clarify pass false so
   *  queued annotations stay pending for a deliberate pass. */
  include_annotations?: boolean
}

export async function runPass(id: string, input: RunPassInput = {}): Promise<void> {
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}/pass`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(input),
  })
  if (!res.ok) await fail(res, "Couldn't start the pass")
}

/** Write the current version back to the source document. `finish` also
 *  marks the review approved. */
export async function applyReview(id: string, finish = false): Promise<void> {
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}/apply`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ finish }),
  })
  if (!res.ok) await fail(res, "Couldn't write the document back to its source")
}

/** Copy version `n` to a NEW head version — history is append-only, so
 *  nothing between `n` and the old head is lost. */
export async function revertToVersion(id: string, n: number): Promise<void> {
  const res = await authedFetch(`/api/doc-reviews/${encodeURIComponent(id)}/revert/${n}`, {
    method: 'POST',
  })
  if (!res.ok) await fail(res, "Couldn't revert to that version")
}

// ─── Folder markdown (the `file` source kind) ───

export async function listMarkdownFiles(folderId: string): Promise<MarkdownFileEntry[]> {
  const res = await authedFetch(`/api/folders/${encodeURIComponent(folderId)}/markdown-files`)
  const data = await json<{ files: MarkdownFileEntry[]; truncated: boolean }>(
    res,
    "Couldn't list the markdown files in this folder",
  )
  return data.files
}

export async function readMarkdownFile(folderId: string, path: string): Promise<string> {
  const res = await authedFetch(
    `/api/folders/${encodeURIComponent(folderId)}/markdown-file?path=${encodeURIComponent(path)}`,
  )
  const data = await json<{ path: string; markdown: string }>(res, "Couldn't read that file")
  return data.markdown
}

/** Compose the `source_ref` for a `file` review. */
export function fileSourceRef(folderId: string, path: string): string {
  return `${folderId}:${path}`
}

/** Navigate to the full-page review screen at /review/<id>. Uses pushState +
 *  a synthetic popstate so App's router picks it up without prop threading —
 *  same shape as lib/plan.ts::openPlan. */
export function openReview(reviewId: string) {
  window.history.pushState(null, '', `/review/${reviewId}`)
  window.dispatchEvent(new PopStateEvent('popstate'))
}

/** Human-facing label for a status chip. */
export const REVIEW_STATUS_LABEL: Record<ReviewStatus, string> = {
  annotating: 'annotating',
  running: 'running',
  needs_input: 'needs input',
  approved: 'approved',
}

/** Human-facing label for a source-kind badge. */
export const REVIEW_SOURCE_LABEL: Record<ReviewSourceKind, string> = {
  file: 'file',
  report: 'report',
  plan: 'plan',
}

/** Human-facing label for an annotation kind, as the rail and the selection
 *  popover both name it. Same word in both places or the rail reads as a
 *  different feature from the popover that filled it. */
export const REVIEW_COMMENT_KIND_LABEL: Record<ReviewCommentKind, string> = {
  comment: 'Comment',
  suggest: 'Suggested edit',
  wrong: 'Marked wrong',
  expand: 'Expand',
  shorten: 'Shorten',
}

/** How the assistant closed an annotation out. */
export const REVIEW_RESOLUTION_LABEL: Record<'fixed' | 'declined' | 'answered', string> = {
  fixed: 'fixed',
  declined: 'declined',
  answered: 'answered',
}

/** `pending` and `sent` are both still awaiting the assistant: a `sent`
 *  annotation is riding along on the pass that's running right now. */
export function isOpenComment(c: DocReviewComment): boolean {
  return c.status === 'pending' || c.status === 'sent'
}

/** Where `apply` would write. Shown in the confirmation so nobody overwrites
 *  a file they only meant to read. */
export function describeReviewSource(review: DocReview): string {
  if (review.source_kind === 'file') {
    // `<folder_id>:<relative/path.md>` — the folder id is noise to a human.
    const sep = review.source_ref.indexOf(':')
    return sep >= 0 ? review.source_ref.slice(sep + 1) : review.source_ref
  }
  if (review.source_kind === 'report') return `report ${review.source_ref}`
  return 'the plan it was created from'
}
