/** Shared formatting for tool calls, used by the chat's tool rows, the
 *  kanban thought bubbles and the pre-hatch activity feed: friendly labels,
 *  the real command line for exec-like tools, and the model-supplied
 *  one-sentence reason.
 *
 *  Every provider drives these rows, and each CLI names the same handful of
 *  actions differently — Claude's `Read`, cursor's `read`, grok/kimi's
 *  `read_file`. Matching is therefore on the BARE name (no `mcp__server__`
 *  prefix), case-folded, against one synonym table, so a row reads the same
 *  whichever agent produced it. Anything unmapped is humanized rather than
 *  dumped raw. */

/** Strip an `mcp__<server>__` prefix, leaving the bare tool name. */
export function bareToolName(toolName: string): string {
  return toolName.replace(/^mcp__.+?__/, '')
}

/** Friendly labels keyed on the case-folded bare name. Per-tool emoji icons
 *  were removed in favour of a single shared chevron — the chevron doubles
 *  as the expand/collapse affordance, so every tool row stays visually
 *  flat. */
const TOOL_LABELS: Record<string, string> = {
  // Shells: Claude/kimi `Bash`, cursor `shell`, Peckboard's `run_command`.
  bash: 'Terminal',
  shell: 'Terminal',
  terminal: 'Terminal',
  run_command: 'Terminal',
  run_terminal_cmd: 'Terminal',
  execute_command: 'Terminal',
  // Files.
  read: 'Read file',
  read_file: 'Read file',
  view_file: 'Read file',
  write: 'Write file',
  write_file: 'Write file',
  create_file: 'Write file',
  edit: 'Edit file',
  edit_file: 'Edit file',
  multiedit: 'Edit file',
  str_replace: 'Edit file',
  apply_patch: 'Edit file',
  delete: 'Delete file',
  delete_file: 'Delete file',
  // Listing / search.
  ls: 'List files',
  list_dir: 'List files',
  list_files: 'List files',
  glob: 'Find files',
  grep: 'Search content',
  search_files: 'Search content',
  codebase_search: 'Semantic search',
  semsearch: 'Semantic search',
  // Web.
  webfetch: 'Fetch URL',
  fetch_url: 'Fetch URL',
  websearch: 'Web search',
  web_search: 'Web search',
  search_web: 'Web search',
  // Agent plumbing.
  toolsearch: 'Tool search',
  agent: 'Sub-agent',
  spawn_subagent: 'Sub-agent',
  notebookedit: 'Edit notebook',
  todowrite: 'Tasks',
  todo: 'Tasks',
  // Cursor's MCP plumbing: one generic `mcp` name for every server call
  // (unwrapped to the real tool by the cursor parser, so this is only the
  // fallback) and a separate tool-listing call.
  mcp: 'MCP tool',
  getmcptools: 'List MCP tools',
}

/** Turn an unmapped tool name into words: `getMcpTools` → "Get Mcp Tools",
 *  `read_symbol` → "Read symbol". Keeps a provider's unknown tools
 *  readable instead of showing a raw identifier. */
function humanize(name: string): string {
  const words = name
    .replace(/([a-z0-9])([A-Z])/g, '$1 $2')
    .replace(/[_-]+/g, ' ')
    .trim()
  if (!words) return name
  return words.charAt(0).toUpperCase() + words.slice(1)
}

/** Map tool names to friendly labels. */
export function getToolLabel(toolName: string): string {
  const bare = bareToolName(toolName)
  const mapped = TOOL_LABELS[bare.toLowerCase()]
  if (mapped) return mapped
  switch (bare) {
    case 'TaskCreate':
    case 'TaskUpdate':
    case 'TaskGet':
    case 'TaskList':
      return bare.replace('Task', 'Task ')
    default:
      return humanize(bare)
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
    // Native shells: Claude/Kimi's Bash, Cursor's shell, grok's terminal.
    case 'Bash':
    case 'shell':
    case 'terminal':
    case 'run_terminal_cmd':
    case 'execute_command': {
      const cmd = input.command
      return typeof cmd === 'string' ? cmd : ''
    }
    default:
      return ''
  }
}

