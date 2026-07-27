import { useEffect, useMemo, useState } from 'react'
import rehypeHighlight from 'rehype-highlight'
import Modal from './Modal'
import DiffBlock from './DiffBlock'
import SafeMarkdown from './SafeMarkdown'
import SubagentTranscript from './SubagentTranscript'
import type { FileDiff, ToolImage } from './chat/events'
import { authedFetch } from '../store/auth'
import {
  bareToolName,
  getCommandLine,
  getSummary,
  getToolLabel,
  getToolReason,
  shortenPath,
} from './chat/toolDisplay'

interface ToolUseBlockProps {
  /** Session the tool event belongs to — scopes blob-image fetches. */
  sessionId: string
  toolName: string
  input?: Record<string, unknown>
  output?: Record<string, unknown> | string
  error?: string
  images?: ToolImage[]
  isRunning?: boolean
  /** Event timestamps (ms) of tool start/end — drive the duration badge. */
  startTs?: number
  endTs?: number
  /** Diff payload attached from a `file-diff` event. */
  diff?: FileDiff
}

/** Build a `data:` URL from a legacy inline tool image, or null when the
 *  image is a blob reference. */
function inlineDataUrl(img: ToolImage): string | null {
  if (!img.dataBase64) return null
  const mime = img.mimeType || 'image/png'
  return `data:${mime};base64,${img.dataBase64}`
}

/** Resolve a tool image to a displayable URL. Legacy inline images become
 *  `data:` URLs; blob references are fetched with auth (the tool-images
 *  route needs the Authorization header, so no plain <img src>) and swapped
 *  in as an object URL. Null while loading or on a missing blob. */
