import { useEffect, useMemo, useState, type ReactNode } from 'react'

import Modal from '../Modal'
import SafeMarkdown from '../SafeMarkdown'
import FieldError from '../FieldError'
import { MenuButton, type MenuItem } from '../Dropdown'
import { authedFetch } from '../../store/auth'
import { useFoldersStore } from '../../store/folders'
import { describeActionError } from '../../utils/actionError'
import {
  createReview,
  fileSourceRef,
  listMarkdownFiles,
  listRepos,
  readMarkdownFile,
  type DocReview,
  type RepoEntry,
  type RepoWorktree,
  type ReviewSourceKind,
} from '../../lib/review'
import './Review.css'

interface Props {
  onClose: () => void
  onCreated: (review: DocReview) => void
}

/** One pickable document, normalised across the three source kinds. */
interface Candidate {
  /** `source_ref` sent to POST /api/doc-reviews. */
  ref: string
  /** Primary line in the picker and the default review title. */
  label: string
  /** Muted second line — the path / date / status that disambiguates. */
  detail: string
  /** Plans already carry their markdown, so their preview needs no fetch. */
  markdown?: string
}

const SOURCE_KINDS: {
  kind: ReviewSourceKind
  label: string
  blurb: string
  icon: ReactNode
}[] = [
  {
    kind: 'file',
    label: 'File',
    blurb: 'A markdown file in a folder, one of its repos, or a worktree.',
    icon: <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />,
  },
  {
    kind: 'report',
    label: 'Report',
    blurb: 'A report an agent wrote for you.',
    icon: (
      <>
        <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
        <polyline points="14 2 14 8 20 8" />
        <line x1="16" y1="13" x2="8" y2="13" />
        <line x1="16" y1="17" x2="8" y2="17" />
      </>
    ),
  },
  {
    kind: 'plan',
    label: 'Plan',
    blurb: 'A plan a session proposed.',
    icon: (
      <>
        <polyline points="9 11 12 14 20 6" />
        <path d="M20 12v7a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h9" />
      </>
    ),
  },
]

const PICKER_LABEL: Record<ReviewSourceKind, string> = {
  file: 'Document',
  report: 'Report',
  plan: 'Plan',
}

interface ReportRow {
  folder: string
  file: string
  title: string
  date: string
}

interface PlanRow {
  id: string
  title: string
  status: string
  version: number
  markdown: string
}

/** The repo level's opt-out: no repo picked means the whole folder tree,
 *  repos included — which is what a folder with no repos in it needs too. */
const BROWSE_FOLDER_LABEL = 'Browse the whole folder'

/** Fetch the pickable set for one (kind, scope) pair. Module-level so the
 *  effect that calls it stays a plain promise chain. A `repo` + `worktree`
 *  pair narrows the `file` kind to that one checkout; without them the walk
 *  covers the whole folder. */
async function loadCandidates(
  kind: ReviewSourceKind,
  folderId: string,
  repo: RepoEntry | null,
  worktree: RepoWorktree | null,
): Promise<Candidate[]> {
  if (kind === 'file') {
    if (repo && worktree) {
      const prefix = worktree.path ? `${worktree.path}/` : ''
      const files = await listMarkdownFiles(repo.folder_id, worktree.path || undefined)
      return files
        .slice()
        .sort((a, b) => a.path.localeCompare(b.path, undefined, { sensitivity: 'base' }))
        .map((f) => ({
          ref: fileSourceRef(repo.folder_id, f.path),
          label: f.path.split('/').pop() ?? f.path,
          // The prefix is the same on every row — show the path inside the
          // worktree, keep the full one in the ref.
          detail: prefix && f.path.startsWith(prefix) ? f.path.slice(prefix.length) : f.path,
        }))
    }
    if (!folderId) return []
    const files = await listMarkdownFiles(folderId)
    // The walk returns whatever order the filesystem hands back, which reads
    // as random in the picker. Sort by path so the list looks like the tree.
    return files
      .slice()
      .sort((a, b) => a.path.localeCompare(b.path, undefined, { sensitivity: 'base' }))
      .map((f) => ({
        ref: fileSourceRef(folderId, f.path),
        label: f.path.split('/').pop() ?? f.path,
        detail: f.path,
      }))
  }
  if (kind === 'report') {
    const res = await authedFetch('/api/reports')
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const data = (await res.json()) as { reports: ReportRow[] }
    return data.reports.map((r) => ({
      ref: `${r.folder}/${r.file}`,
      label: r.title || r.file,
      detail: `${r.folder} · ${r.file}`,
    }))
  }
  const res = await authedFetch('/api/plans')
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  const data = (await res.json()) as { plans: PlanRow[] }
  return data.plans.map((p) => ({
    ref: p.id,
    label: p.title,
    detail: `${p.status} · v${p.version}`,
    markdown: p.markdown,
  }))
}

