/** Shared formatting for tool calls, used by the chat's tool rows, the
 *  kanban thought bubbles and the pre-hatch activity feed: friendly labels,
 *  the real command line for exec-like tools, and the model-supplied
 *  one-sentence reason. */

/** Strip an `mcp__<server>__` prefix, leaving the bare tool name. */
export function bareToolName(toolName: string): string {
  return toolName.replace(/^mcp__.+?__/, '')
}

/** Map tool names to friendly labels. Per-tool emoji icons were removed
 *  in favour of a single shared chevron — the chevron doubles as the
 *  expand/collapse affordance, so every tool row stays visually flat. */
export function getToolLabel(toolName: string): string {
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
      if (toolName.startsWith('mcp__')) {
        return bareToolName(toolName).replace(/_/g, ' ')
      }
      return toolName
  }
}

function joinStringArgs(value: unknown): string {
  return Array.isArray(value) ? value.filter((x) => typeof x === 'string').join(' ') : ''
}

/** The actual command line an exec-like tool is running (`cargo build
 *  --release`, `git log --oneline`, …), or '' for every other tool. The
 *  chat shows this as the row's primary text instead of the tool name. */
export function getCommandLine(toolName: string, input?: Record<string, unknown>): string {
  if (!input) return ''
  switch (bareToolName(toolName)) {
    case 'run_command': {
      const cmd = input.command
      if (typeof cmd !== 'string' || cmd === '') return ''
      const args = joinStringArgs(input.args)
      return args ? `${cmd} ${args}` : cmd
    }
    case 'git': {
      const sub = input.subcommand
      if (typeof sub !== 'string' || sub === '') return ''
      const args = joinStringArgs(input.args)
      return args ? `git ${sub} ${args}` : `git ${sub}`
    }
    case 'run_tests': {
      const runner = typeof input.runner === 'string' && input.runner !== 'auto' ? input.runner : ''
      const args = joinStringArgs(input.args)
      return ['run tests', runner ? `(${runner})` : '', args].filter(Boolean).join(' ')
    }
    // Native shells: Claude/Kimi's Bash, Cursor's shell.
    case 'Bash':
    case 'shell': {
      const cmd = input.command
      return typeof cmd === 'string' ? cmd : ''
    }
    default:
      return ''
  }
}

/** The model's one-sentence why for a tool call: our exec tools' `reason`,
 *  or the native shell tools' `description`. Other tools' `description`
 *  fields carry payloads (e.g. a card body), so they are not reasons. */
export function getToolReason(toolName: string, input?: Record<string, unknown>): string {
  if (!input) return ''
  const bare = bareToolName(toolName)
  const raw = input.reason ?? (bare === 'Bash' || bare === 'shell' ? input.description : undefined)
  return typeof raw === 'string' ? raw.trim() : ''
}

/** Shorten a file path to just the last 2-3 segments */
export function shortenPath(p: string): string {
  const parts = p.split('/')
  if (parts.length <= 3) return p
  return '.../' + parts.slice(-3).join('/')
}

/** Extract a concise one-line summary from tool input */
export function getSummary(toolName: string, input?: Record<string, unknown>): string {
  if (!input) return ''

  switch (toolName) {
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
      break
  }

  // Peckboard MCP tools — matched on the bare name so the `mcp__peckboard__`
  // (or any other server's) prefix doesn't matter.
  switch (bareToolName(toolName)) {
    case 'read_file': {
      const p = input.path as string | undefined
      if (!p) return ''
      const start = input.start_line as number | undefined
      return start ? `${shortenPath(p)}:${start}` : shortenPath(p)
    }
    case 'write_file':
    case 'edit_file':
    case 'file_outline': {
      const p = input.path as string | undefined
      return p ? shortenPath(p) : ''
    }
    case 'read_symbol': {
      const name = input.name as string | undefined
      const p = input.path as string | undefined
      return [name, p ? `in ${shortenPath(p)}` : ''].filter(Boolean).join(' ')
    }
    case 'search_files': {
      const q = input.query as string | undefined
      const scope = input.path_contains as string | undefined
      const parts: string[] = []
      if (q) parts.push(`"${q}"`)
      if (scope) parts.push(`in ${scope}`)
      return parts.join(' ')
    }
    case 'list_files': {
      const scope = input.path_contains as string | undefined
      return scope ?? ''
    }
    case 'fetch_url':
    case 'fetch_web': {
      const url = input.url as string | undefined
      return url ?? ''
    }
    case 'search_web': {
      const q = input.query as string | undefined
      return q ?? ''
    }
    case 'spawn_subagent': {
      const n = input.name as string | undefined
      return n ?? ''
    }
    default:
      return ''
  }
}