function useToolImageUrl(sessionId: string, img?: ToolImage): string | null {
  const [fetchedUrl, setFetchedUrl] = useState<string | null>(null)
  // Only blob references need the fetch; inline images resolve in render.
  const blobId = img && !img.dataBase64 ? img.id : undefined
  useEffect(() => {
    if (!blobId) return
    let objectUrl: string | null = null
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}/tool-images/${blobId}`)
      .then((r) => (r.ok ? r.blob() : Promise.reject(new Error(String(r.status)))))
      .then((b) => {
        const u = URL.createObjectURL(b)
        if (cancelled) {
          URL.revokeObjectURL(u)
          return
        }
        objectUrl = u
        setFetchedUrl(u)
      })
      .catch(() => {
        // Deleted/stale blob — leave the slot empty.
      })
    return () => {
      cancelled = true
      if (objectUrl) URL.revokeObjectURL(objectUrl)
    }
  }, [sessionId, blobId])
  if (!img) return null
  const inline = inlineDataUrl(img)
  if (inline) return inline
  return blobId ? fetchedUrl : null
}

/** One screenshot thumbnail; renders nothing until its URL resolves. */
function ToolImageThumb({
  sessionId,
  img,
  onOpen,
}: {
  sessionId: string
  img: ToolImage
  onOpen: () => void
}) {
  const url = useToolImageUrl(sessionId, img)
  if (!url) return null
  return (
    <button
      type="button"
      className="tool-image-thumb"
      onClick={onOpen}
      aria-label="Open screenshot"
      data-testid="tool-image-thumb"
    >
      <img src={url} alt="Screenshot" loading="lazy" />
    </button>
  )
}

/** Compact human duration: 3s, 1m 12s. */
function formatDuration(ms: number): string {
  const s = Math.round(ms / 1000)
  if (s < 60) return `${s}s`
  return `${Math.floor(s / 60)}m ${s % 60}s`
}

/** Seconds since `startTs`, ticking once a second while the tool runs. */
function useElapsedSeconds(startTs: number | undefined, running: boolean): number | null {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!running || !startTs) return
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [running, startTs])
  if (!running || !startTs) return null
  return Math.max(0, Math.floor((now - startTs) / 1000))
}

/** Tool output arrives either as a structured object (mock provider) or as
 *  the text the CLI echoed back — which for MCP tools is usually a JSON
 *  object serialized to a string. Parse that shape back out so the
 *  specialized sections below work over both transports. */
function normalizeOutput(
  output?: Record<string, unknown> | string,
): Record<string, unknown> | string | undefined {
  if (output === undefined || output === null) return undefined
  if (typeof output !== 'string') return output
  const s = output.trim()
  if (s.startsWith('{') && s.endsWith('}')) {
    try {
      const parsed: unknown = JSON.parse(s)
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
        return parsed as Record<string, unknown>
      }
    } catch {
      // Not JSON after all — show the raw text.
    }
  }
  return output
}

function strField(o: Record<string, unknown>, key: string): string | undefined {
  const v = o[key]
  return typeof v === 'string' && v !== '' ? v : undefined
}

/** Best-effort pass/fail counts from runner stdout (cargo, vitest, pytest…). */
function parseTestCounts(text: string): string {
  const passed = /(\d+)\s+passed/.exec(text)?.[1]
  const failed = /(\d+)\s+failed/.exec(text)?.[1]
  if (!passed && !failed) return ''
  const parts: string[] = []
  if (passed) parts.push(`${passed} passed`)
  if (failed && failed !== '0') parts.push(`${failed} failed`)
  return parts.join(', ')
}

/** Lines beyond which an output pane collapses behind a "Show all" toggle
 *  (with a little slack so a pane never clamps to hide only a few lines). */
const PRE_CLAMP_LINES = 40
const PRE_CLAMP_SLACK = 8

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false)
  useEffect(() => {
    if (!copied) return
    const id = setTimeout(() => setCopied(false), 1500)
    return () => clearTimeout(id)
  }, [copied])
  return (
    <button
      type="button"
      className="tool-mini-btn"
      title="Copy to clipboard"
      onClick={() => {
        void navigator.clipboard?.writeText(text).then(() => setCopied(true))
      }}
    >
      {copied ? 'Copied' : 'Copy'}
    </button>
  )
}

/** Mono pane for tool output. Unbounded text (stdout etc.) is clamped to
 *  the first PRE_CLAMP_LINES lines with an explicit expand toggle; Copy
 *  always copies the full text. */
function ClampedPre({ text, className }: { text: string; className?: string }) {
  const [showAll, setShowAll] = useState(false)
  const lines = useMemo(() => text.split('\n'), [text])
  const clampable = lines.length > PRE_CLAMP_LINES + PRE_CLAMP_SLACK
  const clamped = clampable && !showAll
  const visible = clamped ? lines.slice(0, PRE_CLAMP_LINES).join('\n') : text
  return (
    <div className="tool-pre-wrap">
      <div className="tool-pre-toolbar">
        {clampable && (
          <button
            type="button"
            className="tool-mini-btn"
            aria-expanded={showAll}
            onClick={() => setShowAll((v) => !v)}
          >
            {showAll ? 'Collapse' : `Show all ${lines.length} lines`}
          </button>
        )}
        <CopyButton text={text} />
      </div>
      <pre className={`tool-pre ${className ?? ''}`}>
        {visible}
        {clamped ? `\n… ${lines.length - PRE_CLAMP_LINES} more lines` : ''}
      </pre>
    </div>
  )
}

/** Syntax-highlighted preview of a file's content (write_file / Write
 *  inputs): the content is fenced as markdown so rehype-highlight colors
 *  it, with the fence kept longer than any backtick run inside. */
function CodePreview({ path, content }: { path?: string; content: string }) {
  const [showAll, setShowAll] = useState(false)
  const lines = content.split('\n')
  const clampable = lines.length > PRE_CLAMP_LINES + PRE_CLAMP_SLACK
  const clamped = clampable && !showAll
  const visible = clamped ? lines.slice(0, PRE_CLAMP_LINES).join('\n') : content
  const ext = path?.match(/\.([A-Za-z0-9]+)$/)?.[1]?.toLowerCase() ?? ''
  const fenceLen = Math.max(3, ...(visible.match(/`+/g) ?? []).map((r) => r.length + 1))
  const fence = '`'.repeat(fenceLen)
  return (
    <div className="tool-pre-wrap">
      <div className="tool-pre-toolbar">
        {clampable && (
          <button
            type="button"
            className="tool-mini-btn"
            aria-expanded={showAll}
            onClick={() => setShowAll((v) => !v)}
          >
            {showAll ? 'Collapse' : `Show all ${lines.length} lines`}
          </button>
        )}
        <CopyButton text={content} />
      </div>
      <SafeMarkdown className="chat-markdown tool-code-preview" rehypePlugins={[rehypeHighlight]}>
        {`${fence}${ext}\n${visible}${clamped ? `\n… ${lines.length - PRE_CLAMP_LINES} more lines` : ''}\n${fence}`}
      </SafeMarkdown>
    </div>
  )
}