/** Read the document behind a pick, for the preview pane. Only called for
 *  kinds whose candidates don't already carry their markdown. */
async function loadPreview(kind: ReviewSourceKind, ref: string): Promise<string> {
  if (kind === 'file') {
    // `<folder_id>:<relative/path.md>` — the path may itself contain ':'.
    const [folderId, ...rest] = ref.split(':')
    return readMarkdownFile(folderId, rest.join(':'))
  }
  const [folder, file] = ref.split('/')
  const res = await authedFetch(
    `/api/reports/${encodeURIComponent(folder)}/${encodeURIComponent(file)}`,
  )
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return ((await res.json()) as { body: string }).body
}

/**
 * Two-step creation flow for a document review: pick where the document
 * comes from, then pick the document itself and confirm its title.
 *
 * Every pickable set is a searchable combobox rather than a text field — a
 * markdown tree, the report archive, and the plan list are all known option
 * sets, and typing a path by hand is how you end up with a review pointed at
 * a document that doesn't exist. The only free-form input is the title,
 * which is genuinely the user's own words.
 */
export default function NewReviewWizard({ onClose, onCreated }: Props) {
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)

  const [step, setStep] = useState<1 | 2>(1)
  const [kind, setKind] = useState<ReviewSourceKind>('file')
  const [chosenFolderId, setChosenFolderId] = useState('')
  // `null` while the option set is in flight — the picker shows "Loading…".
  const [candidates, setCandidates] = useState<Candidate[] | null>(null)
  const [candidatesError, setCandidatesError] = useState<string | null>(null)
  const [picked, setPicked] = useState<Candidate | null>(null)
  const [title, setTitle] = useState('')
  const [preview, setPreview] = useState<string | null>(null)
  const [previewError, setPreviewError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // The repos in the chosen folder — `null` while the list is in flight.
  // A null `pickedRepo` means "browse the whole folder"; a picked one always
  // carries a picked worktree, so no level of the cascade dead-ends.
  const [repos, setRepos] = useState<RepoEntry[] | null>(null)
  const [reposError, setReposError] = useState<string | null>(null)
  const [pickedRepo, setPickedRepo] = useState<RepoEntry | null>(null)
  const [pickedWorktree, setPickedWorktree] = useState<RepoWorktree | null>(null)

  useEffect(() => {
    void fetchFolders()
  }, [fetchFolders])

  // Derived rather than stored: the `file` kind needs *a* folder the moment
  // step 2 opens, and the first one is as good a default as any.
  const folderId = chosenFolderId || folders[0]?.id || ''
  const pickerLabel = PICKER_LABEL[kind]

  /** Clear everything downstream of the option set — called whenever the
   *  set itself changes, so a stale pick can't survive into a new list. */
  const resetPick = () => {
    setCandidates(null)
    setCandidatesError(null)
    setPicked(null)
    setPreview(null)
    setPreviewError(null)
  }

  // The repos under the chosen folder. Scoped to that folder rather than
  // scanning every folder's tree: the cascade picks the folder first, so
  // the rest is work nobody asked for. A folder change nulls `repos`, which
  // is what re-runs this.
  useEffect(() => {
    if (step !== 2 || kind !== 'file' || !folderId || repos !== null) return
    let cancelled = false
    void listRepos(folderId)
      .then((rs) => {
        if (cancelled) return
        setRepos(rs)
        setReposError(null)
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setRepos([])
        setReposError(describeActionError(e, "Couldn't list the repos."))
      })
    return () => {
      cancelled = true
    }
  }, [step, kind, folderId, repos])

  // Load the option set for the current (kind, folder, repo worktree) scope.
  useEffect(() => {
    if (step !== 2) return
    let cancelled = false
    void loadCandidates(kind, folderId, pickedRepo, pickedWorktree)
      .then((cs) => {
        if (cancelled) return
        setCandidates(cs)
        setCandidatesError(null)
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setCandidates([])
        setCandidatesError(describeActionError(e, "Couldn't load the documents to choose from."))
      })
    return () => {
      cancelled = true
    }
  }, [step, kind, folderId, pickedRepo, pickedWorktree])

  // Read the picked document for the preview pane. Plans arrive with their
  // markdown already attached, so they never reach this.
  useEffect(() => {
    if (!picked || picked.markdown !== undefined) return
    let cancelled = false
    void loadPreview(kind, picked.ref)
      .then((markdown) => {
        if (cancelled) return
        setPreview(markdown)
        setPreviewError(null)
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setPreview(null)
        setPreviewError(describeActionError(e, "Couldn't read that document."))
      })
    return () => {
      cancelled = true
    }
  }, [picked, kind])

  const pickerItems: MenuItem[] = useMemo(
    () =>
      (candidates ?? []).map((c) => ({
        label: c.label,
        description: c.detail,
        searchText: `${c.detail} ${c.ref}`,
        active: picked?.ref === c.ref,
        onSelect: () => {
          setPicked(c)
          setTitle(c.label.replace(/\.md$/i, ''))
          // Plans carry their body; everything else waits on the effect.
          setPreview(c.markdown ?? null)
          setPreviewError(null)
        },
      })),
    [candidates, picked],
  )

  const repoItems: MenuItem[] = useMemo(() => {
    const pick = (r: RepoEntry | null) => {
      setPickedRepo(r)
      // A repo brings its own worktrees, and its main checkout is the one
      // obvious answer — start there so the level never stalls the cascade.
      setPickedWorktree(r ? (r.worktrees[0] ?? null) : null)
      // A new scope means a new option set.
      setCandidates(null)
      setCandidatesError(null)
      setPicked(null)
      setPreview(null)
      setPreviewError(null)
    }
    return [
      {
        label: BROWSE_FOLDER_LABEL,
        description: 'Every markdown file in the folder, repos included',
        searchText: 'folder browse whole all',
        active: pickedRepo === null,
        onSelect: () => {
          if (pickedRepo === null) return
          pick(null)
        },
      },
      ...(repos ?? []).map((r) => ({
        label: r.name,
        description: r.path || 'Folder root',
        searchText: `${r.name} ${r.path}`,
        active: pickedRepo?.path === r.path,
        onSelect: () => {
          if (pickedRepo?.path === r.path) return
          pick(r)
        },
      })),
    ]
  }, [repos, pickedRepo])

  const worktreeItems: MenuItem[] = useMemo(
    () =>
      (pickedRepo?.worktrees ?? []).map((w) => ({
        label: w.card_title ?? w.branch,
        description: w.main ? 'Main checkout' : w.path,
        searchText: `${w.branch} ${w.path} ${w.card_title ?? ''}`,
        active: pickedWorktree?.path === w.path,
        onSelect: () => {
          if (pickedWorktree?.path === w.path) return
          setPickedWorktree(w)
          // Same reset as a folder change — the option set is new.
          setCandidates(null)
          setCandidatesError(null)
          setPicked(null)
          setPreview(null)
          setPreviewError(null)
        },
      })),
    [pickedRepo, pickedWorktree],
  )

  // Why Create is disabled, stated next to the button — a disabled control
  // with no reason reads as broken.
  const disabledReason =
    kind === 'file' && !folderId
      ? 'Add a folder first'
      : !picked
        ? `Pick a ${pickerLabel.toLowerCase()}`
        : !title.trim()
          ? 'Give the review a title'
          : null

  const submit = async () => {
    if (!picked || disabledReason) return
    setSubmitting(true)
    setError(null)
    try {
      const detail = await createReview({
        source_kind: kind,
        source_ref: picked.ref,
        title: title.trim(),
        ...(kind === 'file'
          ? {
              folder_id: folderId,
            }
          : {}),
      })
      onCreated(detail.review)
    } catch (e) {
      setError(describeActionError(e, "Couldn't create the review. Please try again."))
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={760} data-testid="review-wizard" className="review-wizard">
      <h2>New Review</h2>

      {step === 1 ? (
        <>
          <p className="form-hint review-wizard__lead">
            What are we reviewing? You can annotate it, run as many AI passes as you like, and
            nothing is written back to the document until you apply.
          </p>
          <div
            className="review-wizard__kinds"
            data-testid="review-wizard-source-kind"
            role="radiogroup"
            aria-label="Source kind"
          >
            {SOURCE_KINDS.map((s) => (
              <button
                key={s.kind}
                type="button"
                role="radio"
                aria-checked={kind === s.kind}
                data-testid={`review-wizard-kind-${s.kind}`}
                className={`review-wizard__kind${
                  kind === s.kind ? ' review-wizard__kind--active' : ''
                }`}
                onClick={() => {
                  setKind(s.kind)
                  resetPick()
                }}
              >
                <svg
                  width="22"
                  height="22"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  aria-hidden="true"
                >
                  {s.icon}
                </svg>
                <span className="review-wizard__kind-label">{s.label}</span>
                <span className="review-wizard__kind-blurb">{s.blurb}</span>
              </button>
            ))}
          </div>
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose}>
              Cancel
            </button>
            <button
              type="button"
              className="btn-primary"
              data-testid="review-wizard-next"
              onClick={() => setStep(2)}
            >
              Next
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="review-wizard__body">
            <div className="review-wizard__form">
              {kind === 'file' && (
                <>
                  {/* Folder → repo → worktree → file. Each level narrows the
                      next, and picking one resets everything under it — a
                      stale pick from the previous scope is a review pointed
                      at a document that isn't there. */}
                  <div className="form-field">
                    <label className="form-label" htmlFor="review-wizard-folder">
                      Folder
                    </label>
                    {folders.length > 0 ? (
                      <select
                        id="review-wizard-folder"
                        className="form-input"
                        data-testid="review-wizard-folder"
                        value={folderId}
                        onChange={(e) => {
                          // Re-picking the folder that is already selected
                          // must not reset the option set: the loader effect
                          // keys on `folderId`, so with nothing to re-run the
                          // picker would sit on "Loading…" forever.
                          if (e.target.value === folderId) return
                          setChosenFolderId(e.target.value)
                          // Null repos is what re-runs the repo scan.
                          setRepos(null)
                          setReposError(null)
                          setPickedRepo(null)
                          setPickedWorktree(null)
                          resetPick()
                        }}
                      >
                        {folders.map((f) => (
                          <option key={f.id} value={f.id}>
                            {f.name} — {f.path}
                          </option>
                        ))}
                      </select>
                    ) : (
                      <p className="form-hint">
                        No folders yet. Add one from the Folders page, then come back.
                      </p>
                    )}
                  </div>
                  <div className="form-field">
                    <label className="form-label" htmlFor="review-wizard-repo">
                      Repo
                    </label>
                    <MenuButton
                      id="review-wizard-repo"
                      testId="review-wizard-repo"
                      searchTestId="review-wizard-repo-search"
                      items={repoItems}
                      searchable
                      haspopup="listbox"
                      matchTriggerWidth
                      align="left"
                      ariaLabel="Choose a repo"
                      listLabel="Repo options"
                      searchPlaceholder="Search repos…"
                      emptyLabel={repos === null ? 'Loading…' : 'No repos found'}
                      triggerClassName="form-input review-wizard__picker"
                    >
                      <span className="review-wizard__picker-value">
                        {pickedRepo ? pickedRepo.name : BROWSE_FOLDER_LABEL}
                      </span>
                      <span className="review-wizard__picker-detail">
                        {pickedRepo
                          ? pickedRepo.path || 'Folder root'
                          : 'Every markdown file in the folder'}
                      </span>
                    </MenuButton>
                    <FieldError
                      message={reposError ?? undefined}
                      testId="review-wizard-repo-error"
                    />
                  </div>
                  {/* Only a repo has worktrees — the level appears with one. */}
                  {pickedRepo && (
                    <div className="form-field">
                      <label className="form-label" htmlFor="review-wizard-worktree">
                        Worktree
                      </label>
                      <MenuButton
                        id="review-wizard-worktree"
                        testId="review-wizard-worktree"
                        searchTestId="review-wizard-worktree-search"
                        items={worktreeItems}
                        searchable
                        haspopup="listbox"
                        matchTriggerWidth
                        align="left"
                        ariaLabel="Choose a worktree"
                        listLabel="Worktree options"
                        searchPlaceholder="Search worktrees…"
                        emptyLabel="No worktrees found"
                        triggerClassName="form-input review-wizard__picker"
                      >
                        <span className="review-wizard__picker-value">
                          {pickedWorktree
                            ? (pickedWorktree.card_title ?? pickedWorktree.branch)
                            : 'Select a worktree…'}
                        </span>
                        {pickedWorktree && (
                          <span className="review-wizard__picker-detail">
                            {pickedWorktree.main ? 'Main checkout' : pickedWorktree.path}
                          </span>
                        )}
                      </MenuButton>
                    </div>
                  )}
                </>
              )}

              <div className="form-field">
                <label className="form-label" htmlFor="review-wizard-file">
                  {pickerLabel}
                </label>
                <MenuButton
                  id="review-wizard-file"
                  testId="review-wizard-file"
                  searchTestId="review-wizard-file-search"
                  items={pickerItems}
                  searchable
                  haspopup="listbox"
                  matchTriggerWidth
                  align="left"
                  ariaLabel={`Choose a ${pickerLabel.toLowerCase()}`}
                  listLabel={`${pickerLabel} options`}
                  searchPlaceholder={`Search ${pickerLabel.toLowerCase()}s…`}
                  emptyLabel={
                    candidates === null ? 'Loading…' : `No ${pickerLabel.toLowerCase()}s found`
                  }
                  triggerClassName="form-input review-wizard__picker"
                >
                  <span className="review-wizard__picker-value">
                    {picked ? picked.label : `Select a ${pickerLabel.toLowerCase()}…`}
                  </span>
                  {picked && <span className="review-wizard__picker-detail">{picked.detail}</span>}
                </MenuButton>
                <FieldError
                  message={candidatesError ?? undefined}
                  testId="review-wizard-list-error"
                />
              </div>

              <div className="form-field">
                <label className="form-label" htmlFor="review-wizard-title">
                  Title
                </label>
                <input
                  id="review-wizard-title"
                  className="form-input"
                  data-testid="review-wizard-title"
                  value={title}
                  onChange={(e) => setTitle(e.target.value)}
                  placeholder="Name this review"
                />
                <p className="form-hint">Defaults to the document&apos;s own name.</p>
              </div>
            </div>

            <div className="review-wizard__preview" data-testid="review-wizard-preview">
              {previewError ? (
                <p className="form-error">{previewError}</p>
              ) : preview !== null ? (
                <SafeMarkdown>{preview}</SafeMarkdown>
              ) : picked ? (
                <p className="review-wizard__preview-empty">Loading preview…</p>
              ) : (
                <p className="review-wizard__preview-empty">
                  Pick a {pickerLabel.toLowerCase()} to preview it here.
                </p>
              )}
            </div>
          </div>

          {error && <p className="form-error">{error}</p>}
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={() => setStep(1)}>
              Back
            </button>
            {!submitting && disabledReason && (
              <span className="form-actions-reason" data-testid="review-wizard-disabled-reason">
                {disabledReason}
              </span>
            )}
            <button
              type="button"
              className="btn-primary"
              data-testid="review-wizard-create"
              disabled={submitting || !!disabledReason}
              onClick={() => void submit()}
            >
              {submitting ? 'Creating…' : 'Create review'}
            </button>
          </div>
        </>
      )}
    </Modal>
  )
}
