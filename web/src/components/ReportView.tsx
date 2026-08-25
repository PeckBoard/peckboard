import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type ReactNode,
} from 'react'
import type { Components } from 'react-markdown'
import { authedFetch } from '../store/auth'
import { useMediaQuery } from '../hooks/useMediaQuery'
import { copyText } from '../lib/clipboard'
import { extractToc, rehypeHeadingIds } from '../lib/markdownToc'
import SafeMarkdown from './SafeMarkdown'
import { chatMarkdownComponents } from './chat/markdown'
import { highlightPlugins } from './markdownHighlight'

interface ReportViewProps {
  folder: string
  file: string
  /** Navigate back to the report browser. */
  onBack: () => void
  /** Open a session by id — used by the "View Session" link in the
   *  report frontmatter, which jumps to the agent that produced it. */
  onOpenSession: (sessionId: string) => void
}

interface ReportMeta {
  folder: string
  file: string
  title: string
  date: string
  session_id?: string
  project_name?: string
  session_name?: string
  session_created_at?: string
}

/** Discriminated union for the report fetch lifecycle. Single state
 *  variable so the `useEffect` only ever calls setState inside its
 *  async callback (`react-hooks/set-state-in-effect` would fail a
 *  multi-setState reset pattern at the top of the effect). */
type FetchState =
  | { status: 'loading'; folder: string; file: string }
  | { status: 'ready'; folder: string; file: string; meta: ReportMeta; body: string }
  | { status: 'error'; folder: string; file: string; message: string }

/** Headings get an `id` (from `rehypeHeadingIds`) plus a visible anchor
 *  link, so any section of a long report can be linked directly. The
 *  anchor is JSX — no HTML is parsed out of the markdown source, so
 *  [[SafeMarkdown]]'s sanitisation story is unchanged. */
function headingWithAnchor(Tag: 'h2' | 'h3' | 'h4') {
  return function ReportHeading({ id, children }: { id?: string; children?: ReactNode }) {
    return (
      <Tag id={id} className="report-heading">
        {children}
        {id && (
          <a className="report-heading-anchor" href={`#${id}`} aria-label="Link to this section">
            #
          </a>
        )}
      </Tag>
    )
  }
}

/** A report is one of the documents the review screen renders, so it reads the
 *  same way here: diagrams for ```mermaid fences and highlighted fenced code,
 *  plus this screen's own anchored headings. */
const markdownComponents: Components = {
  ...chatMarkdownComponents,
  h2: headingWithAnchor('h2'),
  h3: headingWithAnchor('h3'),
  h4: headingWithAnchor('h4'),
}

// Module scope so the identity is stable across renders (a fresh array
// would re-run the whole markdown pipeline on every keystroke elsewhere).
const rehypePlugins = [rehypeHeadingIds, ...highlightPlugins]

/**
 * Single-report viewer. Route-driven (mounted from App.tsx at
 * `/reports/:folder/:file`) so the report can be opened as a tab and
 * deep-linked across devices via the cross-device tab strip.
 *
 * Companion to [[ReportBrowser]], which is the list / index page.
 */