/** Old/new pair from a native Edit (or one MultiEdit entry) as a mini diff. */
function MiniDiff({ oldText, newText }: { oldText: string; newText: string }) {
  return (
    <pre className="tool-pre tool-minidiff">
      {oldText.split('\n').map((l, i) => (
        <div key={`o${i}`} className="diff-line-del">
          - {l || ' '}
        </div>
      ))}
      {newText.split('\n').map((l, i) => (
        <div key={`n${i}`} className="diff-line-add">
          + {l || ' '}
        </div>
      ))}
    </pre>
  )
}

function strInput(input: Record<string, unknown>, key: string): string | undefined {
  const v = input[key]
  return typeof v === 'string' ? v : undefined
}

/** Per-tool structured input rendering: native Edit/MultiEdit show old/new
 *  mini-diffs, Peckboard edit_file shows its positional ops, write tools
 *  show a highlighted content preview; anything unrecognized falls back to
 *  pretty-printed JSON. */
function InputSections({ toolName, input }: { toolName: string; input: Record<string, unknown> }) {
  const bare = bareToolName(toolName)
  const filePath = strInput(input, 'path') ?? strInput(input, 'file_path')
  const pathSuffix = filePath ? ` — ${shortenPath(filePath)}` : ''

  if (bare === 'Edit') {
    const oldStr = strInput(input, 'old_string')
    const newStr = strInput(input, 'new_string')
    if (oldStr !== undefined && newStr !== undefined) {
      return (
        <div className="tool-section">
          <div className="tool-section-label">Edit{pathSuffix}</div>
          <MiniDiff oldText={oldStr} newText={newStr} />
        </div>
      )
    }
  }

  if (bare === 'MultiEdit' && Array.isArray(input.edits)) {
    const pairs = (input.edits as unknown[])
      .map((e) => (e ?? {}) as Record<string, unknown>)
      .filter((e) => typeof e.old_string === 'string' && typeof e.new_string === 'string')
    if (pairs.length > 0) {
      return (
        <div className="tool-section">
          <div className="tool-section-label">
            {pairs.length === 1 ? 'Edit' : `${pairs.length} edits`}
            {pathSuffix}
          </div>
          {pairs.map((e, i) => (
            <MiniDiff key={i} oldText={e.old_string as string} newText={e.new_string as string} />
          ))}
        </div>
      )
    }
  }

  if (bare === 'edit_file' && Array.isArray(input.edits)) {
    // Peckboard's edit_file is positional (op + line range + text) — show
    // each op with its target lines instead of the raw JSON envelope.
    const ops = (input.edits as unknown[]).map((e) => (e ?? {}) as Record<string, unknown>)
    return (
      <div className="tool-section">
        <div className="tool-section-label">
          {ops.length === 1 ? 'Edit' : `${ops.length} edits`}
          {pathSuffix}
        </div>
        {ops.map((op, i) => {
          const kind = typeof op.op === 'string' ? op.op : 'edit'
          const at =
            op.start_line !== undefined
              ? `lines ${op.start_line}${
                  op.end_line !== undefined && op.end_line !== op.start_line
                    ? `–${op.end_line}`
                    : ''
                }`
              : op.line !== undefined
                ? `line ${op.line}`
                : ''
          const text = typeof op.text === 'string' ? op.text : undefined
          return (
            <div key={i} className="tool-editop">
              <div className="tool-editop-head">
                {kind}
                {at ? ` ${at}` : ''}
              </div>
              {text !== undefined && (
                <pre className="tool-pre tool-minidiff">
                  {text.split('\n').map((l, j) => (
                    <div key={j} className={kind === 'delete' ? 'diff-line-del' : 'diff-line-add'}>
                      {kind === 'delete' ? '-' : '+'} {l || ' '}
                    </div>
                  ))}
                </pre>
              )}
            </div>
          )
        })}
      </div>
    )
  }

  const content =
    strInput(input, 'content') ?? (bare === 'Write' ? strInput(input, 'file_text') : undefined)
  if ((bare === 'write_file' || bare === 'Write') && content !== undefined) {
    return (
      <div className="tool-section">
        <div className="tool-section-label">Content{pathSuffix}</div>
        <CodePreview path={filePath} content={content} />
      </div>
    )
  }

  return (
    <div className="tool-section">
      <div className="tool-section-label">Input</div>
      <ClampedPre text={JSON.stringify(input, null, 2)} />
    </div>
  )
}

