import { useState } from 'react'
import Modal from './Modal'
import type { ToolImage } from './chat/events'

interface ToolUseBlockProps {
  toolName: string
  input?: Record<string, unknown>
  output?: Record<string, unknown>
  error?: string
  images?: ToolImage[]
  isRunning?: boolean
}

/** Build a `data:` URL from an inline tool image. */
function imageDataUrl(img: ToolImage): string {
  const mime = img.mimeType || 'image/png'
  return `data:${mime};base64,${img.dataBase64}`
}

/** Map tool names to friendly labels. Per-tool emoji icons were removed
 *  in favour of a single shared chevron \u2014 the chevron doubles as the
 *  expand/collapse affordance, so every tool row stays visually flat. */
function getToolLabel(toolName: string): string {
  switch (toolName) {
    case 'Bash':
      return 'Terminal'
    case 'Read':
      return 'Read file'
    case 'Write':
      return 'Write file'
    case 'Edit':
      return 'Edit file'
    case 'Grep':
      return 'Search content'
    case 'Glob':
      return 'Find files'
    case 'ToolSearch':
      return 'Tool search'
    case 'Agent':
      return 'Sub-agent'
    case 'WebFetch':
      return 'Fetch URL'
    case 'WebSearch':
      return 'Web search'
    case 'NotebookEdit':
      return 'Edit notebook'
    case 'TaskCreate':
    case 'TaskUpdate':
    case 'TaskGet':
    case 'TaskList':
      return toolName.replace('Task', 'Task ')
    default:
      if (toolName.startsWith('mcp__peckboard__')) {
        return toolName.replace('mcp__peckboard__', '').replace(/_/g, ' ')
      }
      if (toolName.startsWith('mcp__')) {
        return toolName.replace(/^mcp__[^_]+__/, '').replace(/_/g, ' ')
      }
      return toolName
  }
}

/** Extract a concise one-line summary from tool input */
function getSummary(toolName: string, input?: Record<string, unknown>): string {
  if (!input) return ''

  switch (toolName) {
    case 'Bash': {
      const cmd = input.command as string | undefined
      if (!cmd) return ''
      return cmd.length > 120 ? cmd.slice(0, 117) + '...' : cmd
    }
    case 'Read': {
      const fp = input.file_path as string | undefined
      return fp ? shortenPath(fp) : ''
    }
    case 'Write': {
      const fp = input.file_path as string | undefined
      return fp ? shortenPath(fp) : ''
    }
    case 'Edit': {
      const fp = input.file_path as string | undefined
      return fp ? shortenPath(fp) : ''
    }
    case 'Grep': {
      const pattern = input.pattern as string | undefined
      const path = input.path as string | undefined
      const parts: string[] = []
      if (pattern) parts.push(`"${pattern}"`)
      if (path) parts.push(`in ${shortenPath(path)}`)
      return parts.join(' ')
    }
    case 'Glob': {
      const pattern = input.pattern as string | undefined
      const path = input.path as string | undefined
      const parts: string[] = []
      if (pattern) parts.push(pattern)
      if (path) parts.push(`in ${shortenPath(path)}`)
      return parts.join(' ')
    }
    case 'Agent': {
      const desc = input.description as string | undefined
      return desc ?? ''
    }
    case 'WebFetch': {
      const url = input.url as string | undefined
      return url ?? ''
    }
    case 'WebSearch': {
      const query = input.query as string | undefined
      return query ?? ''
    }
    case 'ToolSearch': {
      const query = input.query as string | undefined
      return query ?? ''
    }
    default:
      return ''
  }
}

/** Shorten a file path to just the last 2-3 segments */
function shortenPath(p: string): string {
  const parts = p.split('/')
  if (parts.length <= 3) return p
  return '.../' + parts.slice(-3).join('/')
}

/** Format output for display: try to extract meaningful text */
function formatOutput(output: Record<string, unknown>): string {
  // If it has a single string value, show that
  const values = Object.values(output)
  if (values.length === 1 && typeof values[0] === 'string') {
    return values[0]
  }
  return JSON.stringify(output, null, 2)
}

export default function ToolUseBlock({
  toolName,
  input,
  output,
  error,
  images,
  isRunning,
}: ToolUseBlockProps) {
  const [expanded, setExpanded] = useState(false)
  // Index of the image currently shown full-size in the lightbox, or null.
  const [lightboxIdx, setLightboxIdx] = useState<number | null>(null)

  const label = getToolLabel(toolName)
  const summary = getSummary(toolName, input)
  const hasImages = !!images && images.length > 0
  const hasDetails = (input && Object.keys(input).length > 0) || output || error

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
        <span className="tool-label">{label}</span>
        {summary && <span className="tool-summary">{summary}</span>}
        {isRunning && <span className="tool-spinner" />}
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
          {output && !error && (
            <div className="tool-section">
              <div className="tool-section-label">Output</div>
              <pre className="tool-pre">{formatOutput(output)}</pre>
            </div>
          )}
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
