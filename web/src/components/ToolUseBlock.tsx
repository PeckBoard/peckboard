import { useEffect, useState } from 'react'
import Modal from './Modal'
import DiffBlock from './DiffBlock'
import SubagentTranscript from './SubagentTranscript'
import type { FileDiff, ToolImage } from './chat/events'
import {
  bareToolName,
  getCommandLine,
  getSummary,
  getToolLabel,
  getToolReason,
} from './chat/toolDisplay'

interface ToolUseBlockProps {
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

/** Build a `data:` URL from an inline tool image. */
function imageDataUrl(img: ToolImage): string {
  const mime = img.mimeType || 'image/png'
  return `data:${mime};base64,${img.dataBase64}`
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

/** Purpose-built output sections: exec tools get stdout/stderr panes, file
 *  reads show their content, searches list their matches; anything
 *  unrecognized falls back to pretty-printed JSON. */
function OutputSections({ out }: { out: Record<string, unknown> | string }) {
  if (typeof out === 'string') {
    return (
      <div className="tool-section">
        <div className="tool-section-label">Output</div>
        <pre className="tool-pre">{out}</pre>
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
            <pre className="tool-pre">
              {stdout}
              {out.stdout_truncated === true ? '\n… (truncated)' : ''}
            </pre>
          </div>
        )}
        {stderr && (
          <div className="tool-section">
            <div className="tool-section-label">Stderr</div>
            <pre className="tool-pre tool-pre-stderr">
              {stderr}
              {out.stderr_truncated === true ? '\n… (truncated)' : ''}
            </pre>
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
        <pre className="tool-pre">{content}</pre>
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
          <pre className="tool-pre">{lines.join('\n')}</pre>
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
      <pre className="tool-pre">{text}</pre>
    </div>
  )
}

export default function ToolUseBlock({
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

  return (
    <div className={`tool-block ${statusClass}`}>
      <button className="tool-header" onClick={() => hasDetails && setExpanded((v) => !v)}>
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
        {isRunning && <span className="tool-spinner" />}
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
            <button
              key={i}
              type="button"
              className="tool-image-thumb"
              onClick={() => setLightboxIdx(i)}
              aria-label="Open screenshot"
              data-testid="tool-image-thumb"
            >
              <img src={imageDataUrl(img)} alt="Screenshot" loading="lazy" />
            </button>
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
            <div className="tool-section">
              <div className="tool-section-label">Input</div>
              <pre className="tool-pre">{JSON.stringify(input, null, 2)}</pre>
            </div>
          )}
          {error && (
            <div className="tool-section">
              <div className="tool-section-label">Error</div>
              <pre className="tool-pre tool-pre-error">{error}</pre>
            </div>
          )}
          {out !== undefined && !error && <OutputSections out={out} />}
        </div>
      )}
      {lightboxImage && (
        <Modal
          onClose={() => setLightboxIdx(null)}
          className="image-lightbox"
          backdropClassName="image-lightbox-backdrop"
          data-testid="tool-image-lightbox"
        >
          <img src={imageDataUrl(lightboxImage)} alt="Screenshot" className="image-lightbox-img" />
        </Modal>
      )}
    </div>
  )
}