/** Purpose-built output sections: exec tools get stdout/stderr panes, file
 *  reads show their content, searches list their matches; anything
 *  unrecognized falls back to pretty-printed JSON. */
function OutputSections({ out }: { out: Record<string, unknown> | string }) {
  if (typeof out === 'string') {
    return (
      <div className="tool-section">
        <div className="tool-section-label">Output</div>
        <ClampedPre text={out} />
      </div>
    )
  }
  const stdout = strField(out, 'stdout')
  const stderr = strField(out, 'stderr')
  if (stdout !== undefined || stderr !== undefined || typeof out.exit_code === 'number') {
    return (
      <>
        {stdout && (
          <div className="tool-section">
            <div className="tool-section-label">Stdout</div>
            <ClampedPre text={stdout + (out.stdout_truncated === true ? '\n… (truncated)' : '')} />
          </div>
        )}
        {stderr && (
          <div className="tool-section">
            <div className="tool-section-label">Stderr</div>
            <ClampedPre
              text={stderr + (out.stderr_truncated === true ? '\n… (truncated)' : '')}
              className="tool-pre-stderr"
            />
          </div>
        )}
        {!stdout && !stderr && (
          <div className="tool-section">
            <div className="tool-section-label">Output</div>
            <pre className="tool-pre">(no output)</pre>
          </div>
        )}
      </>
    )
  }
  const content = strField(out, 'content')
  if (content !== undefined) {
    return (
      <div className="tool-section">
        <div className="tool-section-label">Content</div>
        <ClampedPre text={content} />
      </div>
    )
  }
  const matches = out.matches
  if (Array.isArray(matches) && matches.length > 0) {
    const first = (matches[0] ?? {}) as Record<string, unknown>
    if (typeof first.path === 'string') {
      const lines = matches.map((m) => {
        const mm = (m ?? {}) as Record<string, unknown>
        const text = typeof mm.text === 'string' ? mm.text.trim() : ''
        return `${mm.path as string}:${mm.line ?? ''} ${text}`.trimEnd()
      })
      return (
        <div className="tool-section">
          <div className="tool-section-label">Matches ({matches.length})</div>
          <ClampedPre text={lines.join('\n')} />
        </div>
      )
    }
  }
  const values = Object.values(out)
  const text =
    values.length === 1 && typeof values[0] === 'string' ? values[0] : JSON.stringify(out, null, 2)
  return (
    <div className="tool-section">
      <div className="tool-section-label">Output</div>
      <ClampedPre text={text} />
    </div>
  )
}