export default function ReportView({ folder, file, onBack, onOpenSession }: ReportViewProps) {
  // Download is a fetch + synthetic <a> click, so a failed request has no
  // browser-level feedback of its own: without this the button looks inert
  // and the user clicks again.
  const [downloading, setDownloading] = useState(false)
  const [downloadError, setDownloadError] = useState<string | null>(null)
  const [state, setState] = useState<FetchState>({ status: 'loading', folder, file })

  useEffect(() => {
    let cancelled = false
    const url = `/api/reports/${encodeURIComponent(folder)}/${encodeURIComponent(file)}`
    authedFetch(url)
      .then(async (res) => {
        if (cancelled) return
        if (!res.ok) {
          const message = res.status === 404 ? 'Report not found.' : 'Failed to load report.'
          setState({ status: 'error', folder, file, message })
          return
        }
        const data = (await res.json()) as ReportMeta & { body?: string; content?: string }
        if (cancelled) return
        setState({
          status: 'ready',
          folder,
          file,
          meta: {
            folder: data.folder,
            file: data.file,
            title: data.title,
            date: data.date,
            session_id: data.session_id,
            project_name: data.project_name,
            session_name: data.session_name,
            session_created_at: data.session_created_at,
          },
          body: data.body ?? data.content ?? '',
        })
      })
      .catch(() => {
        if (cancelled) return
        setState({ status: 'error', folder, file, message: 'Failed to load report.' })
      })
    return () => {
      cancelled = true
    }
  }, [folder, file])

  // When the route swaps to a different report mid-mount, reset the
  // displayed state to "loading" so we don't flash stale meta from the
  // previous report. Derived from props rather than synchronously
  // setState'd inside the effect.
  const displayed: FetchState =
    state.folder === folder && state.file === file ? state : { status: 'loading', folder, file }

  const meta = displayed.status === 'ready' ? displayed.meta : null
  const body = displayed.status === 'ready' ? displayed.body : ''
  const loading = displayed.status === 'loading'

  // Table of contents, read off the markdown source; `rehypeHeadingIds`
  // gives the rendered headings the matching `id`s.
  const toc = useMemo(() => extractToc(body), [body])
  const scrollRef = useRef<HTMLDivElement | null>(null)
  const [activeSlug, setActiveSlug] = useState<string | null>(null)
  // On a phone a permanent TOC column would eat the screen, so there it
  // collapses behind a disclosure button instead.
  const collapsible = useMediaQuery('(max-width: 768px)')
  const [tocOpen, setTocOpen] = useState(false)
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'error'>('idle')

  const scrollToSlug = useCallback((slug: string) => {
    // Attribute selector, not `#slug`: a heading like "7. Findings"
    // slugifies to `7-findings`, which is not a valid id selector.
    const el = scrollRef.current?.querySelector(`[id="${slug}"]`)
    if (!el) return false
    el.scrollIntoView({ block: 'start' })
    return true
  }, [])

  // A `#section` fragment has to survive a reload: the markdown isn't in
  // the DOM until the fetch lands, so re-apply the hash once it is.
  useEffect(() => {
    if (loading || !body) return
    const slug = decodeURIComponent(window.location.hash.slice(1))
    if (!slug) return
    const raf = requestAnimationFrame(() => {
      scrollToSlug(slug)
    })
    return () => cancelAnimationFrame(raf)
  }, [loading, body, scrollToSlug])

  // Mark the section the reader is currently in. Heading elements are
  // resolved once per report (not per scroll event) — a long agent report
  // can carry dozens of headings.
  useEffect(() => {
    const container = scrollRef.current
    if (!container || toc.length === 0) return
    let headings: { slug: string; el: Element }[] = []
    const update = () => {
      // A heading counts as "current" once it reaches the top third of the
      // pane, not the very top edge — otherwise the last section of a
      // report can never become current (the scroller runs out of room).
      const threshold = container.getBoundingClientRect().top + container.clientHeight * 0.3
      let current: string | null = headings.length > 0 ? headings[0].slug : null
      for (const h of headings) {
        if (h.el.getBoundingClientRect().top > threshold) break
        current = h.slug
      }
      setActiveSlug(current)
    }
    const measure = () => {
      headings = toc
        .map((entry) => ({
          slug: entry.slug,
          el: container.querySelector(`[id="${entry.slug}"]`),
        }))
        .filter((h): h is { slug: string; el: Element } => h.el !== null)
      update()
    }
    const raf = requestAnimationFrame(measure)
    container.addEventListener('scroll', update, { passive: true })
    return () => {
      cancelAnimationFrame(raf)
      container.removeEventListener('scroll', update)
    }
  }, [toc])

  // Drop the copy confirmation back to the idle label once it's been read.
  useEffect(() => {
    if (copyState === 'idle') return
    const t = setTimeout(() => setCopyState('idle'), 2000)
    return () => clearTimeout(t)
  }, [copyState])

  const onTocClick = (e: MouseEvent<HTMLAnchorElement>, slug: string) => {
    e.preventDefault()
    // replaceState, not pushState: walking Back through every section a
    // reader visited isn't useful, but the URL must stay shareable.
    window.history.replaceState(null, '', `${window.location.pathname}#${slug}`)
    setActiveSlug(slug)
    scrollToSlug(slug)
    if (collapsible) setTocOpen(false)
  }

  const copyMarkdown = async () => {
    setCopyState((await copyText(body)) ? 'copied' : 'error')
  }

  const downloadReport = async () => {
    if (downloading) return
    setDownloading(true)
    setDownloadError(null)
    try {
      const res = await authedFetch(
        `/api/reports/${encodeURIComponent(folder)}/${encodeURIComponent(file)}/download`,
      )
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const blob = await res.blob()
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = file
      a.click()
      URL.revokeObjectURL(url)
    } catch {
      setDownloadError("Couldn't download this report. Please try again.")
    } finally {
      setDownloading(false)
    }
  }

  if (displayed.status === 'error') {
    return (
      <div className="report-viewer">
        <div className="report-viewer-header">
          <button className="btn-secondary" onClick={onBack}>
            &larr; Back
          </button>
        </div>
        <div className="list-view-empty">
          <p>{displayed.message}</p>
        </div>
      </div>
    )
  }
  return (
    <div className="report-viewer">
      <div className="report-viewer-header">
        <button className="btn-secondary" onClick={onBack}>
          &larr; Back
        </button>
        <div className="report-viewer-meta">
          <h2 className="report-viewer-title">{meta?.title || file}</h2>
          <div className="report-viewer-info">
            <span>{folder}</span>
            {meta?.project_name && (
              <span className="report-viewer-project">{meta.project_name}</span>
            )}
            {meta?.session_name && (
              <span className="report-viewer-session">{meta.session_name}</span>
            )}
            {meta?.session_created_at && (
              <span className="report-viewer-session-created">
                Session created {new Date(meta.session_created_at).toLocaleString()}
              </span>
            )}
            {meta?.session_id && (
              <button
                className="report-viewer-session-link"
                onClick={() => onOpenSession(meta.session_id!)}
              >
                View Session
              </button>
            )}
          </div>
        </div>
        <div className="report-copy">
          <button
            className="btn-secondary"
            onClick={() => void copyMarkdown()}
            disabled={loading}
            data-testid="report-copy-markdown"
          >
            {copyState === 'copied'
              ? 'Copied'
              : copyState === 'error'
                ? 'Copy failed'
                : 'Copy markdown'}
          </button>
          <span className="sr-only" role="status" aria-live="polite">
            {copyState === 'copied'
              ? 'Report markdown copied to the clipboard'
              : copyState === 'error'
                ? "Couldn't copy the report markdown"
                : ''}
          </span>
        </div>
        <div className="report-download">
          <button
            className="btn-secondary"
            onClick={() => void downloadReport()}
            disabled={downloading}
            aria-busy={downloading || undefined}
            data-testid="report-download"
          >
            {downloading ? 'Downloading…' : 'Download'}
          </button>
          {downloadError && (
            <span className="form-error" role="alert" data-testid="report-download-error">
              {downloadError}
            </span>
          )}
        </div>
      </div>
      {loading ? (
        <div className="chat-loading">
          <div className="loading-spinner" />
        </div>
      ) : (
        <div className="report-body">
          {toc.length > 0 && (
            <nav className="report-toc" aria-label="Table of contents" data-testid="report-toc">
              {collapsible ? (
                <button
                  type="button"
                  className="report-toc-toggle"
                  aria-expanded={tocOpen}
                  aria-controls="report-toc-list"
                  onClick={() => setTocOpen((open) => !open)}
                  data-testid="report-toc-toggle"
                >
                  <span>On This Page</span>
                  <span className="report-toc-chevron" aria-hidden="true">
                    {tocOpen ? '▾' : '▸'}
                  </span>
                </button>
              ) : (
                <h3 className="report-toc-heading">On This Page</h3>
              )}
              {(!collapsible || tocOpen) && (
                <ul className="report-toc-list" id="report-toc-list">
                  {toc.map((entry) => (
                    <li
                      key={entry.slug}
                      className={`report-toc-item report-toc-depth-${entry.depth}`}
                    >
                      <a
                        className={`report-toc-link${activeSlug === entry.slug ? ' active' : ''}`}
                        href={`#${entry.slug}`}
                        aria-current={activeSlug === entry.slug ? 'true' : undefined}
                        onClick={(e) => onTocClick(e, entry.slug)}
                      >
                        {entry.text}
                      </a>
                    </li>
                  ))}
                </ul>
              )}
            </nav>
          )}
          <div className="report-scroll" ref={scrollRef}>
            <SafeMarkdown
              className="report-content"
              rehypePlugins={rehypePlugins}
              components={markdownComponents}
            >
              {body}
            </SafeMarkdown>
          </div>
        </div>
      )}
    </div>
  )
}
