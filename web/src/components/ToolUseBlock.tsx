import { useState } from 'react'

interface ToolUseBlockProps {
  toolName: string
  input?: Record<string, unknown>
  output?: Record<string, unknown>
  error?: string
  isRunning?: boolean
}

/** Map tool names to icons and friendly labels */
function getToolDisplay(toolName: string): { icon: string; label: string } {
  switch (toolName) {
    case 'Bash':
      return { icon: '\u{25B6}', label: 'Terminal' }
    case 'Read':
      return { icon: '\u{1F4C4}', label: 'Read file' }
    case 'Write':
      return { icon: '\u{270F}\uFE0F', label: 'Write file' }
    case 'Edit':
      return { icon: '\u{270F}\uFE0F', label: 'Edit file' }
    case 'Grep':
      return { icon: '\u{1F50D}', label: 'Search content' }
    case 'Glob':
      return { icon: '\u{1F4C2}', label: 'Find files' }
    case 'ToolSearch':
      return { icon: '\u{1F50D}', label: 'Tool search' }
    case 'Agent':
      return { icon: '\u{1F916}', label: 'Sub-agent' }
    case 'WebFetch':
      return { icon: '\u{1F310}', label: 'Fetch URL' }
    case 'WebSearch':
      return { icon: '\u{1F310}', label: 'Web search' }
    case 'NotebookEdit':
      return { icon: '\u{1F4D3}', label: 'Edit notebook' }
    case 'TaskCreate':
    case 'TaskUpdate':
    case 'TaskGet':
    case 'TaskList':
      return { icon: '\u{2611}\uFE0F', label: toolName.replace('Task', 'Task ') }
    default:
      // MCP tools: strip mcp__peckboard__ prefix
      if (toolName.startsWith('mcp__peckboard__')) {
        const name = toolName.replace('mcp__peckboard__', '').replace(/_/g, ' ')
        return { icon: '\u{1F527}', label: name }
      }
      if (toolName.startsWith('mcp__')) {
        const name = toolName.replace(/^mcp__[^_]+__/, '').replace(/_/g, ' ')
        return { icon: '\u{1F527}', label: name }
      }
      return { icon: '\u{2699}\uFE0F', label: toolName }
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

export default function ToolUseBlock({ toolName, input, output, error, isRunning }: ToolUseBlockProps) {
  const [expanded, setExpanded] = useState(false)

  const { icon, label } = getToolDisplay(toolName)
  const summary = getSummary(toolName, input)
  const hasDetails = (input && Object.keys(input).length > 0) || output || error

  const statusClass = error ? 'tool-error' : isRunning ? 'tool-running' : ''

  return (
    <div className={`tool-block ${statusClass}`}>
      <button className="tool-header" onClick={() => hasDetails && setExpanded((v) => !v)}>
        <span className="tool-icon">{icon}</span>
        <span className="tool-label">{label}</span>
        {summary && <span className="tool-summary">{summary}</span>}
        {isRunning && <span className="tool-spinner" />}
        {error && <span className="tool-status-badge tool-badge-error">Error</span>}
        {hasDetails && (
          <span className={`tool-chevron ${expanded ? 'open' : ''}`}>&#9654;</span>
        )}
      </button>
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
    </div>
  )
}