export default function ToolUseBlock({
  sessionId,
  toolName,
  input,
  output,
  error,
  images,
  isRunning,
  startTs,
  endTs,
  diff,
}: ToolUseBlockProps) {
  const [expanded, setExpanded] = useState(false)
  // Index of the image currently shown full-size in the lightbox, or null.
  const [lightboxIdx, setLightboxIdx] = useState<number | null>(null)
  const elapsedSec = useElapsedSeconds(startTs, !!isRunning)
  const durationMs = !isRunning && startTs && endTs ? Math.max(0, endTs - startTs) : 0

  // Exec-like tools (run_command, git, run_tests, native shells) show the
  // real command line as the row's primary text — never the tool name —
  // with the model's one-sentence reason after it.
  const commandLine = getCommandLine(toolName, input)
  const reason = getToolReason(toolName, input)
  const label = getToolLabel(toolName)
  const summary = getSummary(toolName, input) || reason
  const hasImages = !!images && images.length > 0

  const out = normalizeOutput(output)
  const outObj = out !== undefined && typeof out !== 'string' ? out : undefined
  const exitCode =
    outObj && typeof outObj.exit_code === 'number' ? (outObj.exit_code as number) : undefined
  const timedOut = outObj?.timed_out === true
  const passedFlag =
    outObj && typeof outObj.passed === 'boolean' ? (outObj.passed as boolean) : undefined
  const testCounts =
    passedFlag !== undefined && outObj ? parseTestCounts(strField(outObj, 'stdout') ?? '') : ''
  const replayUrl =
    outObj && typeof outObj.run_id === 'string' && bareToolName(toolName).startsWith('browser_')
      ? `/plugin-page/playwright-video/playwright-tests?run=${encodeURIComponent(outObj.run_id as string)}`
      : undefined
  const subagentSessionId =
    outObj &&
    typeof outObj.subagent_session_id === 'string' &&
    bareToolName(toolName) === 'spawn_subagent'
      ? (outObj.subagent_session_id as string)
      : undefined
  const done = !isRunning && !error

  const hasDetails = (input && Object.keys(input).length > 0) || out !== undefined || error

  const statusClass = error ? 'tool-error' : isRunning ? 'tool-running' : ''
  const lightboxImage = lightboxIdx !== null ? images?.[lightboxIdx] : undefined
  const lightboxUrl = useToolImageUrl(sessionId, lightboxImage)

  return (
    <div className={`tool-block ${statusClass}`}>
      <button
        className="tool-header"
        aria-expanded={hasDetails ? expanded : undefined}
        onClick={() => hasDetails && setExpanded((v) => !v)}
      >
        <span
          className={`tool-chevron ${expanded ? 'open' : ''} ${hasDetails ? '' : 'tool-chevron-leaf'}`}
          aria-hidden="true"
        >
          &#9654;
        </span>
        {commandLine ? (
          <>
            <span className="tool-cmd" title={commandLine}>
              {commandLine.length > 120 ? commandLine.slice(0, 117) + '...' : commandLine}
            </span>
            {reason && <span className="tool-reason">{reason}</span>}
          </>
        ) : (
          <>
            <span className="tool-label">{label}</span>
            {summary && <span className="tool-summary">{summary}</span>}
          </>
        )}
        {isRunning && <span className="tool-spinner" role="status" aria-label="Running" />}
        {isRunning && elapsedSec !== null && elapsedSec >= 1 && (
          <span className="tool-elapsed">{formatDuration(elapsedSec * 1000)}</span>
        )}
        {durationMs >= 1000 && <span className="tool-duration">{formatDuration(durationMs)}</span>}
        {done && timedOut && <span className="tool-status-badge tool-badge-error">Timeout</span>}
        {done && !timedOut && passedFlag !== undefined && (
          <span
            className={`tool-status-badge ${passedFlag ? 'tool-badge-success' : 'tool-badge-error'}`}
          >
            {passedFlag ? 'Pass' : 'Fail'}
          </span>
        )}
        {done &&
          !timedOut &&
          passedFlag === undefined &&
          exitCode !== undefined &&
          exitCode !== 0 && (
            <span className="tool-status-badge tool-badge-error">exit {exitCode}</span>
          )}
        {done && testCounts && <span className="tool-counts">{testCounts}</span>}
        {error && <span className="tool-status-badge tool-badge-error">Error</span>}
      </button>
      {/* Screenshots render outside the collapsible body so they're visible
          at a glance — the whole point of capturing them. Clicking a
          thumbnail opens the full image in a lightbox. */}
      {hasImages && (
        <div className="tool-images" data-testid="tool-images">
          {images!.map((img, i) => (
            <ToolImageThumb
              key={img.id ?? i}
              sessionId={sessionId}
              img={img}
              onOpen={() => setLightboxIdx(i)}
            />
          ))}
        </div>
      )}
      {replayUrl && (
        <div className="tool-replay">
          <a
            href={replayUrl}
            target="_blank"
            rel="noreferrer noopener"
            title="Open the recorded replay (Playwright Tests plugin)"
          >
            ▶ View replay
          </a>
        </div>
      )}
      {subagentSessionId && <SubagentTranscript sessionId={subagentSessionId} />}
      {diff && <DiffBlock diff={diff} />}
      {expanded && (
        <div className="tool-body">
          {input && Object.keys(input).length > 0 && (
            <InputSections toolName={toolName} input={input} />
          )}
          {error && (
            <div className="tool-section">
              <div className="tool-section-label">Error</div>
              <ClampedPre text={error} className="tool-pre-error" />
            </div>
          )}
          {out !== undefined && !error && <OutputSections out={out} />}
        </div>
      )}
      {lightboxImage && lightboxUrl && (
        <Modal
          onClose={() => setLightboxIdx(null)}
          className="image-lightbox"
          backdropClassName="image-lightbox-backdrop"
          data-testid="tool-image-lightbox"
        >
          <img src={lightboxUrl} alt="Screenshot" className="image-lightbox-img" />
        </Modal>
      )}
    </div>
  )
}