/** The model's one-sentence why for a tool call: our exec tools' `reason`
 *  (the cursor parser lifts its `description` into the same key), an
 *  `explanation` arg, or the native shell tools' `description`. Other
 *  tools' `description` fields carry payloads (e.g. a card body), so they
 *  are not reasons. */
export function getToolReason(toolName: string, input?: Record<string, unknown>): string {
  if (!input) return ''
  const bare = bareToolName(toolName)
  const isShell = bare === 'Bash' || bare === 'shell' || bare === 'terminal'
  const raw = input.reason ?? input.explanation ?? (isShell ? input.description : undefined)
  return typeof raw === 'string' ? raw.trim() : ''
}

/** Shorten a file path to just the last 2-3 segments */
export function shortenPath(p: string): string {
  const parts = p.split('/')
  if (parts.length <= 3) return p
  return '.../' + parts.slice(-3).join('/')
}

/** Input keys worth showing when a tool has no purpose-built summary —
 *  what the call is chewing on, most specific first. Path-ish keys are
 *  shortened; the rest are shown verbatim. */
const SUMMARY_KEYS = [
  'path',
  'file_path',
  'filePath',
  'target_file',
  'pattern',
  'query',
  'url',
  'server',
  'name',
] as const
const PATH_KEYS = new Set(['path', 'file_path', 'filePath', 'target_file'])

/** Last-resort summary for a tool nothing else knows about — any provider,
 *  any MCP server. Without it those rows show a label and nothing else. */
function genericSummary(input: Record<string, unknown>): string {
  for (const key of SUMMARY_KEYS) {
    const v = input[key]
    if (typeof v !== 'string' || v === '') continue
    return PATH_KEYS.has(key) ? shortenPath(v) : v
  }
  return ''
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

  // Peckboard MCP tools and the other CLIs' native tools — matched on the
  // bare name so the `mcp__peckboard__` (or any other server's) prefix
  // doesn't matter.
  switch (bareToolName(toolName)) {
    // Cursor's MCP tool-listing call: it looks a server up by tool name or
    // by regex — whichever it used is the interesting half.
    case 'getMcpTools': {
      const tool = (input.toolName ?? input.pattern) as string | undefined
      const server = input.server as string | undefined
      if (tool && server) return `${tool} on ${server}`
      return tool ?? server ?? ''
    }
    case 'read_file': {
      const p = input.path as string | undefined
      if (!p) return ''
      const start = input.start_line as number | undefined
      return start ? `${shortenPath(p)}:${start}` : shortenPath(p)
    }
    // Peckboard's file tools and cursor's own, which all take a `path`.
    case 'write_file':
    case 'edit_file':
    case 'file_outline':
    case 'read':
    case 'write':
    case 'edit':
    case 'delete':
    case 'ls': {
      const p = input.path as string | undefined
      return p ? shortenPath(p) : ''
    }
    case 'read_symbol': {
      const name = input.name as string | undefined
      const p = input.path as string | undefined
      return [name, p ? `in ${shortenPath(p)}` : ''].filter(Boolean).join(' ')
    }
    case 'search_files':
    case 'grep': {
      const q = (input.query ?? input.pattern) as string | undefined
      const scope = (input.path_contains ?? input.path) as string | undefined
      const parts: string[] = []
      if (q) parts.push(`"${q}"`)
      if (scope) parts.push(`in ${shortenPath(scope)}`)
      return parts.join(' ')
    }
    case 'glob': {
      const pattern = input.pattern as string | undefined
      const p = input.path as string | undefined
      return [pattern, p ? `in ${shortenPath(p)}` : ''].filter(Boolean).join(' ')
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
      return genericSummary(input)
  }
}
